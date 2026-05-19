//! Long-lived Sync stream from agent to controller.
//!
//! Ships the happy path:
//!  - open one bidi stream
//!  - first frame: SyncHello { agent_id }
//!  - then: Heartbeat every `interval_secs` until the stream errors or the
//!    caller's cancellation Notify fires
//!  - read every ControllerMessage from the server side, log at debug
//!
//! Reconnection on drop is layered on top.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use isengard_core::{Event as CoreEvent, EventEmitter};
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::{AgentMessage, Event as ProtoEvent, Heartbeat, SyncHello, agent_message};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::transport::Endpoint;
use tracing::{debug, info, instrument, warn};

use crate::Result;
use crate::backoff::Backoff;
use crate::deployment::DeploymentSupervisor;
use crate::lifecycle_hooks::{self, HookContext, HookOutcome, HookPhase, HookSpec};
use crate::logs::LogSource;
use crate::mdns::MdnsResponder;
use crate::proxy::ProxyState;
use crate::runtime::RuntimeBackend;

/// Shared handle to the agent's mDNS responder. Optional: tests + docker-less
/// environments boot the agent without one, in which case the sync loop just
/// skips advertise calls.
pub type MdnsHandle = Arc<tokio::sync::Mutex<MdnsResponder>>;

/// v0.3d compose context: lets the sync loop service `WriteCompose`
/// ControllerMessages by writing to the agent's compose root and replying
/// with a `WriteComposeAck`. `None` makes the agent ignore WriteCompose
/// messages with a warn (used in tests / docker-less envs).
///
/// Also carries an [`EventEmitter`] handle so
/// lifecycle-hook execution can surface `lifecycle_hook.*` audit
/// events back to the controller via the existing outbound event
/// channel.
#[derive(Clone)]
pub struct ComposeContext {
    pub root: std::path::PathBuf,
    pub host_id: String,
    /// Outbound event sink for hook audit events. Optional: tests that
    /// don't care about hooks can pass `None` and the WriteCompose
    /// handler will fall back to a no-op emitter so hooks still run
    /// but their audit trail goes only to tracing logs.
    pub event_emitter: Option<Arc<dyn EventEmitter>>,
}

/// Split the proto's [`isengard_proto::pb::LifecycleHook`]
/// list (which carries every phase mixed together) into per-phase
/// [`HookSpec`] vectors. Unknown `on` values are silently dropped: the
/// dashboard validates the manifest on submit, so an unknown phase on
/// the wire is almost certainly a future-version controller talking to
/// a current agent. Drop-and-log keeps the agent forward-compatible.
fn split_hooks_by_phase(
    raw: &[isengard_proto::pb::LifecycleHook],
) -> (Vec<HookSpec>, Vec<HookSpec>, Vec<HookSpec>) {
    let mut pre = Vec::new();
    let mut post = Vec::new();
    let mut failure = Vec::new();
    for h in raw {
        let spec = HookSpec::from_argv(&h.cmd, h.timeout_ms, &h.on_error);
        match h.on.as_str() {
            "pre-deploy" => pre.push(spec),
            "post-deploy" => post.push(spec),
            "failure" => failure.push(spec),
            other => {
                tracing::debug!(on = other, "WriteCompose: skipping hook with unknown phase",);
            }
        }
    }
    (pre, post, failure)
}

/// In-process registry of active log subscriptions on this agent.
/// Each entry holds a `watch::Sender<bool>` whose receiver the corresponding
/// `run_tail` task selects on; flipping it to `true` cancels the tail.
type LogSubs =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, tokio::sync::watch::Sender<bool>>>>;

