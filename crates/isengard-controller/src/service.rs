//! `Controller` gRPC service. `enroll` (Phase 2c, refactored Phase 14 Task 6)
//! and `sync` (Phase 2e) are real.

use std::sync::Arc;

use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{
    AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse, RenewCertRequest,
    RenewCertResponse,
};
use isengard_storage::{Inventory, Journal};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::bus::EventBus;
use crate::ca::Authority;
use crate::enrollment::{EnrollmentService, HostInfo};
use crate::routing::RoutingPusher;

#[derive(Clone)]
pub struct ControllerService {
    pub inventory: Arc<Inventory>,
    pub journal: Arc<Journal>,
    pub bus: Arc<EventBus>,
    pub routing: Arc<RoutingPusher>,
    pub ca: Arc<Authority>,
    pub enrollment: Arc<EnrollmentService>,
}

impl ControllerService {
    pub fn new(
        inventory: Arc<Inventory>,
        journal: Arc<Journal>,
        bus: Arc<EventBus>,
        routing: Arc<RoutingPusher>,
        ca: Arc<Authority>,
        enrollment: Arc<EnrollmentService>,
    ) -> Self {
        Self {
            inventory,
            journal,
            bus,
            routing,
            ca,
            enrollment,
        }
    }

    /// Test constructor: stubs the bus, journal, and routing pusher with
    /// fresh in-memory instances. Caller supplies the inventory + CA +
    /// enrollment service it wants to exercise. Used by Phase 14 task tests
    /// that only care about the enrollment / cert flows.
    pub async fn new_for_test(
        inventory: Arc<Inventory>,
        ca: Arc<Authority>,
        enrollment: Arc<EnrollmentService>,
    ) -> Self {
        let journal = Arc::new(
            Journal::open_in_memory()
                .await
                .expect("journal open_in_memory in test"),
        );
        let bus = Arc::new(EventBus::new());
        let routing = Arc::new(RoutingPusher::new(inventory.clone()));
        Self {
            inventory,
            journal,
            bus,
            routing,
            ca,
            enrollment,
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

        let host_info = HostInfo {
            hostname: req.hostname.clone(),
            os: req.os,
            version: req.version,
        };

        // EnrollmentService.redeem owns: token validation (lookup by hash,
        // expiry/consumed checks), HostId allocation, leaf cert signing,
        // cert persistence, and token consumption. Any failure surfaces as
        // an `Unauthenticated` gRPC status — the most common cause is an
        // unknown / expired / already-consumed token, and we deliberately
        // don't leak which.
        let resp = self
            .enrollment
            .redeem(&req.token, host_info)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, hostname = %req.hostname, "enroll: redeem failed");
                Status::unauthenticated(format!("{e}"))
            })?;

        // Emit agent.enroll event so dashboard subscribers (e.g. the
        // onboarding wizard's listening step) can react in real time.
        let display_name = if req.hostname.is_empty() {
            "host".to_string()
        } else {
            req.hostname.clone()
        };
        let event = isengard_core::Event {
            kind: "agent.enroll".to_string(),
            occurred_at: chrono::Utc::now(),
            host_id: Some(resp.host_id.0),
            summary: format!("{} enrolled", display_name),
            metadata: serde_json::json!({
                "host_id": resp.host_id.to_string(),
                "hostname": req.hostname,
            }),
            ..Default::default()
        };
        crate::persist_and_broadcast(&self.journal, &self.bus, event).await;

        Ok(Response::new(EnrollResponse {
            host_id: resp.host_id.to_bytes().to_vec(),
            agent_cert_pem: resp.agent_cert_pem,
            agent_key_pem: resp.agent_key_pem,
            ca_root_pem: resp.ca_root_pem,
            heartbeat_interval_secs: resp.heartbeat_interval_secs,
        }))
    }

    /// Sign a fresh leaf cert for the calling agent. The new cert + key go back
    /// in the response; the old cert row stays in storage (append-only audit).
    ///
    /// TODO(task-8): once the mTLS interceptor lands, cross-check that the
    /// `host_id` in the request matches the `host_id` extracted from the
    /// caller's client cert. Until then we trust the request body, which is
    /// fine because nothing reaches this handler without already passing
    /// transport-level authentication once the interceptor is wired in.
    async fn renew_cert(
        &self,
        request: Request<RenewCertRequest>,
    ) -> Result<Response<RenewCertResponse>, Status> {
        let req = request.into_inner();
        let host_id = isengard_storage::HostId::from_db_bytes(req.host_id)
            .map_err(|_| Status::invalid_argument("invalid host_id"))?;

        let renewed = self.enrollment.renew(host_id).await.map_err(|e| {
            tracing::warn!(error = %e, host_id = %host_id, "renew_cert: failed");
            Status::internal(format!("{e}"))
        })?;

        Ok(Response::new(RenewCertResponse {
            agent_cert_pem: renewed.agent_cert_pem,
            agent_key_pem: renewed.agent_key_pem,
            expires_at: renewed.expires_at.to_rfc3339(),
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

        // Build outbound channel. Controller uses this to send HeartbeatAcks,
        // ProxyConfig pushes (Phase 8), and Phase 3+ commands.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ControllerMessage, Status>>(16);
        let outbound = ReceiverStream::new(rx);

        // Register the outbound sender with the routing pusher so it can push
        // ProxyConfig at any time (rule changes, label reports, manual edits).
        // Push the current config immediately so a reconnecting agent catches
        // up without waiting for the next rule change.
        self.routing.register_sender(host_id, tx.clone()).await;
        if let Err(e) = self.routing.push_to_host(host_id).await {
            tracing::warn!(
                error = %e,
                agent = %host.hostname,
                "initial proxy_config push failed",
            );
        }

        let inventory = self.inventory.clone();
        let journal = self.journal.clone();
        let bus = self.bus.clone();
        let routing = self.routing.clone();
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

                        if let Err(e) = crate::sync_services::process_heartbeat_services(
                            &inventory,
                            host_id,
                            &hb.services,
                        )
                        .await
                        {
                            tracing::error!(error = %e, agent = %agent_hostname, "process_heartbeat_services failed");
                        }

                        let pending_actions = match crate::pending_actions::collect_pending_actions(
                            &inventory, host_id,
                        )
                        .await
                        {
                            Ok(actions) => actions,
                            Err(e) => {
                                tracing::error!(error = %e, agent = %agent_hostname, "collect_pending_actions failed");
                                vec![]
                            }
                        };

                        let ack = ControllerMessage {
                            payload: Some(
                                isengard_proto::pb::controller_message::Payload::HeartbeatAck(
                                    isengard_proto::pb::HeartbeatAck {
                                        server_time_ms: (server_ts as u64) * 1000,
                                        pending_actions,
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
                    Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsReport(
                        report,
                    )) => {
                        if let Err(e) = routing.ingest_labels(host_id, report).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "labels: ingest failed",
                            );
                        }
                        if let Err(e) = routing.push_to_host(host_id).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "labels: push_to_host after ingest failed",
                            );
                        }
                    }
                    Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsRemoved(
                        ev,
                    )) => {
                        if let Err(e) = routing.ingest_labels_removed(host_id, ev).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "labels: ingest_removed failed",
                            );
                        }
                        if let Err(e) = routing.push_to_host(host_id).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "labels: push_to_host after ingest_removed failed",
                            );
                        }
                    }
                }
            }

            // Stream is over: drop the registered sender so push_to_host
            // becomes a no-op for this host until it reconnects.
            routing.unregister_sender(host_id).await;
            tracing::info!(agent = %agent_hostname, "agent stream closed");
        });

        Ok(Response::new(outbound))
    }
}
