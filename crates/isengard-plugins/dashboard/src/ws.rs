//! WebSocket handler for live event streaming.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use isengard_controller::ControllerHandles;
use isengard_storage::ServiceId;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::dto::LiveEventDto;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // Pong/Error reserved for future client-message handling (5b Task 7+)
pub enum WsFrame {
    Welcome { event_count_today: usize },
    Event { event: LiveEventDto },
    Pong,
    Error { message: String },
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, handles))
}

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

async fn send_frame(socket: &mut WebSocket, frame: &WsFrame) -> anyhow::Result<()> {
    let json = serde_json::to_string(frame)?;
    socket.send(Message::Text(json.into())).await?;
    Ok(())
}

pub async fn handle_logs(
    ws: WebSocketUpgrade,
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_logs_socket(socket, handles, ServiceId(id)))
}

async fn handle_logs_socket(
    mut socket: WebSocket,
    handles: Arc<ControllerHandles>,
    service_id: ServiceId,
) {
    // Resolve the service. Surface "not found" as a frame, then close.
    let svc = match handles.inventory.get_service(service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = send_err(&mut socket, "service not found").await;
            return;
        }
        Err(e) => {
            let _ = send_err(&mut socket, &format!("get_service: {e}")).await;
            return;
        }
    };

    // v1 placeholder: real log streaming requires a controller→agent
    // command channel, which v1's bidirectional Sync stream doesn't yet
    // support. Land the WS surface so the frontend can be built; v1.x
    // wires the actual `docker logs -f` proxy via an extended Sync stream.
    let info = serde_json::json!({
        "type": "info",
        "service": svc.name,
        "host_id": ulid::Ulid::from(svc.host_id).to_string(),
        "message": "Live log streaming will land in v1.x. The controller→agent command channel needed to proxy `docker logs -f` is not yet implemented."
    });

    let _ = socket.send(Message::Text(info.to_string().into())).await;

    // Hold the connection open briefly so the client can read the message,
    // then close. Browser closes are fine too: we honor them via select.
    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    keepalive.tick().await;

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

async fn send_err(socket: &mut WebSocket, message: &str) -> Result<(), axum::Error> {
    let frame = serde_json::json!({"type": "error", "error": message});
    socket.send(Message::Text(frame.to_string().into())).await
}
