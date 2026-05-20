//! v0.3d compose apply: take a [`ReconcilePlan`] + the desired compose,
//! drive the [`crate::runtime::RuntimeBackend`] to bring the running
//! containers in line.
//!
//! Scope: v0.3d implements `Start | Recreate | Stop` for the smoke-test
//! shape (image, env, ports, restart, labels). Networks, volumes,
//! healthchecks, and depends_on are passed through best-effort but not
//! reconciled per-service. Follow-up work tracked in the v0.3 status
//! note.
//!
//! Rewritten to drive the [`crate::runtime::RuntimeBackend`]
//! trait instead of bollard directly. Bollard remains the default
//! backend; the byte-level Config the bollard backend hands to dockerd
//! still flows through `BollardBackend::spec_to_config`, so the v0.3d
//! reconcile bytes are unchanged.

use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::transport::Endpoint;

use crate::compose_reconciler::{
    DesiredCompose, DesiredService, ReconcilePlan, RunningService, ServiceOp, build_plan,
    parse_compose,
};
use crate::runtime::{
    ContainerCreateSpec, ListFilter, MountKind, MountSpec, PortProtocol, PortSpec,
    RestartPolicy as SpecRestartPolicy, RuntimeBackend, SecretMount,
};
use crate::secret_fetch;

/// Outcome of applying a [`ReconcilePlan`]. Each entry mirrors the
/// matching plan op; failures attach an error string but do not abort
/// the whole sweep.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// The plan op this outcome covers.
    pub op: ServiceOp,
    /// Error string when the op failed, `None` on success.
    pub error: Option<String>,
}

/// Bring the running containers for `stack` in line with `compose_yaml`.
/// Returns the plan that was attempted plus per-op outcomes.
///
/// This variant is for stacks WITHOUT external secrets. When a compose
/// file declares any top-level `secrets: { ... external: true }`, callers
/// should use [`reconcile_stack_with_secrets`] instead so the agent
/// fetches + tmpfs-mounts the values at apply time.
pub async fn reconcile_stack(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    compose_yaml: &str,
) -> anyhow::Result<(ReconcilePlan, Vec<ApplyOutcome>)> {
    let desired = parse_compose(compose_yaml)?;
    let running = list_running_for_stack(backend, stack_name).await?;
    let plan = build_plan(stack_name, &desired, &running);
    let outcomes = apply_plan(backend, stack_name, &desired, &plan, None, &[]).await;
    Ok((plan, outcomes))
}

/// v0.3.6: variant of [`reconcile_stack`] that fetches every external
/// secret referenced by the compose, materialises each on tmpfs, and
/// bind-mounts them at the per-service target path inside each
/// container. The endpoint holder is the same one used by the sync
/// stream: the agent's mTLS client cert authenticates the FetchSecret
/// RPC.
///
/// Cleanup for stopped containers happens inline via
/// [`secret_fetch::cleanup_for_container`]; recreate cleans the prior
/// container's directory before the new one is started.
pub async fn reconcile_stack_with_secrets(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    compose_yaml: &str,
    controller_endpoint: Arc<RwLock<Endpoint>>,
) -> anyhow::Result<(ReconcilePlan, Vec<ApplyOutcome>)> {
    reconcile_stack_with_stack_secrets(backend, stack_name, compose_yaml, controller_endpoint, &[])
        .await
}

/// Follow-up: same as [`reconcile_stack_with_secrets`] but
/// also mounts the supplied stack-level secret names into every service
/// of the stack. Stack-level secrets come from `secrets = [...]` in the
/// operator's `stack.toml` and always mount at `/run/secrets/<name>`
/// (Swarm-compatible default path).
///
/// When the same secret name appears both at stack level and in a
/// service's per-service `secrets:` list, the per-service entry wins
/// (so a long-form `target:` override is honoured); the value is
/// fetched + materialised + mounted exactly once per service.
pub async fn reconcile_stack_with_stack_secrets(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    compose_yaml: &str,
    controller_endpoint: Arc<RwLock<Endpoint>>,
    stack_level_secrets: &[String],
) -> anyhow::Result<(ReconcilePlan, Vec<ApplyOutcome>)> {
    let desired = parse_compose(compose_yaml)?;
    // Fail loud at parse time if a service references an undeclared
    // top-level secret. The dashboard / `isd diff` already caught this
    // for the common case, but the agent re-validates so a malformed
    // file written directly to disk can't slip through.
    desired.referenced_external_secrets()?;
    let running = list_running_for_stack(backend, stack_name).await?;
    let plan = build_plan(stack_name, &desired, &running);
    let outcomes = apply_plan(
        backend,
        stack_name,
        &desired,
        &plan,
        Some(controller_endpoint),
        stack_level_secrets,
    )
    .await;
    Ok((plan, outcomes))
}

/// List the running containers belonging to `stack_name`. Filters via
/// the trait's [`ListFilter`] (which encodes both
/// `com.docker.compose.project=<stack>` and
/// `isengard.stack=<stack>` for the bollard backend), then inspects
/// each handle to fill in env / ports / restart so the diff path has
/// what it needs.
pub async fn list_running_for_stack(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
) -> anyhow::Result<Vec<RunningService>> {
    let summaries = backend
        .list_containers(ListFilter {
            stack: Some(stack_name.to_string()),
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("list_containers for stack {stack_name}: {e}"))?;

    let mut out = Vec::with_capacity(summaries.len());
    for s in summaries {
        // For bollard the list response doesn't carry env / ports /
        // restart; re-inspect to populate them. For wisp the list
        // response is already complete, but inspect is a cheap
        // file-read so we re-call uniformly. If inspect fails (e.g.
        // container removed mid-sweep) we drop the entry rather than
        // surfacing partial data.
        let snap = match backend.inspect_container(&s.id).await {
            Ok(Some(snap)) => snap,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    container_id = %s.id,
                    "compose_apply: inspect failed",
                );
                continue;
            }
        };
        if let Some(rs) = RunningService::from_snapshot(&snap) {
            out.push(rs);
        }
    }
    Ok(out)
}

/// Maximum concurrent ops in [`apply_plan`]. Sized for a homelab; eight
/// containers can pull + extract + start in parallel without saturating
/// disk IO on the boxes Isengard targets. Tunable as a future env knob
/// once a fleet's bottleneck profile demands it.
const APPLY_PLAN_CONCURRENCY: usize = 8;

