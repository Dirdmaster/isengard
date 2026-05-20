//! `Controller` gRPC service. `enroll` (refactored Task 6)
//! and `sync` are real.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{
    AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse, FetchSecretRequest,
    FetchSecretResponse, GetCaPemRequest, GetCaPemResponse, GetSshCaRequest, GetSshCaResponse,
    RenewCertRequest, RenewCertResponse,
};
use isengard_storage::{Inventory, Journal};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::bus::EventBus;
use crate::ca::Authority;
use crate::compose_broker::ComposeBroker;
use crate::enrollment::{EnrollmentService, HostInfo};
use crate::hook_ingest::HookLabelIngest;
use crate::log_fanout::LogFanout;
use crate::policy_ingest::PolicyLabelIngest;
use crate::revocation::RevocationSet;
use crate::routing::RoutingPusher;
use crate::secrets::{SecretsError, SecretsStore};

#[derive(Clone)]
pub struct ControllerService {
    pub inventory: Arc<Inventory>,
    pub journal: Arc<Journal>,
    pub bus: Arc<EventBus>,
    pub routing: Arc<RoutingPusher>,
    /// Container-scope policy ingest from `isengard.policy.*`
    /// labels.
    pub policy_ingest: Arc<PolicyLabelIngest>,
    /// Container-scope lifecycle-hook ingest from
    /// `isengard.hooks.*` labels.
    pub hook_ingest: Arc<HookLabelIngest>,
    pub ca: Arc<Authority>,
    pub enrollment: Arc<EnrollmentService>,
    /// In-memory revocation set the auth interceptor reads on every
    /// RPC. Carried on the service so future handlers (e.g. an admin RPC for
    /// `revoke_agent`) can mutate it without re-fetching from inventory.
    pub revocation: RevocationSet,
    /// Log subscription registry. Inbound `AgentMessage::LogChunk`
    /// frames on the Sync stream are routed through this fanout to the
    /// dashboard's WebSocket tasks.
    pub log_fanout: Arc<LogFanout>,
    /// v0.3d: resolves pending `WriteCompose` requests when the agent's
    /// matching `WriteComposeAck` arrives on the Sync stream.
    pub compose_broker: Arc<ComposeBroker>,
    /// v0.3.6: managed-secrets store. The `FetchSecret` RPC reads
    /// through this; the auth interceptor already gated the connection
    /// behind a valid client cert.
    pub secrets: Arc<SecretsStore>,
    /// Placement scheduler. Optional so the `new_for_test`
    /// constructor doesn't need to fabricate one. The Sync handler calls
    /// `on_heartbeat_labels` here; `enroll` calls `on_host_enroll`;
    /// `disconnect_monitor.rs` calls `on_host_disconnect_long`.
    pub scheduler: Option<Arc<crate::scheduler::Scheduler>>,
    /// SSH user-certificate authority. The `GetSshCa` RPC returns
    /// `public_key_openssh()` so agents install it as a
    /// `TrustedUserCAKeys` drop-in. Later phases use `sign_user_cert`
    /// to mint short-lived operator certs via the dashboard.
    pub ssh_ca: Arc<crate::ssh_ca::SshAuthority>,
}

