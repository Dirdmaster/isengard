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
//! Phase 0.6 (wisp arc): rewritten to drive the [`crate::runtime::RuntimeBackend`]
//! trait instead of bollard directly. Bollard remains the default
//! backend; the byte-level Config the bollard backend hands to dockerd
//! still flows through `BollardBackend::spec_to_config`, so the v0.3d
//! reconcile bytes are unchanged. WispBackend gets the same trait
//! surface, which is what makes engine-end-to-end deploys work.

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
    pub op: ServiceOp,
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
    let outcomes = apply_plan(backend, stack_name, &desired, &plan, None).await;
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
    )
    .await;
    Ok((plan, outcomes))
}

/// List the running containers belonging to `stack_name`. Filters via
/// the trait's [`ListFilter`] (which encodes both
/// `com.docker.compose.project=<stack>` and
/// `isengard.stack=<stack>` for the bollard backend), then inspects
/// each handle to fill in env / ports / restart so the diff path has
/// what it needs. WispBackend's list response already carries those
/// fields from the persisted spec, so the inspect round-trip is cheap.
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

/// Apply each op in `plan`. Returns one outcome per op, in the same
/// order. Failures don't short-circuit: the operator gets the full
/// summary at the end.
///
/// `controller_endpoint`: when `Some`, services with `secrets:` entries
/// fetch each value over the controller's FetchSecret RPC, materialise
/// it on tmpfs, and bind-mount the directory into the container. When
/// `None`, secrets are silently dropped (matches the v0.3d call site).
pub async fn apply_plan(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    desired: &DesiredCompose,
    plan: &ReconcilePlan,
    controller_endpoint: Option<Arc<RwLock<Endpoint>>>,
) -> Vec<ApplyOutcome> {
    let mut outcomes = Vec::with_capacity(plan.ops.len());
    for op in &plan.ops {
        let result = match op {
            ServiceOp::NoChange { .. } => Ok(()),
            ServiceOp::Start { service, .. } => {
                let Some(svc) = desired.services.get(service) else {
                    outcomes.push(ApplyOutcome {
                        op: op.clone(),
                        error: Some(format!("service {service} missing from compose")),
                    });
                    continue;
                };
                ensure_container_started(
                    backend,
                    stack_name,
                    svc,
                    None,
                    desired,
                    controller_endpoint.clone(),
                )
                .await
            }
            ServiceOp::Recreate { service, .. } => {
                let Some(svc) = desired.services.get(service) else {
                    outcomes.push(ApplyOutcome {
                        op: op.clone(),
                        error: Some(format!("service {service} missing from compose")),
                    });
                    continue;
                };
                // Find the existing container by service name and stop+remove
                // before starting fresh. Best-effort: if the container is gone
                // we still attempt the start.
                let existing = list_running_for_stack(backend, stack_name)
                    .await
                    .unwrap_or_default();
                let prior = existing.iter().find(|r| &r.service_name == service);
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
                    controller_endpoint.clone(),
                )
                .await
            }
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
        outcomes.push(ApplyOutcome {
            op: op.clone(),
            error: result.err().map(|e| format!("{e:#}")),
        });
    }
    outcomes
}

/// Compute the container name the agent will use for this service.
/// Mirrors the logic in [`ensure_container_started`].
fn container_name_for(stack_name: &str, service: &str, svc: &DesiredService) -> String {
    svc.container_name
        .clone()
        .unwrap_or_else(|| format!("{stack_name}-{service}"))
}

/// Translate a parsed compose [`DesiredService`] into the
/// backend-agnostic [`ContainerCreateSpec`] every [`crate::runtime::RuntimeBackend`]
/// understands.
///
/// Phase 0.6 wisp arc: this is the inverse of
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

async fn ensure_container_started(
    backend: &dyn RuntimeBackend,
    stack_name: &str,
    svc: &DesiredService,
    _prior_container: Option<&str>,
    desired: &DesiredCompose,
    controller_endpoint: Option<Arc<RwLock<Endpoint>>>,
) -> anyhow::Result<()> {
    if svc.image.is_none() {
        return Err(anyhow::anyhow!("service {} has no image", svc.name));
    }
    let container_name = container_name_for(stack_name, &svc.name, svc);

    // v0.3.6: fetch + materialise external secrets BEFORE creating the
    // container so the bind-mount source paths exist when the backend
    // wires them in. On any failure we abort the start; the operator
    // gets the error in the apply outcome.
    let mut binds: Vec<String> = Vec::new();
    let mut secret_mounts: Vec<SecretMount> = Vec::new();
    if !svc.secrets.is_empty() {
        let Some(endpoint) = controller_endpoint.clone() else {
            return Err(anyhow::anyhow!(
                "service {} references secrets but the agent has no \
                 controller endpoint configured for FetchSecret",
                svc.name,
            ));
        };
        // Cleanup any leftover host directory before we re-fetch.
        let _ = secret_fetch::cleanup_for_container(&container_name);
        let targets = desired.service_secret_targets(&svc.name)?;
        let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
        let materialised = secret_fetch::fetch_and_materialise(endpoint, &container_name, &names)
            .await
            .map_err(|e| anyhow::anyhow!("fetch + materialise secrets for {}: {e}", svc.name))?;
        // Compose target paths from the long-form `target:` (or default
        // `/run/secrets/<name>`) and pair them up with the materialised
        // host paths. The order is stable: `service_secret_targets` and
        // `fetch_and_materialise` both walk the same `svc.secrets` list.
        //
        // Two parallel collections: `binds` are kept for backward
        // compatibility with the bollard call shape (the legacy
        // build_create_config consumed `host:container:ro` strings);
        // `secret_mounts` carries the structured form the trait
        // surface uses. BollardBackend's spec_to_config consumes
        // SecretMount entries directly.
        for (m, (_, target)) in materialised.iter().zip(targets.iter()) {
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
    // `a` AND `b` (not the docker default bridge). bollard's
    // create_container only takes one network at create-time via
    // NetworkingConfig; for multi-network containers the convention is
    // to connect each one explicitly afterward. Disconnecting from
    // `bridge` matches `docker compose up` (containers with explicit
    // `networks:` aren't on the default bridge).
    //
    // For wisp the trait routes the primary network through
    // create_container; connect_network for additional networks
    // currently errors out (live attach is a 0.5 stretch). For bollard
    // it's a noop dance for stack-only networks.
    if !svc.networks.is_empty() {
        // best-effort: swallow disconnect failures (already off / not
        // applicable to wisp).
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

async fn stop_and_remove(backend: &dyn RuntimeBackend, container_id: &str) -> anyhow::Result<()> {
    // Best-effort stop with the legacy 10s timeout: matches the
    // pre-Phase-0.6 bollard invocation. remove with force=true matches
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

    // ----- Phase 0.6: desired_service_to_create_spec golden tests -----

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
}