/// Apply each op in `plan` concurrently and return one outcome per op,
/// in plan-declaration order. Up to [`APPLY_PLAN_CONCURRENCY`] ops run
/// in parallel. Failures don't short-circuit: the operator gets the
/// full summary at the end.
///
/// Parallelism. Live on lausanne servarr reconciles took
/// ~30s wall-clock for 8 services because the previous `for op in
/// &plan.ops` loop awaited each container's pull + extract + start
/// before moving on. With `buffer_unordered` the wall-clock collapses
/// to roughly `max(per_service_time)`.
///
/// Ordering constraints preserved:
///
/// 1. Stop-before-Start within one op (Recreate). Single-op concern,
///    not cross-op: each future already does this dance internally.
/// 2. Networks ensured before any container attaches. The pre-pass
///    below walks every distinct network name across all ops that will
///    create or recreate a container and calls
///    [`RuntimeBackend::ensure_network`] sequentially. The per-container
///    create still re-checks the registry, but at that point each
///    bridge already exists and iptables is already applied, so the
///    work is a fast no-op.
/// 3. iptables / DNAT: dockerd serialises its own iptables writes, so
///    the agent does not need a per-process mutex here.
///
/// Logging: each ensure / create / start call line carries
/// `service=<name>`, so interleaved completion order is still
/// trivially demuxable in the operator's tail.
///
/// `controller_endpoint`: when `Some`, services with `secrets:` entries
/// fetch each value over the controller's FetchSecret RPC, materialise
/// it on tmpfs, and bind-mount the directory into the container. When
/// `None`, secrets are silently dropped (matches the v0.3d call site).
///
/// `stack_level_secrets`: follow-up. Names declared via
/// `secrets = [...]` in the operator's `stack.toml`. Each name is
/// fetched + mounted at `/run/secrets/<name>` in every service of the
/// stack. Empty when the stack has no manifest or no stack-level
/// secrets declared.
pub async fn apply_plan(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    desired: &DesiredCompose,
    plan: &ReconcilePlan,
    controller_endpoint: Option<Arc<RwLock<Endpoint>>>,
    stack_level_secrets: &[String],
) -> Vec<ApplyOutcome> {
    use futures::stream::{self, StreamExt};

    // Pre-pass 1: collect distinct network names across every op that
    // will create or recreate a container, and ensure each on the host
    // before the parallel fan-out fires. Idempotent; failures are
    // logged here and re-surfaced as part of the per-op outcome (the
    // create-time ensure_bridge call will fail with the same error).
    let mut nets: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for op in &plan.ops {
        let service = match op {
            ServiceOp::Start { service, .. } | ServiceOp::Recreate { service, .. } => service,
            ServiceOp::Stop { .. } | ServiceOp::NoChange { .. } => continue,
        };
        if let Some(svc) = desired.services.get(service) {
            for n in &svc.networks {
                nets.insert(n.clone());
            }
        }
    }
    for net in &nets {
        if let Err(e) = backend.ensure_network(net).await {
            tracing::warn!(
                error = %e,
                stack = %stack_name,
                network = %net,
                "compose_apply: pre-pass ensure_network failed; per-op create will retry",
            );
        }
    }

    // Pre-pass 2: snapshot the running containers once, so each
    // Recreate future doesn't fan out N concurrent list_containers
    // calls. The list is read-only relative to the apply path.
    let running = list_running_for_stack(backend, stack_name)
        .await
        .unwrap_or_default();

    // Fan out: each op produces a `(index, outcome)` future so we can
    // re-sort into declaration order after `buffer_unordered` drains.
    // The owned ServiceOp clone keeps the future `'static`-friendly
    // when the planner hands us a borrow; the outer apply_plan future
    // already holds `&desired` / `&running` for the duration of the
    // stream, so the inner borrow is sound.
    let running_ref = &running;
    let indexed: Vec<(usize, ServiceOp)> = plan.ops.iter().cloned().enumerate().collect();
    let mut collected: Vec<(usize, ApplyOutcome)> = stream::iter(indexed)
        .map(|(idx, op)| {
            let controller_endpoint = controller_endpoint.clone();
            async move {
                let outcome = apply_one_op(
                    backend,
                    stack_name,
                    desired,
                    &op,
                    running_ref,
                    controller_endpoint,
                    stack_level_secrets,
                )
                .await;
                (idx, outcome)
            }
        })
        .buffer_unordered(APPLY_PLAN_CONCURRENCY)
        .collect()
        .await;
    collected.sort_by_key(|(i, _)| *i);
    collected.into_iter().map(|(_, o)| o).collect()
}

/// Apply a single op. Extracted from [`apply_plan`] so the parallel
/// `buffer_unordered` future can be a plain async block. Mirrors the
/// earlier sequential body exactly.
async fn apply_one_op(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    desired: &DesiredCompose,
    op: &ServiceOp,
    running: &[RunningService],
    controller_endpoint: Option<Arc<RwLock<Endpoint>>>,
    stack_level_secrets: &[String],
) -> ApplyOutcome {
    let result: anyhow::Result<()> = match op {
        ServiceOp::NoChange { .. } => Ok(()),
        ServiceOp::Start { service, .. } => match desired.services.get(service) {
            None => {
                return ApplyOutcome {
                    op: op.clone(),
                    error: Some(format!("service {service} missing from compose")),
                };
            }
            Some(svc) => {
                ensure_container_started(
                    backend,
                    stack_name,
                    svc,
                    None,
                    desired,
                    controller_endpoint,
                    stack_level_secrets,
                )
                .await
            }
        },
        ServiceOp::Recreate { service, .. } => match desired.services.get(service) {
            None => {
                return ApplyOutcome {
                    op: op.clone(),
                    error: Some(format!("service {service} missing from compose")),
                };
            }
            Some(svc) => {
                // Find the existing container in the pre-snapshot.
                // Best-effort: if the container is gone we still attempt
                // the start.
                let prior = running.iter().find(|r| &r.service_name == service);
                if let Some(prior) = prior {
                    let _ = stop_and_remove(backend, &prior.container_id).await;
                    // Cleanup any stale tmpfs secrets from the previous
                    // container; the new one gets a fresh fetch.
                    let prior_container = container_name_for(stack_name, service, svc);
                    let _ = secret_fetch::cleanup_for_container(&prior_container);
                }
                ensure_container_started(
                    backend,
                    stack_name,
                    svc,
                    prior.map(|r| r.container_id.as_str()),
                    desired,
                    controller_endpoint,
                    stack_level_secrets,
                )
                .await
            }
        },
        ServiceOp::Stop {
            service,
            container_id,
        } => {
            let res = stop_and_remove(backend, container_id).await;
            // Best-effort cleanup of any tmpfs secrets the prior
            // container had mounted. The actual container_name is
            // either the operator's `container_name:` or our default.
            if let Some(svc) = desired.services.get(service) {
                let cn = container_name_for(stack_name, service, svc);
                let _ = secret_fetch::cleanup_for_container(&cn);
            } else {
                let cn = format!("{stack_name}-{service}");
                let _ = secret_fetch::cleanup_for_container(&cn);
            }
            res
        }
    };
    ApplyOutcome {
        op: op.clone(),
        error: result.err().map(|e| format!("{e:#}")),
    }
}