impl ControllerService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inventory: Arc<Inventory>,
        journal: Arc<Journal>,
        bus: Arc<EventBus>,
        routing: Arc<RoutingPusher>,
        policy_ingest: Arc<PolicyLabelIngest>,
        hook_ingest: Arc<HookLabelIngest>,
        ca: Arc<Authority>,
        enrollment: Arc<EnrollmentService>,
        revocation: RevocationSet,
        log_fanout: Arc<LogFanout>,
        compose_broker: Arc<ComposeBroker>,
        secrets: Arc<SecretsStore>,
        scheduler: Option<Arc<crate::scheduler::Scheduler>>,
        ssh_ca: Arc<crate::ssh_ca::SshAuthority>,
    ) -> Self {
        Self {
            inventory,
            journal,
            bus,
            routing,
            policy_ingest,
            hook_ingest,
            ca,
            enrollment,
            revocation,
            log_fanout,
            compose_broker,
            secrets,
            scheduler,
            ssh_ca,
        }
    }

    /// Test constructor: stubs the bus, journal, and routing pusher with
    /// fresh in-memory instances. Caller supplies the inventory + CA +
    /// enrollment service it wants to exercise. Used by task tests
    /// that only care about the enrollment / cert flows.
    pub async fn new_for_test(
        inventory: Arc<Inventory>,
        ca: Arc<Authority>,
        enrollment: Arc<EnrollmentService>,
        revocation: RevocationSet,
    ) -> Self {
        let journal = Arc::new(
            Journal::open_in_memory()
                .await
                .expect("journal open_in_memory in test"),
        );
        let bus = Arc::new(EventBus::new());
        let routing = Arc::new(RoutingPusher::new(inventory.clone()));
        let policy_ingest = Arc::new(PolicyLabelIngest::new(inventory.clone()));
        let hook_ingest = Arc::new(HookLabelIngest::new(inventory.clone()));
        let log_fanout = LogFanout::new();
        let compose_broker = Arc::new(ComposeBroker::new());
        // Tests get a locked (no-master-key) store: any secrets RPC
        // will surface MasterKeyMissing, which lets exercise of error
        // paths without writing key files in the test process.
        let secrets = Arc::new(SecretsStore::new_locked(inventory.clone()));
        let ssh_ca = Arc::new(crate::ssh_ca::SshAuthority::for_tests().expect("ssh_ca for_tests"));
        Self {
            inventory,
            journal,
            bus,
            routing,
            policy_ingest,
            hook_ingest,
            ca,
            enrollment,
            revocation,
            log_fanout,
            compose_broker,
            secrets,
            // Tests stay scheduler-less; the placement scheduler is only
            // wired in via `run_controller`.
            scheduler: None,
            ssh_ca,
        }
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    type SyncStream = ReceiverStream<Result<ControllerMessage, Status>>;

    async fn get_ca_pem(
        &self,
        _request: Request<GetCaPemRequest>,
    ) -> Result<Response<GetCaPemResponse>, Status> {
        // Unauthenticated: the response is the controller's public CA
        // certificate. The agent has no client cert to present here.
        // Listed in `auth::PUBLIC_METHODS` so the interceptor lets the
        // call through without an mTLS handshake.
        let pem = self.ca.root_cert_pem().as_bytes().to_vec();
        Ok(Response::new(GetCaPemResponse { pem }))
    }

    async fn get_ssh_ca(
        &self,
        _request: Request<GetSshCaRequest>,
    ) -> Result<Response<GetSshCaResponse>, Status> {
        // Unauthenticated: the response is the controller's SSH user-
        // cert authority public key. Agents drop it into
        // `TrustedUserCAKeys` right after mTLS enrollment.
        let pubkey = self.ssh_ca.public_key_openssh().to_vec();
        Ok(Response::new(GetSshCaResponse { pubkey }))
    }

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

        // Notify the scheduler the host enrolled. The
        // trigger marks the host Healthy and re-evaluates every
        // Pending placement (operator decision #3: auto-place onto
        // the new host when it satisfies a `where:` selector).
        if let Some(sched) = self.scheduler.as_ref() {
            sched.on_host_enroll(resp.host_id).await;
        }

        Ok(Response::new(EnrollResponse {
            host_id: resp.host_id.to_bytes().to_vec(),
            agent_cert_pem: resp.agent_cert_pem,
            agent_key_pem: resp.agent_key_pem,
            ca_root_pem: resp.ca_root_pem,
            heartbeat_interval_secs: resp.heartbeat_interval_secs,
        }))
    }

    /// Sign a fresh leaf cert for the calling agent. The host_id is read
    /// authoritatively from the client cert's CN (set by `ca::sign_agent_leaf`
    /// to `host_id.to_string()`); the request body carries no fields. This
    /// closes the Bl-1 horizontal-escalation hole where agent A could request
    /// a cert minted for agent B by passing B's host_id in the request body.
    async fn renew_cert(
        &self,
        request: Request<RenewCertRequest>,
    ) -> Result<Response<RenewCertResponse>, Status> {
        let host_id = host_id_from_peer_cert(&request)?;
        let _ = request.into_inner(); // body has no fields after Bl-1 fix

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

    /// v0.3.6: agent fetches a managed secret on container start. The
    /// CertAuthInterceptor already gated the connection: any caller
    /// reaching this handler has a valid, non-revoked agent cert. We
    /// derive `host_id` from the cert CN so the request body can't lie
    /// about who's asking, then return the decrypted plaintext.
    ///
    /// Logs include the agent host_id and secret name. The secret VALUE
    /// is never logged at any level.
    async fn fetch_secret(
        &self,
        request: Request<FetchSecretRequest>,
    ) -> Result<Response<FetchSecretResponse>, Status> {
        let host_id = host_id_from_peer_cert(&request)?;
        let req = request.into_inner();
        let name = req.name;

        // Validate the name shape up front: same rules the storage DAO
        // applies. Catching it here yields a cleaner error than a deeper
        // sqlx round-trip would.
        if let Err(e) = isengard_storage::validate_secret_name(&name) {
            tracing::warn!(host_id = %host_id, error = %e, "fetch_secret: invalid name");
            return Err(Status::invalid_argument(format!("invalid name: {e}")));
        }

        // Look up metadata first so the response can carry updated_at
        // even if the value is empty bytes.
        let meta = match self.secrets.meta(&name).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                tracing::info!(host_id = %host_id, name = %name, "fetch_secret: not found");
                return Err(Status::not_found(format!("secret {name:?} not found")));
            }
            Err(e) => {
                tracing::warn!(host_id = %host_id, name = %name, error = %e, "fetch_secret: meta lookup failed");
                return Err(Status::internal("secret store unavailable"));
            }
        };

        let value = match self.secrets.fetch(&name).await {
            Ok(v) => v,
            Err(SecretsError::MasterKeyMissing) => {
                tracing::error!(
                    host_id = %host_id,
                    name = %name,
                    "fetch_secret: master key not loaded; agent fetch refused",
                );
                return Err(Status::failed_precondition(
                    "controller secrets store is locked: master key file not readable",
                ));
            }
            Err(SecretsError::NotFound(_)) => {
                tracing::info!(host_id = %host_id, name = %name, "fetch_secret: not found at fetch time");
                return Err(Status::not_found(format!("secret {name:?} not found")));
            }
            Err(e) => {
                tracing::warn!(host_id = %host_id, name = %name, error = %e, "fetch_secret: decrypt failed");
                return Err(Status::internal("decrypt failed"));
            }
        };

        // Log the access; value bytes are explicitly excluded.
        tracing::info!(
            host_id = %host_id,
            name = %name,
            bytes = value.len(),
            "fetch_secret: served",
        );

        Ok(Response::new(FetchSecretResponse {
            value,
            updated_at: meta.updated_at.to_rfc3339(),
        }))
    }

    async fn sync(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        // Bl-2 fix: derive the authoritative host_id from the caller's client
        // cert CN (set by `ca::sign_agent_leaf` to `host_id.to_string()`).
        // Any value in SyncHello.agent_id is logged for debugging but
        // explicitly NOT trusted — previously, an agent with cert A could
        // attribute heartbeats / container reports / label reports to agent
        // B by sending B's id in the Hello payload.
        let host_id = host_id_from_peer_cert(&request)?;
        // v0.3b: capture the agent's remote address so the controller-side
        // DNS resolver can map `<host>.<zone>` -> agent IP. Best-effort: if
        // tonic can't surface the peer address (e.g. unix socket transport in
        // tests) we just skip the update.
        let remote_ip = request.remote_addr().map(|a| a.ip().to_string());
        let mut inbound = request.into_inner();

        // Read first frame: must be SyncHello (the agent_id field is logged
        // but ignored — see above).
        let hello = inbound
            .message()
            .await
            .map_err(|e| Status::aborted(format!("reading hello: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before SyncHello"))?;

        let claimed_agent_id = match hello.payload {
            Some(isengard_proto::pb::agent_message::Payload::Hello(h)) => h.agent_id,
            _ => {
                return Err(Status::invalid_argument("first frame must be SyncHello"));
            }
        };
        if claimed_agent_id != host_id.to_string() {
            tracing::warn!(
                cert_host_id = %host_id,
                claimed_agent_id = %claimed_agent_id,
                "Sync: SyncHello.agent_id does not match cert CN; using cert (Bl-2 defense)",
            );
        }

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

        // v0.3b: persist the agent's observed LAN IP so the DNS resolver can
        // resolve `<host>.<zone>` to it. Skipped on update failure (the
        // resolver will simply omit the record until the next reconnect).
        if let Some(ip) = remote_ip.as_deref() {
            if let Err(e) = self.inventory.set_host_lan_ip(host_id, ip).await {
                tracing::warn!(
                    error = %e,
                    agent = %host.hostname,
                    %ip,
                    "set_host_lan_ip failed; DNS resolver may miss this host until next reconnect",
                );
            }
        }

        // Build outbound channel. Controller uses this to send HeartbeatAcks,
        // ProxyConfig pushes, and other commands.
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
        let policy_ingest = self.policy_ingest.clone();
        let hook_ingest = self.hook_ingest.clone();
        let log_fanout = self.log_fanout.clone();
        let compose_broker = self.compose_broker.clone();
        let scheduler = self.scheduler.clone();
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

                        // Wisp: agent gossips its active
                        // runtime backend on every heartbeat so the
                        // dashboard / isd ps can show a backend
                        // column. The setter is idempotent + skips
                        // the UPDATE when the value already matches.
                        if !hb.runtime_backend.is_empty() {
                            if let Err(e) = inventory
                                .set_host_runtime_backend(host_id, &hb.runtime_backend)
                                .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    agent = %agent_hostname,
                                    "set_host_runtime_backend failed"
                                );
                            }
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

                        // Persist the heartbeat's agent
                        // labels for the scheduler. Older agents send an
                        // empty map; `replace_agent_labels` then leaves
                        // the host's row set empty (or clears any stale
                        // rows from a previous post-0.14 agent run). The
                        // scheduler reconcile trigger lives in Task 6;
                        // we persist here so that even before the
                        // scheduler is wired, `agent_labels` stays
                        // fresh on every heartbeat.
                        let label_map: std::collections::BTreeMap<String, String> = hb
                            .labels
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        if let Err(e) = inventory.replace_agent_labels(host_id, &label_map).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "replace_agent_labels failed",
                            );
                        }
                        // Feed the scheduler. Cheap when labels haven't
                        // changed (hash-compare skips the reconcile
                        // inside on_heartbeat_labels). When they have
                        // changed, the auto-place path (decision #3)
                        // re-evaluates every Pending placement.
                        if let Some(sched) = scheduler.as_ref() {
                            sched.on_heartbeat_labels(host_id, label_map).await;
                        }

                        // Ingest the per-container snapshot
                        // alongside services. Empty array (older agent)
                        // is handled by mark_containers_removed marking
                        // every prior row for this host as removed,
                        // which is the correct behaviour: an agent that
                        // stops reporting containers IS reporting that
                        // no containers are alive.
                        //
                        // TODO(phase-0.18 followup): wire a janitor
                        // task at controller startup that calls
                        // `containers::reap_removed_before(pool, now -
                        // 3600)` hourly. Deferred to keep this commit
                        // focused; the spec retention default is 1h.
                        if let Err(e) = crate::sync_containers::process_heartbeat_containers(
                            inventory.pool(),
                            host_id,
                            &hb.containers,
                            server_ts,
                        )
                        .await
                        {
                            tracing::error!(error = %e, agent = %agent_hostname, "process_heartbeat_containers failed");
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
                        // Container-scope policy ingest runs in
                        // parallel with the routing-rule ingest. Both consume
                        // the same payload; routing takes ownership last.
                        if let Err(e) = policy_ingest.ingest(host_id, &report).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "policy labels: ingest failed",
                            );
                        }
                        // Lifecycle-hook label ingest. Same shape;
                        // upserts container_hooks for any container carrying
                        // `isengard.hooks.*` labels.
                        if let Err(e) = hook_ingest.ingest(host_id, &report).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "hook labels: ingest failed",
                            );
                        }
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
                    Some(isengard_proto::pb::agent_message::Payload::LogChunk(chunk)) => {
                        // Route the chunk to the matching
                        // dashboard subscription. Dropped chunks are
                        // surfaced as `dropped` frames on the WebSocket;
                        // unknown subscriptions are simply dropped (the
                        // client already disconnected).
                        let _ = log_fanout.route(chunk).await;
                    }
                    Some(isengard_proto::pb::agent_message::Payload::StackComposeReport(
                        report,
                    )) => {
                        // v0.3c: persist the agent's reverse-engineered
                        // compose.yaml so `GET /api/v1/stacks/<id>/compose`
                        // can serve it without round-tripping to the host.
                        match inventory
                            .set_stack_compose(
                                host_id,
                                &report.stack_name,
                                &report.compose_yaml,
                                &report.sha256,
                                &report.imported_at,
                            )
                            .await
                        {
                            Ok(true) => tracing::debug!(
                                agent = %agent_hostname,
                                stack = %report.stack_name,
                                "compose: persisted import",
                            ),
                            Ok(false) => tracing::warn!(
                                agent = %agent_hostname,
                                stack = %report.stack_name,
                                "compose: report for unknown stack, skipping",
                            ),
                            Err(e) => tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                stack = %report.stack_name,
                                "compose: persist failed",
                            ),
                        }
                    }
                    Some(isengard_proto::pb::agent_message::Payload::WriteComposeAck(ack)) => {
                        // v0.3d: route the ack to the matching dashboard
                        // PUT handler. Unknown request_ids (timed-out
                        // dashboard, replayed agent) are dropped silently.
                        let resolved = compose_broker.resolve(ack.clone()).await;
                        if !resolved {
                            tracing::debug!(
                                agent = %agent_hostname,
                                request_id = %ack.request_id,
                                "compose_broker: ack for unknown request, dropping",
                            );
                        }
                    }
                    Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsRemoved(
                        ev,
                    )) => {
                        // Drop the container-scope policy row.
                        if let Err(e) = policy_ingest.ingest_removed(host_id, &ev).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "policy labels: ingest_removed failed",
                            );
                        }
                        // Drop the container_hooks row.
                        if let Err(e) = hook_ingest.ingest_removed(host_id, &ev).await {
                            tracing::warn!(
                                error = %e,
                                agent = %agent_hostname,
                                "hook labels: ingest_removed failed",
                            );
                        }
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