/// Open a Sync stream and run the heartbeat loop until the stream errors or
/// `cancel` fires. Returns Ok on graceful cancel; Err on stream error.
///
/// `events_rx` is borrowed (not moved) so that it survives reconnects: events
/// emitted while the stream is down stay queued in the channel until the next
/// stream comes up.
#[instrument(
    skip(
        endpoint,
        cancel,
        events_rx,
        agent_msg_rx,
        proxy_state,
        supervisor,
        log_source,
        mdns,
        compose_ctx,
        backend
    ),
    fields(agent_id = %agent_id)
)]
#[allow(clippy::too_many_arguments)]
pub async fn run_sync_loop<S: LogSource>(
    endpoint: Endpoint,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
    events_rx: &mut mpsc::Receiver<CoreEvent>,
    agent_msg_rx: &mut mpsc::Receiver<AgentMessage>,
    proxy_state: ProxyState,
    supervisor: Option<Arc<DeploymentSupervisor>>,
    log_source: Option<Arc<S>>,
    mdns: Option<MdnsHandle>,
    compose_ctx: Option<ComposeContext>,
    backend: Option<Arc<dyn RuntimeBackend>>,
    // Agent labels for placement selectors. Loaded once at
    // agent start; same value attached to every heartbeat. Empty map
    // means "no labels," which the controller treats as no `where:`
    // selectors will match (singletons / spreads with no selector still
    // place onto this host).
    agent_labels: std::collections::HashMap<String, String>,
) -> Result<()> {
    // MTLS replaces the bearer-token interceptor. The endpoint
    // already carries the client identity + CA root.
    let channel = endpoint
        .connect()
        .await
        .with_context(|| "connecting to controller (mTLS)".to_string())?;

    let mut client = ControllerClient::new(channel);

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

    // Reset the per-stream generation counter. The controller's per-host
    // generation lives in memory (`by_host[host_id].generation`) and resets
    // to 0 on controller restart. If the agent kept its previous high
    // counter, the very first push from the new controller (generation=1)
    // would be discarded as "stale" and the agent would never see new
    // routing rules / cert pushes until something forced a higher number.
    // A fresh sync stream means a fresh negotiation: trust the next push.
    proxy_state
        .last_generation
        .store(0, std::sync::atomic::Ordering::Release);

    // Spawn heartbeat task.
    let hb_tx = tx.clone();
    let interval = Duration::from_secs(u64::from(interval_secs.max(1)));
    let cancel_hb = cancel.clone();
    // Gossip the active runtime backend so `isd ps` can show
    // a per-host backend column. Empty string when the agent doesn't
    // know yet (no backend selected): the controller treats empty as
    // `docker` for back-compat with pre-0.5 agents.
    let runtime_backend = backend
        .as_ref()
        .map(|b| b.name().to_string())
        .unwrap_or_default();
    // Heartbeat reads the container snapshot via the live
    // backend when one was selected (so wisp hosts stop dialling
    // docker.sock once per heartbeat). Falls back to the legacy
    // bollard probe when backend selection failed at boot.
    let heartbeat_backend = backend.clone();
    // Snapshot agent labels for this stream lifetime. The
    // controller's scheduler reads these on every heartbeat to keep its
    // `agent_labels` table fresh. A fresh sync stream (post-reconnect)
    // re-uses the same in-memory snapshot the parent passed in.
    let heartbeat_labels = agent_labels.clone();
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
                    let snapshots = crate::container_snapshot::snapshots_via_backend_or_legacy(
                        heartbeat_backend.as_ref(),
                    )
                    .await;
                    let stacks = crate::container_snapshot::derive_stacks(&snapshots);
                    let services = crate::container_snapshot::derive_services(&snapshots);
                    // Ship one ContainerInfo per runtime
                    // container alongside the legacy services array.
                    // observed_at_ms uses the same agent-side `ts_ms`
                    // so the controller's last_seen clamp sees a
                    // consistent clock.
                    let containers = crate::container_snapshot::derive_containers(
                        &snapshots,
                        ts_ms as i64,
                    );
                    let msg = AgentMessage {
                        payload: Some(isengard_proto::pb::agent_message::Payload::Heartbeat(
                            Heartbeat {
                                ts_ms,
                                stacks,
                                services,
                                runtime_backend: runtime_backend.clone(),
                                containers,
                                labels: heartbeat_labels.clone(),
                            },
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

    // Read inbound messages (HeartbeatAck, ProxyConfig, AbortDeployment,
    // future Command/ConfigUpdate).
    let read_proxy_state = proxy_state.clone();
    let read_supervisor = supervisor.clone();
    let read_log_source = log_source.clone();
    let read_log_tx = tx.clone();
    let read_mdns = mdns.clone();
    let read_compose_ctx = compose_ctx.clone();
    let read_backend = backend.clone();
    let log_subs: LogSubs = Arc::new(tokio::sync::Mutex::new(Default::default()));
    let read_log_subs = log_subs.clone();
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
                    // Clone the rule list before handing the config to the
                    // proxy: mDNS apply runs after the proxy has installed
                    // the upstream registry so a router request that lands
                    // first sees an upstream, not just a DNS record.
                    let rules_for_mdns = cfg.rules.clone();
                    let backend_for_apply = read_backend.as_deref();
                    if let Err(e) = crate::proxy::apply_config_with_backend(
                        &read_proxy_state,
                        cfg,
                        backend_for_apply,
                    )
                    .await
                    {
                        warn!(error = %e, "proxy: apply_config failed");
                    }
                    if let Some(handle) = read_mdns.as_ref() {
                        let mut guard = handle.lock().await;
                        if let Err(e) = guard.apply(&rules_for_mdns) {
                            warn!(error = %e, "mdns: apply failed");
                        }
                    }
                }
                Some(isengard_proto::pb::controller_message::Payload::AbortDeployment(abort)) => {
                    if let Some(ref supervisor) = read_supervisor {
                        let cancelled = supervisor.handle_abort(&abort.deployment_id).await;
                        if !cancelled {
                            warn!(
                                deployment_id = %abort.deployment_id,
                                "AbortDeployment received for unknown id"
                            );
                        }
                    } else {
                        warn!("AbortDeployment received but supervisor not wired");
                    }
                }
                Some(isengard_proto::pb::controller_message::Payload::StartLogStream(start)) => {
                    let Some(source) = read_log_source.clone() else {
                        warn!(
                            sub = %start.subscription_id,
                            "StartLogStream received but no LogSource is wired (no docker)"
                        );
                        // Best-effort surfacing back to the controller.
                        let chunk = isengard_proto::pb::LogChunk {
                            subscription_id: start.subscription_id.clone(),
                            kind: isengard_proto::pb::log_chunk::Kind::Unavailable as i32,
                            occurred_at: String::new(),
                            stream: String::new(),
                            line: String::new(),
                            dropped: 0,
                            reason: "agent_no_docker".into(),
                        };
                        let _ = read_log_tx
                            .send(AgentMessage {
                                payload: Some(agent_message::Payload::LogChunk(chunk)),
                            })
                            .await;
                        continue;
                    };
                    let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);
                    {
                        let mut subs = read_log_subs.lock().await;
                        subs.insert(start.subscription_id.clone(), abort_tx);
                    }
                    let out = read_log_tx.clone();
                    let sub_id = start.subscription_id.clone();
                    let container = start.container_name.clone();
                    let tail = start.tail;
                    let follow = start.follow;
                    let subs_for_cleanup = read_log_subs.clone();
                    tokio::spawn(async move {
                        crate::logs::run_tail(
                            source,
                            sub_id.clone(),
                            container,
                            tail,
                            follow,
                            abort_rx,
                            out,
                        )
                        .await;
                        // Drop the entry on natural completion so a fresh
                        // StartLogStream with the same id can take over.
                        subs_for_cleanup.lock().await.remove(&sub_id);
                    });
                }
                Some(isengard_proto::pb::controller_message::Payload::StopLogStream(stop)) => {
                    let mut subs = read_log_subs.lock().await;
                    if let Some(tx) = subs.remove(&stop.subscription_id) {
                        let _ = tx.send(true);
                    }
                }
                Some(isengard_proto::pb::controller_message::Payload::WriteCompose(req)) => {
                    let Some(ctx) = read_compose_ctx.as_ref() else {
                        warn!(
                            request_id = %req.request_id,
                            stack = %req.stack_name,
                            "WriteCompose received but no ComposeContext (no docker)",
                        );
                        let ack = isengard_proto::pb::WriteComposeAck {
                            request_id: req.request_id.clone(),
                            kind: isengard_proto::pb::write_compose_ack::Kind::Error as i32,
                            error: "agent has no compose context".into(),
                            ..Default::default()
                        };
                        let _ = read_log_tx
                            .send(AgentMessage {
                                payload: Some(agent_message::Payload::WriteComposeAck(ack)),
                            })
                            .await;
                        continue;
                    };
                    let stack_dir = ctx.root.join(&req.stack_name);

                    // Split hooks by phase, build
                    // the per-deploy [`HookContext`], and run pre-deploy
                    // hooks BEFORE the compose write. Pre-deploy hook
                    // failure aborts the deploy: WriteComposeAck =
                    // ERROR, no compose written.
                    let (pre_hooks, post_hooks, failure_hooks) = split_hooks_by_phase(&req.hooks);
                    let hook_ctx = HookContext {
                        stack: req.stack_name.clone(),
                        host_id: ctx.host_id.clone(),
                        deployment_id: req.deployment_id.clone(),
                        stack_dir: stack_dir.clone(),
                        failure_reason: None,
                        failure_detail: None,
                    };
                    let noop_emitter: Arc<dyn EventEmitter> = Arc::new(isengard_core::NoopEmitter);
                    let emitter: Arc<dyn EventEmitter> = ctx
                        .event_emitter
                        .clone()
                        .unwrap_or_else(|| noop_emitter.clone());

                    let pre_outcome = lifecycle_hooks::run_hooks(
                        HookPhase::PreDeploy,
                        &pre_hooks,
                        &hook_ctx,
                        emitter.as_ref(),
                    )
                    .await;
                    if let HookOutcome::Aborted { reason, .. } = &pre_outcome {
                        warn!(
                            request_id = %req.request_id,
                            stack = %req.stack_name,
                            reason = %reason,
                            "WriteCompose: pre-deploy hook aborted; refusing compose write",
                        );
                        let mut fail_ctx = hook_ctx.clone();
                        fail_ctx.failure_reason = Some("pre-deploy hook aborted".into());
                        fail_ctx.failure_detail = Some(reason.clone());
                        let _ = lifecycle_hooks::run_hooks(
                            HookPhase::Failure,
                            &failure_hooks,
                            &fail_ctx,
                            emitter.as_ref(),
                        )
                        .await;
                        let ack = isengard_proto::pb::WriteComposeAck {
                            request_id: req.request_id.clone(),
                            kind: isengard_proto::pb::write_compose_ack::Kind::Error as i32,
                            error: format!("pre-deploy hook aborted: {reason}"),
                            ..Default::default()
                        };
                        let _ = read_log_tx
                            .send(AgentMessage {
                                payload: Some(agent_message::Payload::WriteComposeAck(ack)),
                            })
                            .await;
                        continue;
                    }

                    let outcome = crate::compose_writer::apply_controller_write(
                        &stack_dir,
                        &req.compose_yaml,
                        &req.expected_sha256,
                        &ctx.host_id,
                        req.force,
                        // Persist verbatim stack.toml beside
                        // compose.yml. The agent does NOT parse it; the
                        // hook + secrets behavior is driven by the
                        // explicit proto fields.
                        &req.manifest_toml,
                    );

                    // ---- post-deploy OR failure hooks ----------------
                    // Compose write failure -> fire failure hooks.
                    // Compose write success -> fire post-deploy hooks.
                    // Post-deploy failure does NOT roll back the deploy
                    // (the compose already wrote); it logs a warning and
                    // also fires failure hooks for symmetry.
                    let write_failed: Option<(String, String)> = match &outcome {
                        crate::compose_writer::ApplyWriteOutcome::Ok { .. } => None,
                        crate::compose_writer::ApplyWriteOutcome::Conflict {
                            current_sha256,
                            ..
                        } => Some((
                            "compose write conflict".into(),
                            format!("on-disk sha256 = {current_sha256}"),
                        )),
                        crate::compose_writer::ApplyWriteOutcome::Error(e) => {
                            Some(("compose write error".into(), e.clone()))
                        }
                    };

                    if let Some((reason, detail)) = &write_failed {
                        let mut fail_ctx = hook_ctx.clone();
                        fail_ctx.failure_reason = Some(reason.clone());
                        fail_ctx.failure_detail = Some(detail.clone());
                        let _ = lifecycle_hooks::run_hooks(
                            HookPhase::Failure,
                            &failure_hooks,
                            &fail_ctx,
                            emitter.as_ref(),
                        )
                        .await;
                    } else {
                        let post = lifecycle_hooks::run_hooks(
                            HookPhase::PostDeploy,
                            &post_hooks,
                            &hook_ctx,
                            emitter.as_ref(),
                        )
                        .await;
                        if let HookOutcome::Aborted { reason, .. } = &post {
                            warn!(
                                request_id = %req.request_id,
                                stack = %req.stack_name,
                                reason = %reason,
                                "WriteCompose: post-deploy hook failed (compose write succeeded; not rolling back)",
                            );
                            let mut fail_ctx = hook_ctx.clone();
                            fail_ctx.failure_reason = Some("post-deploy hook aborted".into());
                            fail_ctx.failure_detail = Some(reason.clone());
                            let _ = lifecycle_hooks::run_hooks(
                                HookPhase::Failure,
                                &failure_hooks,
                                &fail_ctx,
                                emitter.as_ref(),
                            )
                            .await;
                        }
                    }

                    let ack = match outcome {
                        crate::compose_writer::ApplyWriteOutcome::Ok { written_sha256 } => {
                            isengard_proto::pb::WriteComposeAck {
                                request_id: req.request_id.clone(),
                                kind: isengard_proto::pb::write_compose_ack::Kind::Ok as i32,
                                error: String::new(),
                                current_sha256: String::new(),
                                current_yaml: String::new(),
                                written_sha256,
                            }
                        }
                        crate::compose_writer::ApplyWriteOutcome::Conflict {
                            current_sha256,
                            current_yaml,
                        } => isengard_proto::pb::WriteComposeAck {
                            request_id: req.request_id.clone(),
                            kind: isengard_proto::pb::write_compose_ack::Kind::Conflict as i32,
                            error: "on-disk hash mismatch".into(),
                            current_sha256,
                            current_yaml,
                            written_sha256: String::new(),
                        },
                        crate::compose_writer::ApplyWriteOutcome::Error(e) => {
                            isengard_proto::pb::WriteComposeAck {
                                request_id: req.request_id.clone(),
                                kind: isengard_proto::pb::write_compose_ack::Kind::Error as i32,
                                error: e,
                                current_sha256: String::new(),
                                current_yaml: String::new(),
                                written_sha256: String::new(),
                            }
                        }
                    };
                    let _ = read_log_tx
                        .send(AgentMessage {
                            payload: Some(agent_message::Payload::WriteComposeAck(ack)),
                        })
                        .await;
                }
                _ => {
                    warn!(?msg.payload, "unexpected ControllerMessage payload");
                }
            }
        }
        // Stream closed: abort every still-running subscription so the
        // tasks unwind promptly. (Not strictly required since `out` will
        // close on drop, but explicit cancellation avoids race with Sender
        // refcounts.)
        let mut subs = read_log_subs.lock().await;
        for (_, tx) in subs.drain() {
            let _ = tx.send(true);
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
            maybe_msg = agent_msg_rx.recv() => {
                let Some(msg) = maybe_msg else {
                    // Pre-built AgentMessage channel closed — treat like cancel.
                    info!("agent_msg channel closed, shutting down sync");
                    break Ok(());
                };
                if let Err(e) = tx.send(msg).await {
                    warn!(error = %e, "failed to forward agent message over sync stream");
                    break Err(anyhow::anyhow!("outbound channel closed while forwarding agent message"));
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
///
/// Imp-2: takes an `Arc<RwLock<Endpoint>>` so the cert renewal task can swap
/// in a freshly-built Endpoint after rotating the on-disk cert. Each
/// reconnect attempt clones the *current* endpoint, so the new cert
/// propagates the next time the stream cycles (no agent restart needed).
#[instrument(
    skip(
        endpoint,
        cancel,
        events_rx,
        agent_msg_rx,
        proxy_state,
        supervisor,
        log_source,
        mdns,
        compose_ctx,
        backend
    ),
    fields(agent_id = %agent_id)
)]
#[allow(clippy::too_many_arguments)]
pub async fn run_sync_with_reconnect<S: LogSource>(
    endpoint: Arc<tokio::sync::RwLock<Endpoint>>,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
    events_rx: &mut mpsc::Receiver<CoreEvent>,
    agent_msg_rx: &mut mpsc::Receiver<AgentMessage>,
    proxy_state: ProxyState,
    supervisor: Option<Arc<DeploymentSupervisor>>,
    log_source: Option<Arc<S>>,
    mdns: Option<MdnsHandle>,
    compose_ctx: Option<ComposeContext>,
    backend: Option<Arc<dyn RuntimeBackend>>,
    // See run_sync_loop. Same value is reused across reconnect
    // attempts.
    agent_labels: std::collections::HashMap<String, String>,
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

        // Snapshot the current endpoint. Holding the read lock for the
        // length of the connect+sync would block renewals; cloning is cheap
        // (Endpoint is a config bundle, not a live connection).
        let endpoint_snapshot = endpoint.read().await.clone();
        let attempt_started = Instant::now();
        let result = run_sync_loop(
            endpoint_snapshot,
            agent_id.clone(),
            interval_secs,
            cancel.clone(),
            events_rx,
            agent_msg_rx,
            proxy_state.clone(),
            supervisor.clone(),
            log_source.clone(),
            mdns.clone(),
            compose_ctx.clone(),
            backend.clone(),
            agent_labels.clone(),
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