/// Compute the container name the agent will use for this service.
/// Mirrors the logic in [`ensure_container_started`].
fn container_name_for(stack_name: &str, service: &str, svc: &DesiredService) -> String {
    svc.container_name
        .clone()
        .unwrap_or_else(|| format!("{stack_name}-{service}"))
}

/// Follow-up: build the ordered `(name, container_path)`
/// list of secrets the agent should fetch + mount for one service.
///
/// Order: per-service `secrets:` (in declaration order, with their
/// `target:` overrides) first; then stack-level `secrets = [...]` names
/// not already covered, each mounted at the Swarm-compatible default
/// `/run/secrets/<name>`. Same-name collisions resolve in favour of
/// the per-service entry: the value is fetched + mounted exactly once.
///
/// Returns an error when a per-service reference points at a top-level
/// secret that isn't declared external (the existing v0.3.6 invariant).
fn merge_secret_targets(
    desired: &DesiredCompose,
    service: &str,
    stack_level_secrets: &[String],
) -> anyhow::Result<Vec<(String, String)>> {
    // Per-service first so its `target:` wins on collision.
    let per_service = desired.service_secret_targets(service)?;
    let mut out: Vec<(String, String)> =
        Vec::with_capacity(per_service.len() + stack_level_secrets.len());
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (n, t) in per_service {
        if seen.insert(n.clone()) {
            out.push((n, t));
        }
    }
    for name in stack_level_secrets {
        if seen.insert(name.clone()) {
            out.push((name.clone(), format!("/run/secrets/{name}")));
        }
    }
    Ok(out)
}

/// Translate a parsed compose [`DesiredService`] into the
/// backend-agnostic [`ContainerCreateSpec`] every [`crate::runtime::RuntimeBackend`]
/// understands.
///
/// Wisp arc: this is the inverse of
/// [`crate::runtime::bollard_backend::spec_to_config`]. The compose
/// pipeline historically built `bollard::Config<String>` directly from a
/// `DesiredService`; threading the trait through reconcile_stack means
/// emitting a [`ContainerCreateSpec`] here and letting each backend's
/// `create_container` produce its own native shape (bollard's
/// `Config<String>` or wisp's `BundleBuilder` overrides).
///
/// Mapping rules:
/// - `binds` (already `host:container[:options]` strings the secret /
///   compose layer produced) are split back into [`MountSpec`] entries
///   so the backend translation is uniform. The `:ro` / `:rw` suffix is
///   honoured.
/// - `secrets` are passed through as [`SecretMount`] entries with their
///   target paths; the backend turns each into a bind-mount of the
///   agent-materialised tmpfs path.
/// - Compose port strings are parsed into [`PortSpec`] entries via
///   [`parse_port_mapping`]; protocol defaults to TCP, host_ip is set
///   when the operator wrote `127.0.0.1:8080:80`.
/// - `restart:` strings translate to [`SpecRestartPolicy`] (default `No`).
/// - Labels gain the compose project + isengard.stack pair so a later
///   reconcile can match the container back to its service.
pub fn desired_service_to_create_spec(
    stack_name: &str,
    svc: &DesiredService,
    binds: &[String],
    secrets: Vec<SecretMount>,
) -> ContainerCreateSpec {
    let container_name = container_name_for(stack_name, &svc.name, svc);

    // Labels: ensure compose project + service labels are present so the
    // next reconcile can match the container back to the service.
    let mut labels = svc.labels.clone();
    labels.insert(
        "com.docker.compose.project".to_string(),
        stack_name.to_string(),
    );
    labels.insert("com.docker.compose.service".to_string(), svc.name.clone());
    labels.insert("isengard.stack".to_string(), stack_name.to_string());
    // compose `cap_add:` flows through the `isengard.cap.add` label.
    // The bollard backend reads it back and threads the entries into
    // `HostConfig.CapAdd` when creating the container. Carried as a
    // label rather than a typed field on `ContainerCreateSpec` to keep
    // the persisted spec.json shape unchanged across upgrades.
    if !svc.cap_add.is_empty() {
        labels.insert("isengard.cap.add".to_string(), svc.cap_add.join(","));
    }

    // Port mappings.
    let mut ports: Vec<PortSpec> = Vec::new();
    for spec in &svc.ports {
        if let Some(p) = compose_port_to_spec(spec) {
            ports.push(p);
        }
    }

    // Bind strings -> MountSpec entries.
    let mut mounts: Vec<MountSpec> = Vec::new();
    for bind in binds {
        if let Some(m) = parse_bind_string(bind) {
            mounts.push(m);
        }
    }

    // restart: translate to typed RestartPolicy.
    let restart = match svc.restart.as_deref() {
        Some("always") => SpecRestartPolicy::Always,
        Some("on-failure") => SpecRestartPolicy::OnFailure { max_retries: None },
        Some("unless-stopped") => SpecRestartPolicy::UnlessStopped,
        _ => SpecRestartPolicy::No,
    };

    ContainerCreateSpec {
        container_name,
        image: svc.image.clone().unwrap_or_default(),
        stack: stack_name.to_string(),
        service: svc.name.clone(),
        command: svc.command.clone(),
        entrypoint: svc.entrypoint.clone(),
        env: svc.environment.clone(),
        labels,
        mounts,
        ports,
        networks: svc.networks.clone(),
        restart,
        healthcheck: None,
        user: None,
        working_dir: None,
        hostname: None,
        linux_resources: None,
        secrets,
    }
}