/// Extract the authoritative `HostId` from the caller's mTLS client cert.
/// The CN of every agent leaf is set to `host_id.to_string()` by
/// [`crate::ca::Authority::sign_agent_leaf`], so the CN is the source of
/// truth for which agent is calling.
///
/// Used by `renew_cert` (Bl-1 fix) and `sync` (Bl-2 fix) to prevent
/// horizontal-escalation attacks where one agent's cert is used to mint
/// or impersonate another agent.
///
/// Returns `Status::unauthenticated` on missing peer cert (the auth layer
/// rejects this on non-public methods, so this is a defense-in-depth
/// double-check) or a CN that doesn't parse as a ULID.
fn host_id_from_peer_cert<T>(request: &Request<T>) -> Result<isengard_storage::HostId, Status> {
    use tonic::transport::server::TlsConnectInfo;
    use x509_parser::prelude::*;

    let peer_certs = request
        .extensions()
        .get::<TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>()
        .and_then(|info| info.peer_certs())
        .ok_or_else(|| Status::unauthenticated("client cert required"))?;

    let cert_der = peer_certs
        .first()
        .ok_or_else(|| Status::unauthenticated("no client cert presented"))?;

    let (_, cert) = parse_x509_certificate(cert_der.as_ref())
        .map_err(|e| Status::unauthenticated(format!("malformed client cert: {e}")))?;

    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .ok_or_else(|| Status::unauthenticated("client cert has no CN"))?
        .as_str()
        .map_err(|e| Status::unauthenticated(format!("client cert CN not utf8: {e}")))?;

    let ulid = ulid::Ulid::from_string(cn)
        .map_err(|e| Status::unauthenticated(format!("client cert CN not a valid ULID: {e}")))?;

    Ok(isengard_storage::HostId(ulid))
}
