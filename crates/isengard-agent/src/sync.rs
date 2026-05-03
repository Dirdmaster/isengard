//! Long-lived Sync stream from agent to controller.
//!
//! Phase 2e ships the happy path:
//!  - open one bidi stream
//!  - first frame: SyncHello { agent_id }
//!  - then: Heartbeat every `interval_secs` until the stream errors or the
//!    caller's cancellation Notify fires
//!  - read every ControllerMessage from the server side, log at debug
//!
//! Reconnection on drop is Phase 2f.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use isengard_core::Event as CoreEvent;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::{AgentMessage, Event as ProtoEvent, Heartbeat, SyncHello, agent_message};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tracing::{debug, info, instrument, warn};

use crate::Result;
use crate::backoff::Backoff;
use crate::proxy::ProxyState;

/// Open a Sync stream and run the heartbeat loop until the stream errors or
/// `cancel` fires. Returns Ok on graceful cancel; Err on stream error.
///
/// `events_rx` is borrowed (not moved) so that it survives reconnects: events
/// emitted while the stream is down stay queued in the channel until the next
/// stream comes up.
#[instrument(skip(token, cancel, events_rx, proxy_state), fields(agent_id = %agent_id))]
pub async fn run_sync_loop(
    controller_url: String,
    token: String,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
    events_rx: &mut mpsc::Receiver<CoreEvent>,
    proxy_state: ProxyState,
) -> Result<()> {
    let channel = Channel::from_shared(controller_url.clone())
        .with_context(|| format!("invalid controller url {controller_url:?}"))?
        .connect()
        .await
        .with_context(|| format!("connecting to controller at {controller_url}"))?;

    let bearer: MetadataValue<_> = format!("Bearer {token}")
        .parse()
        .context("token contains characters not legal in a Bearer header")?;

    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    // Outbound: Hello, then Heartbeat every interval.
    let (tx, rx) = mpsc::channel::<AgentMessage>(16);
    let outbound = ReceiverStream::new(rx);

    // Send Hello first.
    let hello = AgentMessage {
        payload: Some(isengard_proto::pb::agent_message::Payload::Hello(
            SyncHello {
                agent_id: agent_id.clone(),
            },
        )),
    };
    tx.send(hello)
        .await
        .context("sending Hello to outbound channel")?;
    info!("Sync stream opened, Hello sent");

    // Spawn heartbeat task.
    let hb_tx = tx.clone();
    let interval = Duration::from_secs(u64::from(interval_secs.max(1)));
    let cancel_hb = cancel.clone();
    let mut heartbeat_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the first immediate tick; we want the first heartbeat one
        // interval after Hello, not piggybacked on the connection.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel_hb.notified() => {
                    debug!("heartbeat task cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    let ts_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let snapshots = crate::container_snapshot::list_container_snapshots().await;
                    let stacks = crate::container_snapshot::derive_stacks(&snapshots);
                    let services = crate::container_snapshot::derive_services(&snapshots);
                    let msg = AgentMessage {
                        payload: Some(isengard_proto::pb::agent_message::Payload::Heartbeat(
                            Heartbeat { ts_ms, stacks, services },
                        )),
                    };
                    if hb_tx.send(msg).await.is_err() {
                        debug!("outbound channel closed, exiting heartbeat task");
                        break;
                    }
                }
            }
        }
    });

    // Open the bidi stream.
    let response = client
        .sync(Request::new(outbound))
        .await
        .context("Sync RPC failed")?;
    let mut inbound = response.into_inner();

    // Read inbound messages (HeartbeatAck, ProxyConfig, future Command/ConfigUpdate).
    let read_proxy_state = proxy_state.clone();
    let mut read_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = inbound.message().await {
            match msg.payload {
                Some(isengard_proto::pb::controller_message::Payload::HeartbeatAck(ack)) => {
                    debug!(server_time_ms = ack.server_time_ms, "HeartbeatAck");
                    for action in ack.pending_actions {
                        // v1: log receipt. Real execution (force-update via updater
                        // plugin signal, decommission, etc.) lands in v1.x. The
                        // controller marks the action delivered as soon as it
                        // includes it in the ack — see Task 5.
                        info!(
                            action_id = action.id,
                            kind = %action.kind,
                            payload = %action.payload_json,
                            "received pending action (execution deferred to v1.x)"
                        );
                    }
                }
                Some(isengard_proto::pb::controller_message::Payload::ProxyConfig(cfg)) => {
                    if let Err(e) = crate::proxy::apply_config(&read_proxy_state, cfg).await {
                        warn!(error = %e, "proxy: apply_config failed");
                    }
                }
                _ => {
                    warn!(?msg.payload, "unexpected ControllerMessage payload");
                }
            }
        }
        info!("inbound stream closed");
    });

    // Wait for cancel OR for either spawned task to end OR for an outbound
    // event to drain. If a task ends before cancel fires, that means the
    // stream broke (controller died, network dropped, etc.) — we return Err
    // so the outer reconnect loop retries. Outbound events are forwarded as
    // AgentMessage::Event frames; we loop until one of the terminal arms
    // fires.
    //
    // `read_consumed` / `hb_consumed` track which handle (if any) was polled
    // to completion by the select. We must NOT await a JoinHandle a second
    // time after select consumed its output — tokio panics with
    // "JoinHandle polled after completion".
    let mut read_consumed = false;
    let mut hb_consumed = false;
    let result: Result<()> = loop {
        tokio::select! {
            _ = cancel.notified() => {
                info!("cancel received, shutting down sync");
                break Ok(());
            }
            res = &mut read_task => {
                read_consumed = true;
                tracing::warn!(?res, "inbound stream task ended before cancel");
                break Err(anyhow::anyhow!("inbound stream ended (controller likely went away)"));
            }
            res = &mut heartbeat_task => {
                hb_consumed = true;
                tracing::warn!(?res, "heartbeat task ended before cancel");
                break Err(anyhow::anyhow!("heartbeat task ended (outbound channel closed)"));
            }
            maybe_ev = events_rx.recv() => {
                let Some(core_ev) = maybe_ev else {
                    // Receiver closed — agent shutting down. Treat like cancel.
                    info!("events channel closed, shutting down sync");
                    break Ok(());
                };
                let proto_ev: ProtoEvent = core_ev.into();
                let msg = AgentMessage {
                    payload: Some(agent_message::Payload::Event(proto_ev)),
                };
                if let Err(e) = tx.send(msg).await {
                    warn!(error = %e, "failed to send event over sync stream");
                    break Err(anyhow::anyhow!("outbound channel closed while sending event"));
                }
            }
        }
    };

    // Cleanup: drop tx to signal server-side EOF, then abort + reap any task
    // we didn't already drain via select.
    drop(tx);
    if !read_consumed {
        read_task.abort();
        let _ = read_task.await;
    }
    if !hb_consumed {
        heartbeat_task.abort();
        let _ = heartbeat_task.await;
    }

    result
}