/// Parse a compose port string into a [`PortSpec`]. Accepts `"80"`,
/// `"8080:80"`, `"8080:80/udp"`, `"127.0.0.1:8080:80"`, or full
/// `"127.0.0.1:8080:80/tcp"`. Returns `None` for bare container ports
/// (compose expose, no mapping) since reconcile only cares about
/// published ports.
fn compose_port_to_spec(spec: &str) -> Option<PortSpec> {
    let (host_port_str, container_part) = parse_port_mapping(spec)?;
    let host_port: u16 = host_port_str.parse().ok()?;
    // Optional /proto suffix.
    let (cport_str, protocol) = match container_part.split_once('/') {
        Some((p, "udp")) => (p.to_string(), PortProtocol::Udp),
        Some((p, _)) => (p.to_string(), PortProtocol::Tcp),
        None => (container_part, PortProtocol::Tcp),
    };
    let container_port: u16 = cport_str.parse().ok()?;
    let host_ip = parse_host_ip_prefix(spec);
    Some(PortSpec {
        host_ip,
        host_port,
        container_port,
        protocol,
    })
}

/// Pull the optional `host_ip:` prefix out of a compose port string.
/// Returns `None` when the spec is `host:container[/proto]` (two colons
/// or fewer) or unparsable.
fn parse_host_ip_prefix(spec: &str) -> Option<std::net::IpAddr> {
    if spec.matches(':').count() < 2 {
        return None;
    }
    let (ip_str, _rest) = spec.split_once(':')?;
    ip_str.parse().ok()
}

/// Split a docker-style bind string into a [`MountSpec`]. Accepts the
/// `host:container` and `host:container:ro` / `:rw` forms produced by
/// the secret_fetch / compose path.
fn parse_bind_string(bind: &str) -> Option<MountSpec> {
    let parts: Vec<&str> = bind.split(':').collect();
    let (source, target, read_only) = match parts.as_slice() {
        [src, dst] => ((*src).to_string(), (*dst).to_string(), false),
        [src, dst, opts] => {
            let read_only = opts.split(',').any(|t| t.trim() == "ro");
            ((*src).to_string(), (*dst).to_string(), read_only)
        }
        _ => return None,
    };
    Some(MountSpec {
        source,
        target,
        kind: MountKind::Bind,
        read_only,
    })
}

#[allow(clippy::too_many_arguments)]
/// Internal helper: ensure container started.
async fn ensure_container_started(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    svc: &DesiredService,
    _prior_container: Option<&str>,
    desired: &DesiredCompose,
    controller_endpoint: Option<Arc<RwLock<Endpoint>>>,
    stack_level_secrets: &[String],
) -> anyhow::Result<()> {
    if svc.image.is_none() {
        return Err(anyhow::anyhow!("service {} has no image", svc.name));
    }
    let container_name = container_name_for(stack_name, &svc.name, svc);

    // v0.3.6 + follow-up: union of per-service `secrets:`
    // and stack-level `secrets = [...]` (the latter mounts into every
    // service of the stack at the default `/run/secrets/<name>` path).
    // Per-service entries win on collision so `target:` overrides are
    // honoured; each name is fetched + mounted exactly once.
    let merged_targets = merge_secret_targets(desired, &svc.name, stack_level_secrets)?;
    let mut binds: Vec<String> = Vec::new();
    let mut secret_mounts: Vec<SecretMount> = Vec::new();
    if !merged_targets.is_empty() {
        let Some(endpoint) = controller_endpoint.clone() else {
            return Err(anyhow::anyhow!(
                "service {} references secrets but the agent has no \
                 controller endpoint configured for FetchSecret",
                svc.name,
            ));
        };
        // Cleanup any leftover host directory before we re-fetch.
        let _ = secret_fetch::cleanup_for_container(&container_name);
        let names: Vec<String> = merged_targets.iter().map(|(n, _)| n.clone()).collect();
        let materialised = secret_fetch::fetch_and_materialise(endpoint, &container_name, &names)
            .await
            .map_err(|e| anyhow::anyhow!("fetch + materialise secrets for {}: {e}", svc.name))?;
        // Compose target paths come from the merged list (per-service
        // long-form `target:` overrides, or `/run/secrets/<name>` for
        // stack-level entries). The order is stable: merge_secret_targets
        // and fetch_and_materialise both walk the same merged list.
        //
        // Two parallel collections: `binds` are kept for backward
        // compatibility with the bollard call shape (the legacy
        // build_create_config consumed `host:container:ro` strings);
        // `secret_mounts` carries the structured form the trait
        // surface uses. BollardBackend's spec_to_config consumes
        // SecretMount entries directly.
        for (m, (_, target)) in materialised.iter().zip(merged_targets.iter()) {
            binds.push(format!("{}:{}:ro", m.host_path.to_string_lossy(), target));
            secret_mounts.push(SecretMount {
                source: m.host_path.to_string_lossy().to_string(),
                target: std::path::PathBuf::from(target),
                mode: 0o400,
            });
        }
    }

    // Build the backend-agnostic spec. `binds` is intentionally empty
    // here: the secret bind-mounts are routed through the typed
    // `secrets` field so each backend (bollard, wisp) can decide how to
    // surface them. Plain compose volumes would land in `binds` once
    // the parser models them; today the reconciler doesn't.
    let spec = desired_service_to_create_spec(stack_name, svc, &[], secret_mounts);

    // Best-effort: a previous run may have left a container with the
    // same name. Stop+remove it before creating a new one. The
    // backend's create_container errors out on conflict, which is the
    // most common failure mode for the smoke test.
    let _ = stop_named(backend, &container_name).await;

    let id = backend
        .create_container(&spec)
        .await
        .map_err(|e| anyhow::anyhow!("create_container {container_name}: {e}"))?;

    // Attach to declared networks. Compose-spec semantics: when a
    // service declares `networks: [a, b]`, the container should be on
    // `a` AND `b` (not the docker default bridge). The two backends
    // route this differently:
    //
    // - Bollard / dockerd: `create_container` only accepts one network
    //   via NetworkingConfig at create time; secondary networks need
    //   an explicit `connect_network` afterward. Disconnecting from
    //   `bridge` matches `docker compose up` (containers with explicit
    //   `networks:` aren't on the default bridge).
    // - Wisp: every declared network is passed to `create_container`
    //   via `spec.networks` and attached during the pre-exec window.
    //   `connect_network` is not supported post-start (would mean
    //   rebuilding the netns), so we skip the loop entirely when the
    //   backend reports `supports_live_network_attach() == false`.
    if !svc.networks.is_empty() && backend.supports_live_network_attach() {
        // best-effort: swallow disconnect failures (already off / not
        // applicable here).
        let _ = backend.disconnect_network(&id, "bridge").await;
        for net in &svc.networks {
            backend.connect_network(&id, net).await.map_err(|e| {
                anyhow::anyhow!(
                    "connect_network {net} -> {container_name}: {e} (does the network exist?)"
                )
            })?;
        }
    }

    backend
        .start_container(&id)
        .await
        .map_err(|e| anyhow::anyhow!("start_container {container_name}: {e}"))?;
    Ok(())
}

