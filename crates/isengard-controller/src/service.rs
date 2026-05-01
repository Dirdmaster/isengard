//! `Controller` gRPC service. `enroll` (Phase 2c) and `sync` (Phase 2e) are real.

use std::sync::Arc;

use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse};
use isengard_storage::{Inventory, Journal};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::bus::EventBus;

#[derive(Clone)]
pub struct ControllerService {
    inventory: Arc<Inventory>,
    journal: Arc<Journal>,
    bus: Arc<EventBus>,
}

impl ControllerService {
    pub fn new(inventory: Arc<Inventory>, journal: Arc<Journal>, bus: Arc<EventBus>) -> Self {
        Self {
            inventory,
            journal,
            bus,
        }
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    type SyncStream = ReceiverStream<Result<ControllerMessage, Status>>;

    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let req = request.into_inner();

        // Convert proto request → storage request.
        let enroll_req = isengard_storage::EnrollHost {
            fingerprint: req.fingerprint.clone(),
            hostname: req.hostname.clone(),
            os: req.os,
            arch: req.arch,
            agent_version: req.agent_version,
            docker_version: req.docker_version,
        };

        let id = match self.inventory.enroll_host(enroll_req).await {
            Ok(id) => id,
            Err(isengard_storage::Error::Db(sqlx_err)) => {
                // UNIQUE violation on fingerprint maps to AlreadyExists.
                let is_unique_violation = sqlx_err
                    .as_database_error()
                    .map(|db_err| {
                        db_err.message().contains("UNIQUE")
                            || db_err.code().as_deref() == Some("2067")
                            || db_err.code().as_deref() == Some("19")
                    })
                    .unwrap_or(false);
                if is_unique_violation {
                    return Err(Status::already_exists(format!(
                        "host with fingerprint {:?} is already enrolled",
                        req.fingerprint,
                    )));
                }
                tracing::error!(error = %sqlx_err, "Enroll: db error");
                return Err(Status::internal("database error"));
            }
            Err(other) => {
                tracing::error!(error = %other, "Enroll: unexpected error");
                return Err(Status::internal("storage error"));
            }
        };

        let server_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Ok(Response::new(EnrollResponse {
            agent_id: id.to_string(),
            heartbeat_interval_secs: 10,
            server_time_ms,
        }))
    }

    async fn sync(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        let mut inbound = request.into_inner();

        // Read first frame: must be SyncHello with a valid agent_id.
        let hello = inbound
            .message()
            .await
            .map_err(|e| Status::aborted(format!("reading hello: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before SyncHello"))?;

        let agent_id_str = match hello.payload {
            Some(isengard_proto::pb::agent_message::Payload::Hello(h)) => h.agent_id,
            _ => {
                return Err(Status::invalid_argument("first frame must be SyncHello"));
            }
        };

        // Parse the ULID and look up the host.
        let agent_ulid = ulid::Ulid::from_string(&agent_id_str)
            .map_err(|e| Status::unauthenticated(format!("invalid agent_id: {e}")))?;
        let host_id = isengard_storage::HostId(agent_ulid);

        let host = self
            .inventory
            .get_host(host_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Sync: db error during host lookup");
                Status::internal("database error")
            })?
            .ok_or_else(|| Status::unauthenticated("agent_id not in inventory; re-enroll"))?;

        tracing::info!(agent = %host.hostname, "agent connected for sync");

        // Build outbound channel. Controller uses this to send HeartbeatAcks
        // (and Phase 3+ commands).
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ControllerMessage, Status>>(16);
        let outbound = ReceiverStream::new(rx);

        let inventory = self.inventory.clone();
        let journal = self.journal.clone();
        let bus = self.bus.clone();
        let agent_hostname = host.hostname.clone();

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(isengard_proto::pb::agent_message::Payload::Heartbeat(hb)) => {
                        // Update last_seen_at to the controller's clock (not the
                        // agent's — keeps the inventory's notion of "recent" tied
                        // to a single clock).
                        let server_ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);

                        if let Err(e) = inventory.touch_host(host_id, server_ts).await {
                            tracing::error!(error = %e, agent = %agent_hostname, "touch_host failed");
                        }

                        if let Err(e) = crate::sync_stacks::process_heartbeat_stacks(
                            &inventory, host_id, &hb.stacks,
                        )
                        .await
                        {
                            tracing::error!(error = %e, agent = %agent_hostname, "process_heartbeat_stacks failed");
                        }

                        let ack = ControllerMessage {
                            payload: Some(
                                isengard_proto::pb::controller_message::Payload::HeartbeatAck(
                                    isengard_proto::pb::HeartbeatAck {
                                        server_time_ms: (server_ts as u64) * 1000,
                                        pending_actions: vec![],
                                    },
                                ),
                            ),
                        };
                        if tx.send(Ok(ack)).await.is_err() {
                            // Client closed the receive side; stream is over.
                            tracing::debug!(agent = %agent_hostname, "client receiver closed");
                            break;
                        }

                        let _ = hb.ts_ms; // agent clock for future drift logging
                    }
                    Some(isengard_proto::pb::agent_message::Payload::Hello(_)) => {
                        // Hello as second-or-later frame is invalid; ignore + log.
                        tracing::warn!(agent = %agent_hostname, "received Hello after first frame");
                    }
                    Some(isengard_proto::pb::agent_message::Payload::Event(proto_ev)) => {
                        // host_id is known from the SyncHello flow that opened
                        // this stream. Convert proto → core, then persist + bus.
                        let core_ev: isengard_core::Event = match proto_ev.try_into() {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(
                                    agent = %agent_hostname,
                                    error = %e,
                                    "discarding malformed event",
                                );
                                continue;
                            }
                        };
                        let mut core_ev = core_ev;
                        core_ev.host_id = Some(host_id.into());
                        crate::persist_and_broadcast(&journal, &bus, core_ev).await;
                    }
                    None => {
                        tracing::debug!(agent = %agent_hostname, "empty payload, skipping");
                    }
                }
            }

            tracing::info!(agent = %agent_hostname, "agent stream closed");
        });

        Ok(Response::new(outbound))
    }
}
