//! WebSocket handler for live event streaming + logs streaming.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use isengard_controller::ControllerHandles;
use isengard_proto::pb::{
    ControllerMessage, StartLogStream, StopLogStream, controller_message, log_chunk,
};
use isengard_storage::{HostId, StackId};
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::dto::LiveEventDto;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // Pong/Error reserved for future client-message handling (5b Task 7+)
/// WsFrame.
pub enum WsFrame {
    /// Sent once on connect.
    Welcome {
        /// Number of events recorded so far today.
        event_count_today: usize,
    },
    /// Live event frame broadcast from the bus.
    Event {
        /// Decoded event payload.
        event: LiveEventDto,
    },
    /// Pong frame reserved for future client-driven ping/pong.
    Pong,
    /// Server-emitted error frame.
    Error {
        /// Human-readable error message.
        message: String,
    },
}

/// WebSocket handler for handler.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, handles))
}

/// Handler for socket.
async fn handle_socket(mut socket: WebSocket, handles: Arc<ControllerHandles>) {
    // Send welcome frame.
    let count_today = handles
        .journal
        .list_recent(500)
        .await
        .map(|r| r.len())
        .unwrap_or(0);
    let welcome = WsFrame::Welcome {
        event_count_today: count_today,
    };
    if let Err(e) = send_frame(&mut socket, &welcome).await {
        debug!(error = %e, "failed to send welcome");
        return;
    }

    let mut rx = handles.bus.subscribe();
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));
    keepalive.tick().await; // skip first immediate tick

    loop {
        tokio::select! {
            event_result = rx.recv() => {
                match event_result {
                    Ok(event) => {
                        let dto = LiveEventDto::from(&event);
                        let frame = WsFrame::Event { event: dto };
                        if let Err(e) = send_frame(&mut socket, &frame).await {
                            debug!(error = %e, "ws send failed; closing");
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(skipped = n, "dashboard ws lagged");
                    }
                    Err(RecvError::Closed) => {
                        debug!("event bus closed");
                        break;
                    }
                }
            }
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    debug!("ws ping failed; closing");
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("ws closed by client");
                        break;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Text(_))) => {
                        // v1 doesn't accept client text frames; ignore.
                    }
                    Some(Err(e)) => {
                        debug!(error = %e, "ws recv error");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// `send_frame`.
async fn send_frame(socket: &mut WebSocket, frame: &WsFrame) -> anyhow::Result<()> {
    let json = serde_json::to_string(frame)?;
    socket.send(Message::Text(json.into())).await?;
    Ok(())
}

/// WebSocket entry point for service logs.
///
/// Path: `GET /api/v1/services/:stack_id/:service_name/logs/ws`.
///
/// Resolves the stack + primary service + replicas on other hosts, then
/// fans out a `StartLogStream` ControllerMessage to each host's Sync sender.
/// Inbound `LogChunk` frames are routed via the controller's `LogFanout`
/// into a per-subscription mpsc whose receiver this handler drains.
pub async fn handle_service_logs(
    ws: WebSocketUpgrade,
    State(handles): State<Arc<ControllerHandles>>,
    Path((stack_id, service_name)): Path<(i64, String)>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_service_logs_socket(socket, handles, stack_id, service_name))
}

/// Handler for service logs socket.
async fn handle_service_logs_socket(
    mut socket: WebSocket,
    handles: Arc<ControllerHandles>,
    stack_id: i64,
    service_name: String,
) {
    // Resolve the stack + primary service.
    let stack = match handles.inventory.get_stack(StackId(stack_id)).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = send_err_text(&mut socket, "stack not found").await;
            return;
        }
        Err(e) => {
            let _ = send_err_text(&mut socket, &format!("get_stack: {e}")).await;
            return;
        }
    };
    let primary = match handles
        .inventory
        .get_service_by_name(stack.host_id, Some(stack.id), &service_name)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = send_err_text(&mut socket, "service not found").await;
            return;
        }
        Err(e) => {
            let _ = send_err_text(&mut socket, &format!("get_service_by_name: {e}")).await;
            return;
        }
    };

    // Discover replicas: services with the same name + stack name on other hosts.
    let mut hosts: Vec<HostId> = vec![primary.host_id];
    let stacks = handles
        .inventory
        .list_stacks(None)
        .await
        .unwrap_or_default();
    for s in stacks {
        if s.host_id == primary.host_id || s.name != stack.name {
            continue;
        }
        if let Ok(Some(_)) = handles
            .inventory
            .get_service_by_name(s.host_id, Some(s.id), &service_name)
            .await
        {
            hosts.push(s.host_id);
        }
    }

    let subscription_id = ulid::Ulid::new().to_string();
    let mut rx = handles.log_fanout.register(subscription_id.clone()).await;

    // Send StartLogStream to each host. Track which hosts accepted so we can
    // emit `unavailable` for the rest up-front.
    let tail_default: u32 = 200;
    let mut accepted: Vec<HostId> = Vec::new();
    for host in &hosts {
        let msg = ControllerMessage {
            payload: Some(controller_message::Payload::StartLogStream(
                StartLogStream {
                    subscription_id: subscription_id.clone(),
                    container_name: service_name.clone(),
                    tail: tail_default,
                    follow: true,
                },
            )),
        };
        if handles.routing.send_to_host(*host, msg).await {
            accepted.push(*host);
        } else {
            let frame = json!({
                "type": "unavailable",
                "host": short_host(*host),
                "reason": "agent_disconnected",
            });
            let _ = socket.send(Message::Text(frame.to_string().into())).await;
        }
    }

    if accepted.is_empty() {
        let frame = json!({"type": "closed", "reason": "no_agents"});
        let _ = socket.send(Message::Text(frame.to_string().into())).await;
        handles.log_fanout.unregister(&subscription_id).await;
        return;
    }

    // Drain loop. 15s heartbeat ping; 30s no-traffic close.
    let mut last_traffic = std::time::Instant::now();
    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    keepalive.tick().await;

    loop {
        tokio::select! {
            chunk = rx.recv() => {
                let Some(chunk) = chunk else { break; };
                last_traffic = std::time::Instant::now();
                let host_short = chunk_host_short(&chunk, &accepted);
                let frame = chunk_to_frame(&chunk, &host_short);
                if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = keepalive.tick() => {
                if last_traffic.elapsed() > Duration::from_secs(30) {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Text(t))) => {
                        // Client -> server control frames: pause/resume/seek_tail/set_level.
                        // For this slice we route `seek_tail` as a re-Start with the new tail
                        // and ignore the rest; the agent's tail task is idempotent on
                        // subscription_id.
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(t.as_str()) {
                            if value.get("type").and_then(|v| v.as_str()) == Some("seek_tail") {
                                let tail = value
                                    .get("tail")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(u64::from(tail_default))
                                    .min(5000) as u32;
                                for host in &accepted {
                                    let restart = ControllerMessage {
                                        payload: Some(controller_message::Payload::StartLogStream(
                                            StartLogStream {
                                                subscription_id: subscription_id.clone(),
                                                container_name: service_name.clone(),
                                                tail,
                                                follow: true,
                                            },
                                        )),
                                    };
                                    let _ = handles.routing.send_to_host(*host, restart).await;
                                }
                            }
                        }
                    }
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    // Cleanup: tell each agent to stop tailing, then unregister.
    for host in &accepted {
        let stop = ControllerMessage {
            payload: Some(controller_message::Payload::StopLogStream(StopLogStream {
                subscription_id: subscription_id.clone(),
            })),
        };
        let _ = handles.routing.send_to_host(*host, stop).await;
    }
    handles.log_fanout.unregister(&subscription_id).await;
}

/// `short_host`.
fn short_host(h: HostId) -> String {
    let s = ulid::Ulid::from(h).to_string();
    s[..8.min(s.len())].to_string()
}

/// `chunk_host_short`.
fn chunk_host_short(chunk: &isengard_proto::pb::LogChunk, accepted: &[HostId]) -> String {
    // We don't carry host id back from the agent today (the agent's
    // subscription is keyed by id; the controller knows which host the
    // chunk came from from the Sync stream context, not the chunk body).
    // For the WebSocket frame we surface an "agg" prefix when there's
    // exactly one host and the host short id when there are several. The
    // routing of multi-host frames happens at controller-stream ingress
    // (see service.rs), where each host's Sync handler tags chunks with
    // the host_id on the way in. This helper is the rendering side; it
    // currently flattens to "agg" until that wire change lands. Callers
    // already see ordering preserved.
    let _ = chunk;
    if accepted.len() == 1 {
        short_host(accepted[0])
    } else {
        "agg".into()
    }
}

/// `chunk_to_frame`.
fn chunk_to_frame(chunk: &isengard_proto::pb::LogChunk, host_short: &str) -> serde_json::Value {
    match log_chunk::Kind::try_from(chunk.kind).unwrap_or(log_chunk::Kind::Unspecified) {
        log_chunk::Kind::Backfill => json!({
            "type": "backfill",
            "host": host_short,
            "lines": [{
                "ts": chunk.occurred_at,
                "stream": chunk.stream,
                "msg": chunk.line,
            }],
        }),
        log_chunk::Kind::Line => json!({
            "type": "line",
            "host": host_short,
            "ts": chunk.occurred_at,
            "stream": chunk.stream,
            "msg": chunk.line,
        }),
        log_chunk::Kind::Dropped => json!({
            "type": "dropped",
            "host": host_short,
            "count": chunk.dropped,
            "reason": chunk.reason,
        }),
        log_chunk::Kind::Unavailable => json!({
            "type": "unavailable",
            "host": host_short,
            "reason": chunk.reason,
        }),
        log_chunk::Kind::Unspecified => json!({
            "type": "line",
            "host": host_short,
            "ts": "",
            "stream": "",
            "msg": "",
        }),
    }
}

/// `send_err_text`.
async fn send_err_text(socket: &mut WebSocket, message: &str) -> Result<(), axum::Error> {
    let frame = json!({"type": "unavailable", "reason": message});
    socket.send(Message::Text(frame.to_string().into())).await
}