/// Parse a compose port string like `"8080:80"` / `"127.0.0.1:8080:80/tcp"`
/// / `"80"`. Returns `(host_port, container_port_with_proto)` or `None`
/// for unmappable specs. Used only by the helper that builds typed
/// [`PortSpec`] entries; the bollard backend re-derives bindings from
/// the typed shape.
fn parse_port_mapping(spec: &str) -> Option<(String, String)> {
    // Normalize: drop optional `host_ip:` prefix when present.
    let bare = match spec.matches(':').count() {
        0 => return None, // bare container port; skip
        1 => spec.to_string(),
        _ => {
            // ip:host:container: drop the ip. v0.3d ignores host_ip.
            spec.split_once(':').map(|(_ip, rest)| rest.to_string())?
        }
    };
    let (host, container) = bare.split_once(':')?;
    Some((host.to_string(), container.to_string()))
}

/// Internal helper: stop named.
async fn stop_named(backend: &dyn RuntimeBackend, name: &str) -> anyhow::Result<()> {
    // Inspect by name; if the runtime knows about it, stop + remove.
    // The trait's inspect_container accepts both the runtime ID and
    // the operator-chosen container name (bollard normalises
    // internally; wisp uses the name as the ID).
    match backend.inspect_container(name).await {
        Ok(Some(snap)) => stop_and_remove(backend, &snap.id).await,
        Ok(None) => Ok(()),
        Err(_) => Ok(()), // best-effort: not found
    }
}

/// Internal helper: stop and remove.
async fn stop_and_remove(backend: &dyn RuntimeBackend, container_id: &str) -> anyhow::Result<()> {
    // Best-effort stop with the legacy 10s timeout: matches the
    // earlier bollard invocation. remove with force=true matches
    // the legacy v=false / link=false / force=true shape.
    let _ = backend.stop_container(container_id, 10).await;
    match backend.remove_container(container_id, true).await {
        Ok(()) => Ok(()),
        Err(crate::runtime::RuntimeError::NotFound(_)) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("remove_container {container_id}: {e}")),
    }
}