/// Run the Sync loop with automatic reconnection on stream failure. Returns
/// Ok only when `cancel` fires (graceful shutdown). On stream error, sleeps
/// per the backoff policy and retries.
///
/// Backoff resets to base if the previous attempt's stream stayed open ≥ 60s
/// (proves the connection was healthy).
#[instrument(skip(token, cancel, events_rx, proxy_state), fields(agent_id = %agent_id))]
pub async fn run_sync_with_reconnect(
    controller_url: String,
    token: String,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
    events_rx: &mut mpsc::Receiver<CoreEvent>,
    proxy_state: ProxyState,
) -> Result<()> {
    let mut backoff = Backoff::new();
    const STABLE_THRESHOLD: Duration = Duration::from_secs(60);

    loop {
        let delay = backoff.next_delay();
        if delay > Duration::ZERO {
            info!(
                attempt = backoff.attempt(),
                delay_ms = delay.as_millis() as u64,
                "waiting before sync reconnect"
            );
            tokio::select! {
                _ = cancel.notified() => {
                    info!("cancel during backoff, exiting");
                    return Ok(());
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }

        let attempt_started = Instant::now();
        let result = run_sync_loop(
            controller_url.clone(),
            token.clone(),
            agent_id.clone(),
            interval_secs,
            cancel.clone(),
            events_rx,
            proxy_state.clone(),
        )
        .await;

        match result {
            Ok(()) => {
                // Graceful cancel — exit.
                info!("sync loop exited cleanly via cancel");
                return Ok(());
            }
            Err(e) => {
                let elapsed = attempt_started.elapsed();
                if elapsed >= STABLE_THRESHOLD {
                    info!(
                        elapsed_secs = elapsed.as_secs(),
                        "stream was stable; resetting backoff"
                    );
                    backoff.reset();
                }
                tracing::warn!(error = %e, attempt = backoff.attempt(), "sync stream error, will retry");
                // Loop continues; cancel-during-backoff handled at the top.
            }
        }
    }
}
