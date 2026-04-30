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
use std::time::Duration;

use anyhow::Context;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::{AgentMessage, Heartbeat, SyncHello};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use tracing::{debug, info, instrument, warn};

use crate::Result;

/// Open a Sync stream and run the heartbeat loop until the stream errors or
/// `cancel` fires. Returns Ok on graceful cancel; Err on stream error.
#[instrument(skip(token, cancel), fields(agent_id = %agent_id))]
pub async fn run_sync_loop(
    controller_url: String,
    token: String,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
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
        payload: Some(isengard_proto::pb::agent_message::Payload::Hello(SyncHello {
            agent_id: agent_id.clone(),
        })),
    };
    tx.send(hello).await.context("sending Hello to outbound channel")?;
    info!("Sync stream opened, Hello sent");

    // Spawn heartbeat task.
    let hb_tx = tx.clone();
    let interval = Duration::from_secs(u64::from(interval_secs.max(1)));
    let cancel_hb = cancel.clone();
    let heartbeat_task = tokio::spawn(async move {
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
                    let msg = AgentMessage {
                        payload: Some(isengard_proto::pb::agent_message::Payload::Heartbeat(
                            Heartbeat { ts_ms },
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

    // Read inbound messages (HeartbeatAck, future Command/ConfigUpdate).
    let read_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = inbound.message().await {
            match msg.payload {
                Some(isengard_proto::pb::controller_message::Payload::HeartbeatAck(ack)) => {
                    debug!(server_time_ms = ack.server_time_ms, "HeartbeatAck");
                }
                _ => {
                    warn!(?msg.payload, "unexpected ControllerMessage payload");
                }
            }
        }
        info!("inbound stream closed");
    });

    // Wait for cancellation.
    cancel.notified().await;
    info!("cancel received, shutting down sync");
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    drop(tx); // close outbound; server sees EOF
    let _ = read_task.await;

    Ok(())
}