/// Convenience: same as [`reconcile_stack`] but the caller already has
/// an `Arc<dyn RuntimeBackend>` (matches the agent's main loop).
pub async fn reconcile_stack_arc(
    backend: Arc<dyn RuntimeBackend>,
    stack_name: &str,
    compose_yaml: &str,
) -> anyhow::Result<(ReconcilePlan, Vec<ApplyOutcome>)> {
    reconcile_stack(backend.as_ref(), stack_name, compose_yaml).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_basic_mapping() {
        assert_eq!(
            parse_port_mapping("8080:80"),
            Some(("8080".into(), "80".into()))
        );
    }

    #[test]
    fn parse_port_with_host_ip_drops_ip() {
        assert_eq!(
            parse_port_mapping("127.0.0.1:8080:80/tcp"),
            Some(("8080".into(), "80/tcp".into()))
        );
    }

    #[test]
    fn parse_port_bare_returns_none() {
        assert_eq!(parse_port_mapping("80"), None);
    }

    #[test]
    fn container_name_for_uses_default_when_unset() {
        let svc = DesiredService {
            name: "web".into(),
            ..Default::default()
        };
        assert_eq!(container_name_for("hello", "web", &svc), "hello-web");
    }

    #[test]
    fn container_name_for_honours_override() {
        let svc = DesiredService {
            name: "web".into(),
            container_name: Some("my-web".into()),
            ..Default::default()
        };
        assert_eq!(container_name_for("hello", "web", &svc), "my-web");
    }

    // ----- desired_service_to_create_spec golden tests -----

    #[test]
    fn desired_service_to_create_spec_minimal_image_only() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx:1.27".into()),
            ..Default::default()
        };
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(spec.container_name, "hello-web");
        assert_eq!(spec.image, "nginx:1.27");
        assert_eq!(spec.stack, "hello");
        assert_eq!(spec.service, "web");
        assert!(matches!(spec.restart, SpecRestartPolicy::No));
        assert!(spec.command.is_none());
        assert!(spec.entrypoint.is_none());
        assert!(spec.ports.is_empty());
        assert!(spec.mounts.is_empty());
        assert!(spec.secrets.is_empty());
        // Compose / isengard labels are auto-injected.
        assert_eq!(spec.labels["com.docker.compose.project"], "hello");
        assert_eq!(spec.labels["com.docker.compose.service"], "web");
        assert_eq!(spec.labels["isengard.stack"], "hello");
    }

    #[test]
    fn desired_service_to_create_spec_honours_container_name_override() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            container_name: Some("my-web".into()),
            ..Default::default()
        };
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(spec.container_name, "my-web");
    }

    #[test]
    fn desired_service_to_create_spec_passes_environment_through() {
        let mut svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        svc.environment.insert("FOO".into(), "bar".into());
        svc.environment.insert("TZ".into(), "UTC".into());
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(spec.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(spec.env.get("TZ").map(String::as_str), Some("UTC"));
    }

    #[test]
    fn desired_service_to_create_spec_translates_restart_strings() {
        for (input, expect_always) in [
            ("always", true),
            ("unless-stopped", false),
            ("on-failure", false),
            ("no", false),
        ] {
            let svc = DesiredService {
                name: "web".into(),
                image: Some("nginx".into()),
                restart: Some(input.into()),
                ..Default::default()
            };
            let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
            match (input, &spec.restart) {
                ("always", SpecRestartPolicy::Always) => assert!(expect_always),
                ("unless-stopped", SpecRestartPolicy::UnlessStopped) => {}
                ("on-failure", SpecRestartPolicy::OnFailure { max_retries: None }) => {}
                ("no", SpecRestartPolicy::No) => {}
                (i, p) => panic!("unexpected restart mapping: {i} -> {p:?}"),
            }
        }
    }

    #[test]
    fn desired_service_to_create_spec_parses_port_strings() {
        let mut svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        svc.ports.push("8080:80".into());
        svc.ports.push("127.0.0.1:9090:90/udp".into());
        // Bare ports (compose expose-only) are skipped.
        svc.ports.push("100".into());
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(spec.ports.len(), 2);
        assert_eq!(spec.ports[0].host_port, 8080);
        assert_eq!(spec.ports[0].container_port, 80);
        assert!(matches!(spec.ports[0].protocol, PortProtocol::Tcp));
        assert!(spec.ports[0].host_ip.is_none());
        assert_eq!(spec.ports[1].host_port, 9090);
        assert_eq!(spec.ports[1].container_port, 90);
        assert!(matches!(spec.ports[1].protocol, PortProtocol::Udp));
        assert_eq!(
            spec.ports[1].host_ip.unwrap().to_string(),
            "127.0.0.1".to_string()
        );
    }

    #[test]
    fn desired_service_to_create_spec_splits_bind_strings_into_mounts() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        let binds = vec![
            "/srv/data:/data".to_string(),
            "/run/isengard-secrets/hello-web/cf:/run/secrets/cf:ro".to_string(),
        ];
        let spec = desired_service_to_create_spec("hello", &svc, &binds, Vec::new());
        assert_eq!(spec.mounts.len(), 2);
        assert_eq!(spec.mounts[0].source, "/srv/data");
        assert_eq!(spec.mounts[0].target, "/data");
        assert!(!spec.mounts[0].read_only);
        assert!(matches!(spec.mounts[0].kind, MountKind::Bind));
        assert!(spec.mounts[1].read_only);
        assert_eq!(spec.mounts[1].target, "/run/secrets/cf");
    }

    #[test]
    fn desired_service_to_create_spec_threads_secret_mounts() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        let secrets = vec![SecretMount {
            source: "/run/isengard-secrets/hello-web/cf".into(),
            target: std::path::PathBuf::from("/run/secrets/cf"),
            mode: 0o400,
        }];
        let spec = desired_service_to_create_spec("hello", &svc, &[], secrets.clone());
        assert_eq!(spec.secrets.len(), 1);
        assert_eq!(spec.secrets[0].source, secrets[0].source);
    }

    #[test]
    fn desired_service_to_create_spec_preserves_user_labels_then_adds_managed_ones() {
        let mut svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        svc.labels.insert("traefik.enable".into(), "true".into());
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(spec.labels["traefik.enable"], "true");
        assert_eq!(spec.labels["isengard.stack"], "hello");
    }

    #[test]
    fn desired_service_to_create_spec_passes_through_networks() {
        let mut svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        svc.networks.push("isengard-proxy".into());
        svc.networks.push("hello_default".into());
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(spec.networks, svc.networks);
    }

    #[test]
    fn desired_service_to_create_spec_lifts_cap_add_into_label() {
        // compose `cap_add:` flows into the `isengard.cap.add` label;
        // the bollard backend later reads it back into HostConfig.CapAdd
        // at container-create time.
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx:alpine".into()),
            cap_add: vec![
                "CHOWN".into(),
                "SETUID".into(),
                "SETGID".into(),
                "DAC_OVERRIDE".into(),
                "FOWNER".into(),
                "SETPCAP".into(),
            ],
            ..Default::default()
        };
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert_eq!(
            spec.labels.get("isengard.cap.add").map(String::as_str),
            Some("CHOWN,SETUID,SETGID,DAC_OVERRIDE,FOWNER,SETPCAP"),
        );
    }

    #[test]
    fn desired_service_to_create_spec_omits_cap_label_when_cap_add_empty() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        let spec = desired_service_to_create_spec("hello", &svc, &[], Vec::new());
        assert!(!spec.labels.contains_key("isengard.cap.add"));
    }

    #[test]
    fn parse_bind_string_two_components_no_options() {
        let m = parse_bind_string("/srv:/data").unwrap();
        assert_eq!(m.source, "/srv");
        assert_eq!(m.target, "/data");
        assert!(!m.read_only);
    }

    #[test]
    fn parse_bind_string_three_components_ro() {
        let m = parse_bind_string("/srv:/data:ro").unwrap();
        assert!(m.read_only);
    }

    #[test]
    fn parse_bind_string_invalid_returns_none() {
        assert!(parse_bind_string("/srv").is_none());
        assert!(parse_bind_string("a:b:c:d").is_none());
    }

    // ----- follow-up: merge_secret_targets -----

    /// Build a [`DesiredCompose`] with one service and an optional set
    /// of top-level external secrets + per-service refs.
    fn desired_with_service_secrets(
        service: &str,
        top_level: &[&str],
        per_service: &[(&str, Option<&str>)],
    ) -> DesiredCompose {
        use crate::compose_reconciler::{ServiceSecretRef, TopLevelSecret};
        let mut d = DesiredCompose::default();
        let mut svc = DesiredService {
            name: service.into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        for (src, tgt) in per_service {
            svc.secrets.push(ServiceSecretRef {
                source: (*src).to_string(),
                target: tgt.map(|s| s.to_string()),
            });
        }
        d.services.insert(service.into(), svc);
        for name in top_level {
            d.secrets
                .insert((*name).to_string(), TopLevelSecret::External);
        }
        d
    }

    #[test]
    fn merge_secret_targets_only_stack_level_when_service_has_no_secrets() {
        let d = desired_with_service_secrets("web", &[], &[]);
        let stack = vec!["cf_dns_token".to_string(), "github_token".to_string()];
        let out = merge_secret_targets(&d, "web", &stack).unwrap();
        assert_eq!(
            out,
            vec![
                (
                    "cf_dns_token".to_string(),
                    "/run/secrets/cf_dns_token".to_string()
                ),
                (
                    "github_token".to_string(),
                    "/run/secrets/github_token".to_string()
                ),
            ]
        );
    }

    #[test]
    fn merge_secret_targets_only_per_service_when_stack_empty() {
        // Service `web` references `db_pass` with a custom target.
        let d =
            desired_with_service_secrets("web", &["db_pass"], &[("db_pass", Some("/etc/db.pass"))]);
        let out = merge_secret_targets(&d, "web", &[]).unwrap();
        assert_eq!(
            out,
            vec![("db_pass".to_string(), "/etc/db.pass".to_string())]
        );
    }

    #[test]
    fn merge_secret_targets_unions_disjoint_lists() {
        // Service has `a` per-service; stack has `b`. Both should mount.
        let d = desired_with_service_secrets("web", &["a"], &[("a", None)]);
        let stack = vec!["b".to_string()];
        let out = merge_secret_targets(&d, "web", &stack).unwrap();
        assert_eq!(
            out,
            vec![
                ("a".to_string(), "/run/secrets/a".to_string()),
                ("b".to_string(), "/run/secrets/b".to_string()),
            ]
        );
    }

    #[test]
    fn merge_secret_targets_dedups_same_name_per_service_wins() {
        // Per-service entry for `foo` declares a custom `target:`;
        // stack-level also lists `foo`. Result: one entry, custom target.
        let d = desired_with_service_secrets("web", &["foo"], &[("foo", Some("/etc/foo"))]);
        let stack = vec!["foo".to_string()];
        let out = merge_secret_targets(&d, "web", &stack).unwrap();
        assert_eq!(out, vec![("foo".to_string(), "/etc/foo".to_string())]);
    }

    #[test]
    fn merge_secret_targets_dedups_same_name_default_target() {
        // Per-service entry without `target:` for `foo`, plus stack-level
        // `foo`. Still one entry, default `/run/secrets/foo`.
        let d = desired_with_service_secrets("web", &["foo"], &[("foo", None)]);
        let stack = vec!["foo".to_string()];
        let out = merge_secret_targets(&d, "web", &stack).unwrap();
        assert_eq!(
            out,
            vec![("foo".to_string(), "/run/secrets/foo".to_string())]
        );
    }

    #[test]
    fn merge_secret_targets_empty_returns_empty() {
        let d = desired_with_service_secrets("web", &[], &[]);
        let out = merge_secret_targets(&d, "web", &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn merge_secret_targets_stack_only_does_not_require_top_level_secrets_block() {
        // The compose YAML may have NO top-level `secrets:` (no
        // per-service refs). Stack-level `secrets = [...]` should still
        // produce mount entries: service_secret_targets returns empty
        // when the service has no `secrets:` of its own.
        let d = DesiredCompose {
            services: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "web".to_string(),
                    DesiredService {
                        name: "web".into(),
                        image: Some("nginx".into()),
                        ..Default::default()
                    },
                );
                m
            },
            secrets: std::collections::BTreeMap::new(),
            name: None,
        };
        let stack = vec!["fleet_secret".to_string()];
        let out = merge_secret_targets(&d, "web", &stack).unwrap();
        assert_eq!(
            out,
            vec![(
                "fleet_secret".to_string(),
                "/run/secrets/fleet_secret".to_string()
            )]
        );
    }

    #[test]
    fn merge_secret_targets_preserves_per_service_order_then_stack_order() {
        let d = desired_with_service_secrets(
            "web",
            &["alpha", "beta"],
            &[("alpha", None), ("beta", None)],
        );
        let stack = vec!["gamma".to_string(), "delta".to_string()];
        let out = merge_secret_targets(&d, "web", &stack).unwrap();
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma", "delta"]);
    }

    // ----- parallel apply_plan -----

    use crate::runtime::{
        ContainerSnapshot, ContainerState, HealthState, HealthcheckSpec, LogChunk, LogOptions,
        NetworkSettings, RestartPolicy, RuntimeError, RuntimeEvent,
    };
    use async_trait::async_trait;
    use futures_util::Stream;
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    /// Mock backend that drives `apply_plan` end-to-end. Tracks how many
    /// concurrent `create_container` calls were in flight at peak so a
    /// parallelism test can assert the fan-out actually happened.
    #[derive(Debug)]
    struct ParallelMock {
        /// Per-service create delay; missing entries return immediately.
        delays: BTreeMap<String, Duration>,
        /// Per-service hard failure: when the service name appears, the
        /// create_container call returns an Err carrying the canned msg.
        fail_create: BTreeMap<String, String>,
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
        /// Distinct network names the pre-pass touched.
        ensured_networks: std::sync::Mutex<Vec<String>>,
    }

    impl ParallelMock {
        fn new() -> Self {
            Self {
                delays: BTreeMap::new(),
                fail_create: BTreeMap::new(),
                in_flight: AtomicUsize::new(0),
                peak_in_flight: AtomicUsize::new(0),
                ensured_networks: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl RuntimeBackend for ParallelMock {
        async fn ensure_image(&self, _r: &str) -> Result<String, RuntimeError> {
            Ok(String::new())
        }
        async fn create_container(
            &self,
            spec: &crate::runtime::ContainerCreateSpec,
        ) -> Result<String, RuntimeError> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            // Bump peak under a relaxed fetch_max compatibility loop.
            let mut peak = self.peak_in_flight.load(Ordering::SeqCst);
            while now > peak {
                match self.peak_in_flight.compare_exchange(
                    peak,
                    now,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(curr) => peak = curr,
                }
            }
            if let Some(d) = self.delays.get(&spec.service) {
                tokio::time::sleep(*d).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if let Some(msg) = self.fail_create.get(&spec.service) {
                return Err(RuntimeError::Container(msg.clone()));
            }
            Ok(spec.container_name.clone())
        }
        async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn stop_container(&self, _id: &str, _t: u32) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn remove_container(&self, _id: &str, _force: bool) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn list_containers(
            &self,
            _f: ListFilter,
        ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
            Ok(Vec::new())
        }
        async fn inspect_container(
            &self,
            _id: &str,
        ) -> Result<Option<ContainerSnapshot>, RuntimeError> {
            Ok(None)
        }
        async fn connect_network(&self, _c: &str, _n: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn disconnect_network(&self, _c: &str, _n: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn ensure_network(&self, network: &str) -> Result<(), RuntimeError> {
            self.ensured_networks
                .lock()
                .expect("ensured_networks poisoned")
                .push(network.to_string());
            Ok(())
        }
        fn stream_logs(
            &self,
            _id: &str,
            _opts: LogOptions,
        ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
            Box::pin(futures_util::stream::empty())
        }
        fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>> {
            Box::pin(futures_util::stream::empty())
        }
        async fn run_healthcheck(
            &self,
            _id: &str,
            _hc: &HealthcheckSpec,
        ) -> Result<HealthState, RuntimeError> {
            Ok(HealthState::Healthy)
        }
        fn name(&self) -> &'static str {
            "parallel-mock"
        }
    }

    /// Build a [`DesiredCompose`] with N services named `svc-1..=svc-N`,
    /// each with the same image. Each service attaches to `network`.
    fn desired_with_n_services(n: usize, network: &str) -> DesiredCompose {
        let mut d = DesiredCompose::default();
        for i in 1..=n {
            let name = format!("svc-{i}");
            let mut svc = DesiredService {
                name: name.clone(),
                image: Some("nginx:1.27".into()),
                ..Default::default()
            };
            svc.networks.push(network.to_string());
            d.services.insert(name, svc);
        }
        d
    }

    /// Build a Start-only plan covering every service in `desired`.
    fn start_plan(stack: &str, desired: &DesiredCompose) -> ReconcilePlan {
        let ops = desired
            .services
            .values()
            .map(|svc| ServiceOp::Start {
                service: svc.name.clone(),
                image: svc.image.clone().unwrap_or_default(),
            })
            .collect();
        ReconcilePlan {
            stack: stack.to_string(),
            ops,
        }
    }

    #[tokio::test]
    async fn apply_plan_parallel_all_succeed_preserves_declaration_order() {
        let desired = desired_with_n_services(4, "appnet");
        let plan = start_plan("stack", &desired);
        let backend = ParallelMock::new();
        let outcomes = apply_plan(&backend, "stack", &desired, &plan, None, &[]).await;
        assert_eq!(outcomes.len(), 4);
        // Outcomes must match plan.ops index-for-index even though the
        // futures completed concurrently. ServiceOp's PartialEq is
        // derived; equal ops match by service name + image.
        for (i, oc) in outcomes.iter().enumerate() {
            assert_eq!(oc.op, plan.ops[i], "outcome {i} op mismatch");
            assert!(oc.error.is_none(), "outcome {i} unexpectedly errored");
        }
        // Pre-pass walked the (single) distinct network exactly once.
        let nets = backend.ensured_networks.lock().unwrap().clone();
        assert_eq!(nets, vec!["appnet".to_string()]);
    }

    #[tokio::test]
    async fn apply_plan_parallel_mixed_failures_all_outcomes_present() {
        let desired = desired_with_n_services(4, "appnet");
        let plan = start_plan("stack", &desired);
        let mut backend = ParallelMock::new();
        backend
            .fail_create
            .insert("svc-2".into(), "boom svc-2".into());
        backend
            .fail_create
            .insert("svc-4".into(), "boom svc-4".into());
        let outcomes = apply_plan(&backend, "stack", &desired, &plan, None, &[]).await;
        assert_eq!(outcomes.len(), 4);
        // Outcomes still aligned to plan order; the failing services
        // surface their errors verbatim while the others succeed.
        assert!(outcomes[0].error.is_none());
        assert!(
            outcomes[1]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("boom svc-2")
        );
        assert!(outcomes[2].error.is_none());
        assert!(
            outcomes[3]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("boom svc-4")
        );
    }

    #[tokio::test]
    async fn apply_plan_parallel_dedups_network_pre_pass() {
        let mut desired = desired_with_n_services(4, "appnet");
        // Add a second network to svc-2 and svc-3 so the pre-pass sees
        // two distinct names across the plan.
        desired
            .services
            .get_mut("svc-2")
            .unwrap()
            .networks
            .push("dbnet".into());
        desired
            .services
            .get_mut("svc-3")
            .unwrap()
            .networks
            .push("dbnet".into());
        let plan = start_plan("stack", &desired);
        let backend = ParallelMock::new();
        let _ = apply_plan(&backend, "stack", &desired, &plan, None, &[]).await;
        let nets = backend.ensured_networks.lock().unwrap().clone();
        // BTreeSet collection gives sorted unique order.
        assert_eq!(nets, vec!["appnet".to_string(), "dbnet".to_string()]);
    }

    /// Wall-clock test: with N=4 ops each sleeping 1s, the parallel
    /// implementation should finish in well under 4s (single-pass
    /// concurrency = 8 covers all 4). The earlier sequential
    /// loop would take ~4s. Marked `#[ignore]` so it doesn't slow the
    /// regular `cargo test` run; opt in via `cargo test ...
    /// apply_plan_parallel_wall_clock_under_budget -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn apply_plan_parallel_wall_clock_under_budget() {
        let desired = desired_with_n_services(4, "appnet");
        let plan = start_plan("stack", &desired);
        let mut backend = ParallelMock::new();
        for i in 1..=4 {
            backend
                .delays
                .insert(format!("svc-{i}"), Duration::from_secs(1));
        }
        let start = std::time::Instant::now();
        let outcomes = apply_plan(&backend, "stack", &desired, &plan, None, &[]).await;
        let elapsed = start.elapsed();
        assert_eq!(outcomes.len(), 4);
        assert!(
            elapsed < Duration::from_millis(3500),
            "parallel apply_plan took {elapsed:?}, expected < 3.5s (was sequential?)",
        );
        // Peak in-flight should be > 1 if any concurrency happened.
        let peak = backend.peak_in_flight.load(Ordering::SeqCst);
        assert!(peak > 1, "expected peak in-flight > 1, got {peak}");
    }

    /// Silence unused-import warnings for items only the mock pulls in.
    #[allow(dead_code)]
    fn _suppress_unused() {
        let _ = ContainerState::Running;
        let _ = SystemTime::UNIX_EPOCH;
        let _ = RestartPolicy::No;
        let _: NetworkSettings = NetworkSettings::default();
    }
}
