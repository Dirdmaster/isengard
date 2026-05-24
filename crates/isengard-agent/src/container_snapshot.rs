//! Reads the local container list, groups containers into stacks
//! based on the `com.docker.compose.project` label (or `isengard.stack=` override),
//! and converts to the wire-format `StackInfo`.
//!
//! Drives off the [`crate::runtime::RuntimeBackend`] trait
//! so heartbeats from a wisp host stop dialling docker.sock every
//! interval. The legacy `list_container_snapshots()` function (no
//! backend arg) is kept for back-compat but routes through a
//! best-effort BollardBackend factory; new callers pass the live
//! backend Arc via [`list_container_snapshots_via`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use isengard_proto::pb::{ContainerInfo, ServiceInfo, StackInfo};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::runtime::{ContainerState, ListFilter, RuntimeBackend};

/// Lightweight snapshot of one container: the bits we need to derive
/// stacks, services, and (phase 0.18) container rows from a heartbeat.
///
/// Extended with `id`, `created_at`, and `exit_code` so the
/// agent can ship a `ContainerInfo` per container. The runtime id is
/// the backend's native handle (bollard container id, wisp handle).
/// Pre-0.18 callers that consume only `name`/`image`/`state`/`labels`
/// are unaffected.
///
/// 2026-05-23: widened with the rich-detail fields (`ports`, `env`,
/// `mounts`, `networks`, `restart_policy`, `command`, `entrypoint`,
/// `working_dir`, `user`, `healthcheck`) that the controller needs to
/// synthesize a `compose.yaml` for stacks the operator brought up
/// outside isengard. List-only paths (the docker-summary fallback)
/// leave these empty; the inspect-driven path
/// [`enrich_snapshot_from_inspect`] populates them from a bollard
/// `ContainerInspectResponse`.
#[derive(Debug, Clone, Default)]
pub struct ContainerSnapshot {
    /// Container name, leading slash already trimmed.
    pub name: String,
    /// Image reference as the runtime reports it.
    pub image: String,
    /// Lifecycle state string (`running`, `exited`, ...).
    pub state: String,
    /// Container labels.
    pub labels: HashMap<String, String>,
    /// Bollard / wisp native id. Empty when the legacy (no-backend)
    /// fallback path produced this snapshot from a bollard ContainerSummary
    /// without an id field.
    pub id: String,
    /// Unix milliseconds when the container was created. 0 when the
    /// runtime did not record a creation time.
    pub created_at_ms: i64,
    /// Exit code reported by the runtime (only set for `exited` /
    /// `dead`). None for everything else.
    pub exit_code: Option<i32>,
    /// Host -> container port mappings. Empty when the list path didn't
    /// inspect or no ports are published.
    pub ports: Vec<PortMapping>,
    /// Environment as `KEY=value` entries, as docker reports them.
    /// Empty when the runtime didn't expose env (list-only path).
    pub env: Vec<String>,
    /// Bind / named volume / tmpfs mounts attached to the container.
    pub mounts: Vec<MountSpec>,
    /// Network names the container is attached to.
    pub networks: Vec<String>,
    /// Effective restart policy string (`no`, `always`, `unless-stopped`,
    /// `on-failure`, `on-failure:N`). `None` when the runtime didn't
    /// record one or it's the default empty value.
    pub restart_policy: Option<String>,
    /// `cmd` override the container was created with. `None` keeps the
    /// image-baked command.
    pub command: Option<Vec<String>>,
    /// `entrypoint` override. `None` keeps the image-baked entrypoint.
    pub entrypoint: Option<Vec<String>>,
    /// Container working directory override, when set.
    pub working_dir: Option<String>,
    /// `USER` override, when set.
    pub user: Option<String>,
    /// Healthcheck declaration, when the container has one.
    pub healthcheck: Option<HealthcheckSpec>,
}

/// One host-side port -> container-side port mapping, including the
/// transport protocol. Mirrors the docker / compose `ports:` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    /// Host interface IP. Empty string when bound to all interfaces.
    pub host_ip: String,
    /// Host port number.
    pub host_port: u16,
    /// Container port number.
    pub container_port: u16,
    /// Transport protocol: `tcp`, `udp`, `sctp`.
    pub protocol: String,
}

/// One mount entry attached to a container. Distinct from the
/// runtime-side `crate::runtime::MountSpec` (which describes what the
/// agent wants to create); this shape describes what the runtime
/// reports as currently attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Mount kind: `bind`, `volume`, `tmpfs`.
    pub kind: String,
    /// Host-side path (bind), volume name (volume), or empty (tmpfs).
    pub source: String,
    /// In-container target path.
    pub target: String,
    /// Read-only when true.
    pub read_only: bool,
}

/// Container healthcheck as the runtime reports it. Distinct from the
/// runtime-side `crate::runtime::HealthcheckSpec` (which carries typed
/// `Duration` values for the create path); this shape mirrors the wire
/// vocabulary the synthesizer eventually emits and survives a JSON
/// round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthcheckSpec {
    /// Command argv. First element is typically `"CMD"` or `"CMD-SHELL"`.
    pub test: Vec<String>,
    /// Interval between probes in nanoseconds, as docker records it.
    /// 0 means "use the image default".
    pub interval_ns: i64,
    /// Per-probe timeout in nanoseconds. 0 means "use the image default".
    pub timeout_ns: i64,
    /// Consecutive failures before the container is marked unhealthy.
    /// 0 means "use the image default".
    pub retries: i64,
    /// Start grace period in nanoseconds. 0 means "use the image default".
    pub start_period_ns: i64,
}

/// Query the [`RuntimeBackend`] for all containers
/// (running + stopped) and project to the heartbeat-oriented
/// [`ContainerSnapshot`] shape. Returns an empty Vec on backend error
/// (logged at warn level so the heartbeat still sends).
pub async fn list_container_snapshots_via(backend: &dyn RuntimeBackend) -> Vec<ContainerSnapshot> {
    let snaps = match backend
        .list_containers(ListFilter {
            all: true,
            ..Default::default()
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, backend = %backend.name(), "container_snapshot: list_containers failed");
            return Vec::new();
        }
    };
    let docker = backend.as_bollard();
    let mut out = Vec::with_capacity(snaps.len());
    for listed in snaps {
        let mut snap = runtime_snapshot_to_heartbeat(listed);

        if let Some(docker) = docker.as_ref() {
            if !snap.id.is_empty() {
                match docker
                    .inspect_container(
                        &snap.id,
                        None::<bollard::container::InspectContainerOptions>,
                    )
                    .await
                {
                    Ok(inspect) => enrich_snapshot_from_inspect(&mut snap, &inspect),
                    Err(e) => {
                        warn!(error = %e, id = %snap.id, "container_snapshot: inspect_container failed");
                    }
                }
            }
        } else if !snap.id.is_empty() {
            match backend.inspect_container(&snap.id).await {
                Ok(Some(inspected)) => snap = runtime_snapshot_to_heartbeat(inspected),
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, id = %snap.id, backend = %backend.name(), "container_snapshot: inspect_container failed");
                }
            }
        }

        out.push(snap);
    }
    out
}

/// Convert a runtime snapshot into the heartbeat container snapshot shape.
fn runtime_snapshot_to_heartbeat(s: crate::runtime::ContainerSnapshot) -> ContainerSnapshot {
    let created_at_ms = s
        .created_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    ContainerSnapshot {
        name: s.name,
        image: s.image,
        state: state_to_str(s.state).to_string(),
        labels: s.labels.into_iter().collect(),
        id: s.id,
        created_at_ms,
        exit_code: s.exit_code,
        env: s.env.into_iter().map(|(k, v)| format!("{k}={v}")).collect(),
        networks: s.network_settings.ip_addresses.into_keys().collect(),
        ports: runtime_ports_to_heartbeat(s.network_settings.ports),
        restart_policy: s.restart,
        ..Default::default()
    }
}

/// Convert runtime-level host port bindings into heartbeat rich port mappings.
fn runtime_ports_to_heartbeat(
    ports: std::collections::BTreeMap<String, Vec<crate::runtime::HostPort>>,
) -> Vec<PortMapping> {
    let mut out = Vec::new();
    for (key, bindings) in ports {
        let (container_port, protocol) = parse_port_proto(&key);
        for binding in bindings {
            out.push(PortMapping {
                host_ip: binding.host_ip.to_string(),
                host_port: binding.host_port,
                container_port,
                protocol: protocol.to_string(),
            });
        }
    }
    out
}

/// Back-compat wrapper for callers that don't have a backend handle.
/// Equivalent to the pre-0.6 behavior: dial docker.sock every call,
/// return empty on connection failure. Heartbeat code paths now pass
/// the live backend via [`list_container_snapshots_via`]; this remains
/// for the few legacy / test paths that haven't been threaded through.
pub async fn list_container_snapshots() -> Vec<ContainerSnapshot> {
    use bollard::Docker;
    use bollard::container::ListContainersOptions;

    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "container_snapshot: failed to connect to Docker");
            return Vec::new();
        }
    };
    let opts = ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    };
    let containers = match docker.list_containers(Some(opts)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "container_snapshot: list_containers failed");
            return Vec::new();
        }
    };
    containers
        .into_iter()
        .map(|c| {
            let name = c
                .names
                .as_ref()
                .and_then(|ns| ns.first().cloned())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();
            let image = c.image.clone().unwrap_or_default();
            let state = c.state.clone().unwrap_or_else(|| "unknown".to_string());
            let labels = c.labels.unwrap_or_default();
            let id = c.id.clone().unwrap_or_default();
            // bollard reports `created` in unix seconds (i64). Convert
            // up to ms; treat negative values (clock skew) as 0.
            let created_at_ms = c.created.map(|s| s.max(0) * 1000).unwrap_or(0);
            ContainerSnapshot {
                name,
                image,
                state,
                labels,
                id,
                created_at_ms,
                exit_code: None,
                ..Default::default()
            }
        })
        .collect()
}

/// Map the trait's typed state enum into the wire-format state string the
/// controller persists via `ServiceState::from_str`.
///
/// v0.5.3: `Created` now reports as `"creating"` (was `"created"`), so
/// `ServiceState::from_str` lands on `ServiceState::Creating` rather than
/// the pre-extension `Unknown` fallback. `from_str` still accepts the
/// legacy `"created"` string so heartbeats from older agents stay green.
fn state_to_str(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Created => "creating",
        ContainerState::Running => "running",
        ContainerState::Restarting => "restarting",
        ContainerState::Paused => "paused",
        ContainerState::Exited => "stopped",
        ContainerState::Dead => "failed",
    }
}

/// Heartbeat hook that prefers the live backend when one is
/// available; falls back to the legacy bollard probe otherwise.
/// Callers pass the same `Option<Arc<dyn RuntimeBackend>>` they already
/// hold for the rest of the agent.
pub async fn snapshots_via_backend_or_legacy(
    backend: Option<&Arc<dyn RuntimeBackend>>,
) -> Vec<ContainerSnapshot> {
    match backend {
        Some(b) => list_container_snapshots_via(b.as_ref()).await,
        None => list_container_snapshots().await,
    }
}

/// Build per-container ServiceInfo entries with stack association.
/// Mirrors derive_stacks naming: same precedence (isengard.stack > compose label > inferred).
pub fn derive_services(containers: &[ContainerSnapshot]) -> Vec<ServiceInfo> {
    containers
        .iter()
        .map(|c| {
            let stack = c
                .labels
                .get("isengard.stack")
                .or_else(|| c.labels.get("com.docker.compose.project"))
                .cloned()
                .unwrap_or_else(|| c.name.clone());

            ServiceInfo {
                name: c.name.clone(),
                image: c.image.clone(),
                state: c.state.clone(),
                stack: Some(stack),
            }
        })
        .collect()
}

/// Group container snapshots into stacks based on Docker Compose labels,
/// the optional `isengard.stack` override, or fall back to single-service
/// inferred stacks.
pub fn derive_stacks(containers: &[ContainerSnapshot]) -> Vec<StackInfo> {
    // BTreeMap for deterministic output ordering (helps tests + diffs).
    let mut grouped: BTreeMap<(String, &'static str), Vec<String>> = BTreeMap::new();

    for c in containers {
        let (name, source) = if let Some(n) = c.labels.get("isengard.stack") {
            (n.clone(), "manual")
        } else if let Some(n) = c.labels.get("com.docker.compose.project") {
            (n.clone(), "compose")
        } else {
            (c.name.clone(), "inferred")
        };

        grouped
            .entry((name, source))
            .or_default()
            .push(c.name.clone());
    }

    grouped
        .into_iter()
        .map(|((name, source), mut services)| {
            services.sort();
            StackInfo {
                name,
                source: source.to_string(),
                services,
            }
        })
        .collect()
}

/// State vocabulary the agent emits in [`ContainerInfo`].
/// Distinct from [`state_to_str`] which targets the storage-side
/// `ServiceState` enum. The container vocabulary is fixed: `running`,
/// `restarting`, `paused`, `created`, `exited`, `dead`. `removing` is
/// not modelled by [`ContainerState`] today; the legacy fallback path
/// may produce arbitrary strings from bollard's `State` field, which
/// the controller treats as-is.
fn container_state_to_str(state: &str) -> &str {
    // The bollard-fallback path passes the raw State string (e.g.
    // `running`, `exited`, `dead`, `paused`, `removing`, `restarting`,
    // `created`). The backend-driven path passes one of the legacy
    // service strings (`creating`, `stopped`, `failed`). Normalise both
    // onto the vocabulary so the controller sees a single
    // dictionary.
    match state {
        "creating" => "created",
        "stopped" => "exited",
        "failed" => "dead",
        // running / restarting / paused / created / exited / dead /
        // removing pass through unchanged.
        other => other,
    }
}

/// Render the per-container `STATUS` column the way docker
/// ps does. Inputs are the container's state vocabulary (post
/// `container_state_to_str`), the unix-ms creation time (0 when the
/// runtime didn't record one), the optional exit code, and the
/// reference `now` (unix ms) used to compute "Up 5m".
pub fn render_status_message(
    state: &str,
    created_at_ms: i64,
    exit_code: Option<i32>,
    now_ms: i64,
) -> String {
    match state {
        "running" => format!("Up {}", humanize_age_ms(now_ms, created_at_ms)),
        "exited" => match exit_code {
            Some(code) => format!(
                "Exited ({code}) {} ago",
                humanize_age_ms(now_ms, created_at_ms)
            ),
            None => format!("Exited {} ago", humanize_age_ms(now_ms, created_at_ms)),
        },
        "paused" => "Paused".to_string(),
        "restarting" => "Restarting".to_string(),
        "created" => "Created".to_string(),
        "dead" => "Dead".to_string(),
        "removing" => "Removing".to_string(),
        // Anything else: pass the raw vocabulary through with a capital
        // letter. Defensive; the agent shouldn't emit anything else.
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Format `now_ms - then_ms` like `15s`, `5m`, `2h`, `3d`. Negative
/// deltas (the reference is BEFORE the event) collapse to `0s`.
fn humanize_age_ms(now_ms: i64, then_ms: i64) -> String {
    if then_ms <= 0 {
        // Unknown creation time: don't lie with a duration.
        return "?".to_string();
    }
    let delta_secs = ((now_ms - then_ms).max(0)) / 1000;
    if delta_secs < 60 {
        format!("{delta_secs}s")
    } else if delta_secs < 3600 {
        format!("{}m", delta_secs / 60)
    } else if delta_secs < 86_400 {
        format!("{}h", delta_secs / 3600)
    } else {
        format!("{}d", delta_secs / 86_400)
    }
}

/// Project a slice of snapshots to the wire-format
/// [`ContainerInfo`] vec carried on every heartbeat. Each row gets its
/// `observed_at_ms` stamped from `now_ms` so the controller's
/// `last_seen_at` derivation can clamp to the agent's clock.
///
/// Containers without a runtime id are skipped: without a stable id the
/// controller would mint a synthetic operator id every heartbeat,
/// defeating the deduplication. This only happens on the legacy
/// no-backend bollard fallback path which doesn't carry container ids.
pub fn derive_containers(snapshots: &[ContainerSnapshot], now_ms: i64) -> Vec<ContainerInfo> {
    snapshots
        .iter()
        .filter(|s| !s.id.is_empty())
        .map(|s| {
            let stack = s
                .labels
                .get("isengard.stack")
                .or_else(|| s.labels.get("com.docker.compose.project"))
                .cloned()
                .unwrap_or_default();
            let service = s
                .labels
                .get("com.docker.compose.service")
                .cloned()
                .unwrap_or_default();
            let state = container_state_to_str(&s.state).to_string();
            let status_message =
                render_status_message(&state, s.created_at_ms, s.exit_code, now_ms);
            let rich = build_rich(s);
            ContainerInfo {
                runtime_container_id: s.id.clone(),
                image: s.image.clone(),
                command: String::new(),
                state,
                status_message,
                names: s.name.clone(),
                stack,
                service,
                created_at_ms: s.created_at_ms,
                observed_at_ms: now_ms,
                rich,
            }
        })
        .collect()
}

/// Translate the agent-side rich fields on `ContainerSnapshot` into the
/// wire-format `ContainerRich`. Returns `None` when the snapshot has no
/// rich data (list-only path / legacy fallback), so the controller sees
/// `rich = None` and skips the containers_rich upsert.
fn build_rich(s: &ContainerSnapshot) -> Option<isengard_proto::pb::ContainerRich> {
    use isengard_proto::pb::{
        ContainerHealthcheck as ProtoHc, ContainerMount as ProtoMount,
        ContainerPortMapping as ProtoPort, ContainerRich,
    };
    // "No rich data at all" check: nothing inspect-driven was filled in.
    let empty = s.ports.is_empty()
        && s.env.is_empty()
        && s.mounts.is_empty()
        && s.networks.is_empty()
        && s.restart_policy.is_none()
        && s.command.is_none()
        && s.entrypoint.is_none()
        && s.working_dir.is_none()
        && s.user.is_none()
        && s.healthcheck.is_none();
    if empty {
        return None;
    }
    Some(ContainerRich {
        ports: s
            .ports
            .iter()
            .map(|p| ProtoPort {
                host_ip: p.host_ip.clone(),
                host_port: u32::from(p.host_port),
                container_port: u32::from(p.container_port),
                protocol: p.protocol.clone(),
            })
            .collect(),
        env: s.env.clone(),
        mounts: s
            .mounts
            .iter()
            .map(|m| ProtoMount {
                kind: m.kind.clone(),
                source: m.source.clone(),
                target: m.target.clone(),
                read_only: m.read_only,
            })
            .collect(),
        networks: s.networks.clone(),
        restart_policy: s.restart_policy.clone().unwrap_or_default(),
        command: s.command.clone().unwrap_or_default(),
        entrypoint: s.entrypoint.clone().unwrap_or_default(),
        working_dir: s.working_dir.clone().unwrap_or_default(),
        user_spec: s.user.clone().unwrap_or_default(),
        healthcheck: s.healthcheck.as_ref().map(|hc| ProtoHc {
            test: hc.test.clone(),
            interval_ns: hc.interval_ns,
            timeout_ns: hc.timeout_ns,
            retries: hc.retries,
            start_period_ns: hc.start_period_ns,
        }),
    })
}

/// Convenience: stamp `observed_at_ms` from the system clock. Splits
/// the wall-clock call from [`derive_containers`] so tests can inject a
/// deterministic `now_ms`.
pub fn derive_containers_now(snapshots: &[ContainerSnapshot]) -> Vec<ContainerInfo> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    derive_containers(snapshots, now_ms)
}

/// Populate the rich-detail fields (`ports`, `env`, `mounts`,
/// `networks`, `restart_policy`, `command`, `entrypoint`,
/// `working_dir`, `user`, `healthcheck`) on a snapshot from the bollard
/// `ContainerInspectResponse` for the same container.
///
/// The agent's list path returns only the heartbeat-required bits;
/// `compose synthesize` on the controller needs the full inspect view.
/// Callers per heartbeat: fetch the snapshot via the trait, inspect once
/// per container, hand both into this helper.
///
/// Tolerant of missing fields: a half-populated inspect response just
/// leaves the corresponding snapshot field empty / `None`.
pub fn enrich_snapshot_from_inspect(
    snap: &mut ContainerSnapshot,
    inspect: &bollard::secret::ContainerInspectResponse,
) {
    let cfg = inspect.config.as_ref();
    let host_cfg = inspect.host_config.as_ref();
    let net = inspect.network_settings.as_ref();

    // Env: pass through verbatim (KEY=value entries).
    snap.env = cfg.and_then(|c| c.env.clone()).unwrap_or_default();

    // Command / entrypoint: pass through as-is (None when image default).
    snap.command = cfg.and_then(|c| c.cmd.clone());
    snap.entrypoint = cfg.and_then(|c| c.entrypoint.clone());
    snap.working_dir = cfg
        .and_then(|c| c.working_dir.clone())
        .filter(|s| !s.is_empty());
    snap.user = cfg.and_then(|c| c.user.clone()).filter(|s| !s.is_empty());

    // Healthcheck: every duration is in nanoseconds in the docker API.
    snap.healthcheck = cfg.and_then(|c| c.healthcheck.as_ref()).and_then(|hc| {
        let test = hc.test.clone().unwrap_or_default();
        if test.is_empty() {
            // Bollard yields Test=["NONE"] when the operator explicitly
            // disabled the healthcheck; preserve that signal.
            return None;
        }
        Some(HealthcheckSpec {
            test,
            interval_ns: hc.interval.unwrap_or(0),
            timeout_ns: hc.timeout.unwrap_or(0),
            retries: hc.retries.unwrap_or(0),
            start_period_ns: hc.start_period.unwrap_or(0),
        })
    });

    // Restart policy: docker reports it as a name + optional retry
    // count. Map to the compose-style string the controller stores
    // (`always`, `unless-stopped`, `no`, `on-failure[:N]`).
    snap.restart_policy = host_cfg
        .and_then(|h| h.restart_policy.as_ref())
        .and_then(|rp| rp.name.map(|n| (n, rp.maximum_retry_count)))
        .and_then(|(name, retries)| match name {
            bollard::secret::RestartPolicyNameEnum::ALWAYS => Some("always".to_string()),
            bollard::secret::RestartPolicyNameEnum::UNLESS_STOPPED => {
                Some("unless-stopped".to_string())
            }
            bollard::secret::RestartPolicyNameEnum::NO => Some("no".to_string()),
            bollard::secret::RestartPolicyNameEnum::ON_FAILURE => match retries {
                Some(n) if n > 0 => Some(format!("on-failure:{n}")),
                _ => Some("on-failure".to_string()),
            },
            bollard::secret::RestartPolicyNameEnum::EMPTY => None,
        });

    // Networks: prefer NetworkSettings.networks (covers attached-after-
    // create networks). Fall back to HostConfig.network_mode when no
    // explicit attachments are recorded.
    snap.networks = if let Some(nets) = net.and_then(|n| n.networks.as_ref()) {
        let mut names: Vec<String> = nets.keys().cloned().collect();
        names.sort();
        names
    } else if let Some(mode) = host_cfg.and_then(|h| h.network_mode.clone()) {
        if mode.is_empty() {
            Vec::new()
        } else {
            vec![mode]
        }
    } else {
        Vec::new()
    };

    // Ports: walk HostConfig.port_bindings to capture the host:container
    // mapping. NetworkSettings.ports also covers it but only for the
    // bindings that actually got published; HostConfig is the operator's
    // declared intent.
    snap.ports = Vec::new();
    if let Some(bindings) = host_cfg.and_then(|h| h.port_bindings.as_ref()) {
        let mut keys: Vec<&String> = bindings.keys().collect();
        keys.sort();
        for cport in keys {
            let (container_port, protocol) = parse_port_proto(cport);
            let Some(Some(binds)) = bindings.get(cport).map(|v| v.as_ref()) else {
                continue;
            };
            for b in binds {
                let host_port = b
                    .host_port
                    .as_deref()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(0);
                let host_ip = b.host_ip.clone().unwrap_or_default();
                snap.ports.push(PortMapping {
                    host_ip,
                    host_port,
                    container_port,
                    protocol: protocol.to_string(),
                });
            }
        }
    }

    // Mounts: walk inspect.mounts (the runtime-attached list). Distinct
    // from HostConfig.binds (which is the operator's intent string).
    snap.mounts = Vec::new();
    if let Some(mounts) = inspect.mounts.as_ref() {
        for m in mounts {
            let kind = m
                .typ
                .as_ref()
                .map(|t| match t {
                    bollard::secret::MountPointTypeEnum::BIND => "bind",
                    bollard::secret::MountPointTypeEnum::VOLUME => "volume",
                    bollard::secret::MountPointTypeEnum::TMPFS => "tmpfs",
                    bollard::secret::MountPointTypeEnum::NPIPE => "npipe",
                    bollard::secret::MountPointTypeEnum::CLUSTER => "cluster",
                    bollard::secret::MountPointTypeEnum::EMPTY => "bind",
                })
                .unwrap_or("bind")
                .to_string();
            let source = match kind.as_str() {
                "volume" => m.name.clone().unwrap_or_default(),
                _ => m.source.clone().unwrap_or_default(),
            };
            snap.mounts.push(MountSpec {
                kind,
                source,
                target: m.destination.clone().unwrap_or_default(),
                read_only: !m.rw.unwrap_or(true),
            });
        }
    }
}

/// Parse a bollard port-binding key like `"80/tcp"` into `(80, "tcp")`.
/// Keys without a slash collapse to `(<port>, "tcp")`.
fn parse_port_proto(key: &str) -> (u16, &'static str) {
    let (port_str, proto) = match key.split_once('/') {
        Some((p, "udp")) => (p, "udp"),
        Some((p, "sctp")) => (p, "sctp"),
        Some((p, _)) => (p, "tcp"),
        None => (key, "tcp"),
    };
    let port = port_str.parse::<u16>().unwrap_or(0);
    (port, proto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::pin::Pin;

    use futures_util::Stream;

    use crate::runtime::{
        self, ContainerCreateSpec, HealthState, HostPort, LogChunk, LogOptions, NetworkSettings,
        RuntimeError, RuntimeEvent,
    };

    /// v0.5.3: `state_to_str` now maps `Created` -> `"creating"` and
    /// `Exited` / `Dead` -> `"stopped"` / `"failed"` so the controller's
    /// `ServiceState::from_str` lands on the matching variant instead of
    /// `Unknown`. Backstops the bug fix that surfaced live on the
    /// lausanne v0.5.1 deploy (4/8 services rendering `unknown`).
    #[test]
    fn state_to_str_maps_wisp_created_to_creating() {
        assert_eq!(state_to_str(ContainerState::Created), "creating");
        assert_eq!(state_to_str(ContainerState::Running), "running");
        assert_eq!(state_to_str(ContainerState::Restarting), "restarting");
        assert_eq!(state_to_str(ContainerState::Paused), "paused");
        assert_eq!(state_to_str(ContainerState::Exited), "stopped");
        assert_eq!(state_to_str(ContainerState::Dead), "failed");
    }

    /// The runtime-level mapping must compose with the storage-level
    /// `ServiceState::from_str` so that no runtime state ends up as
    /// `Unknown` at the controller boundary.
    #[test]
    fn agent_state_str_decodes_into_concrete_service_state() {
        use isengard_storage::ServiceState;
        for cs in [
            ContainerState::Created,
            ContainerState::Running,
            ContainerState::Restarting,
            ContainerState::Exited,
            ContainerState::Dead,
        ] {
            let s = state_to_str(cs);
            let decoded = ServiceState::from_str(s);
            assert_ne!(
                decoded,
                ServiceState::Unknown,
                "ContainerState::{cs:?} -> {s:?} -> Unknown (regression)"
            );
        }
    }

    #[derive(Debug)]
    struct InspectingBackend {
        listed: Vec<runtime::ContainerSnapshot>,
        inspected: HashMap<String, runtime::ContainerSnapshot>,
    }

    #[async_trait::async_trait]
    impl RuntimeBackend for InspectingBackend {
        async fn ensure_image(&self, _reference: &str) -> Result<String, RuntimeError> {
            Err(RuntimeError::Image("unused".into()))
        }

        async fn create_container(
            &self,
            _spec: &ContainerCreateSpec,
        ) -> Result<String, RuntimeError> {
            Err(RuntimeError::Container("unused".into()))
        }

        async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::Container("unused".into()))
        }

        async fn stop_container(&self, _id: &str, _timeout_s: u32) -> Result<(), RuntimeError> {
            Err(RuntimeError::Container("unused".into()))
        }

        async fn remove_container(&self, _id: &str, _force: bool) -> Result<(), RuntimeError> {
            Err(RuntimeError::Container("unused".into()))
        }

        async fn list_containers(
            &self,
            _filter: ListFilter,
        ) -> Result<Vec<runtime::ContainerSnapshot>, RuntimeError> {
            Ok(self.listed.clone())
        }

        async fn inspect_container(
            &self,
            id: &str,
        ) -> Result<Option<runtime::ContainerSnapshot>, RuntimeError> {
            Ok(self.inspected.get(id).cloned())
        }

        async fn connect_network(
            &self,
            _container_id: &str,
            _network: &str,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::Network("unused".into()))
        }

        async fn disconnect_network(
            &self,
            _container_id: &str,
            _network: &str,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::Network("unused".into()))
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
            _hc: &runtime::HealthcheckSpec,
        ) -> Result<HealthState, RuntimeError> {
            Err(RuntimeError::Healthcheck("unused".into()))
        }

        fn name(&self) -> &'static str {
            "test"
        }
    }

    fn runtime_snap(id: &str) -> runtime::ContainerSnapshot {
        runtime::ContainerSnapshot {
            id: id.into(),
            name: "web".into(),
            image: "nginx:latest".into(),
            state: ContainerState::Running,
            stack: Some("hello".into()),
            service: Some("web".into()),
            labels: BTreeMap::from([
                ("com.docker.compose.project".into(), "hello".into()),
                ("com.docker.compose.service".into(), "web".into()),
            ]),
            created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(100),
            started_at: None,
            finished_at: None,
            exit_code: None,
            restart_count: 0,
            network_settings: NetworkSettings::default(),
            env: BTreeMap::new(),
            port_bindings: Vec::new(),
            restart: None,
        }
    }

    #[tokio::test]
    async fn list_via_backend_inspects_containers_for_rich_heartbeat_data() {
        let listed = runtime_snap("rt-web");
        let mut inspected = runtime_snap("rt-web");
        inspected.env.insert("PUID".into(), "1000".into());
        inspected.restart = Some("unless-stopped".into());
        inspected
            .network_settings
            .ip_addresses
            .insert("frontend".into(), "172.20.0.10".parse().unwrap());
        inspected.network_settings.ports.insert(
            "80/tcp".into(),
            vec![HostPort {
                host_ip: "0.0.0.0".parse().unwrap(),
                host_port: 8080,
            }],
        );

        let backend = InspectingBackend {
            listed: vec![listed],
            inspected: HashMap::from([("rt-web".into(), inspected)]),
        };

        let snapshots = list_container_snapshots_via(&backend).await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].env, vec!["PUID=1000"]);
        assert_eq!(
            snapshots[0].restart_policy.as_deref(),
            Some("unless-stopped")
        );
        assert_eq!(snapshots[0].networks, vec!["frontend"]);
        assert_eq!(snapshots[0].ports.len(), 1);

        let infos = derive_containers(&snapshots, 1_700_000_000_000);
        let rich = infos[0].rich.as_ref().expect("rich heartbeat block");
        assert_eq!(rich.env, vec!["PUID=1000"]);
        assert_eq!(rich.restart_policy, "unless-stopped");
        assert_eq!(rich.networks, vec!["frontend"]);
        assert_eq!(rich.ports[0].host_port, 8080);
    }

    fn snap(name: &str, labels: &[(&str, &str)]) -> ContainerSnapshot {
        ContainerSnapshot {
            name: name.into(),
            image: format!("{name}:latest"),
            state: "running".into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            id: format!("rt-{name}"),
            created_at_ms: 0,
            exit_code: None,
            ..Default::default()
        }
    }

    #[test]
    fn derives_stack_info_from_compose_label() {
        let containers = vec![
            snap("web", &[("com.docker.compose.project", "wordpress")]),
            snap("db", &[("com.docker.compose.project", "wordpress")]),
            snap("homer", &[]),
        ];

        let stacks = derive_stacks(&containers);
        assert_eq!(stacks.len(), 2);

        let wp = stacks.iter().find(|s| s.name == "wordpress").unwrap();
        assert_eq!(wp.source, "compose");
        assert_eq!(wp.services.len(), 2);
        assert!(wp.services.contains(&"db".to_string()));
        assert!(wp.services.contains(&"web".to_string()));

        let homer = stacks.iter().find(|s| s.name == "homer").unwrap();
        assert_eq!(homer.source, "inferred");
        assert_eq!(homer.services, vec!["homer".to_string()]);
    }

    #[test]
    fn isengard_stack_label_overrides_compose_label() {
        let containers = vec![snap(
            "x",
            &[
                ("com.docker.compose.project", "default-name"),
                ("isengard.stack", "override-name"),
            ],
        )];

        let stacks = derive_stacks(&containers);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].name, "override-name");
        assert_eq!(stacks[0].source, "manual");
    }

    #[test]
    fn derives_service_info_with_state_and_stack_link() {
        let mut compose_label = std::collections::HashMap::new();
        compose_label.insert("com.docker.compose.project".to_string(), "blog".to_string());

        let containers = vec![
            ContainerSnapshot {
                name: "web".into(),
                image: "nginx:1.25-alpine".into(),
                state: "running".into(),
                labels: compose_label.clone(),
                id: "rt-web".into(),
                created_at_ms: 0,
                exit_code: None,
                ..Default::default()
            },
            ContainerSnapshot {
                name: "homer".into(),
                image: "b4bz/homer:latest".into(),
                state: "stopped".into(),
                labels: std::collections::HashMap::new(),
                id: "rt-homer".into(),
                created_at_ms: 0,
                exit_code: None,
                ..Default::default()
            },
        ];

        let services = derive_services(&containers);
        assert_eq!(services.len(), 2);

        let web = services.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.image, "nginx:1.25-alpine");
        assert_eq!(web.state, "running");
        assert_eq!(web.stack.as_deref(), Some("blog"));

        let homer = services.iter().find(|s| s.name == "homer").unwrap();
        assert_eq!(homer.state, "stopped");
        assert_eq!(homer.stack.as_deref(), Some("homer"));
    }

    // Derive_containers shape tests.

    fn rich_snap(name: &str, id: &str, state: &str, created_at_ms: i64) -> ContainerSnapshot {
        let mut labels = HashMap::new();
        labels.insert("com.docker.compose.project".into(), "hello".into());
        labels.insert("com.docker.compose.service".into(), "web".into());
        ContainerSnapshot {
            name: name.into(),
            image: "nginx:alpine".into(),
            state: state.into(),
            labels,
            id: id.into(),
            created_at_ms,
            exit_code: None,
            ..Default::default()
        }
    }

    /// Every populated field on the source snapshot lands
    /// on the wire-format `ContainerInfo`. Runtime id, image, names,
    /// stack, service, created_at_ms, observed_at_ms.
    #[test]
    fn derive_containers_round_trip_preserves_fields() {
        let snap = rich_snap("hello-web.1", "rt-abc", "running", 1_700_000_000_000);
        let infos = derive_containers(std::slice::from_ref(&snap), 1_700_000_300_000);
        assert_eq!(infos.len(), 1);
        let info = &infos[0];
        assert_eq!(info.runtime_container_id, "rt-abc");
        assert_eq!(info.image, "nginx:alpine");
        assert_eq!(info.names, "hello-web.1");
        assert_eq!(info.stack, "hello");
        assert_eq!(info.service, "web");
        assert_eq!(info.state, "running");
        assert_eq!(info.created_at_ms, 1_700_000_000_000);
        assert_eq!(info.observed_at_ms, 1_700_000_300_000);
    }

    /// Status_message renders consistently per state with a
    /// deterministic clock. Up uses humanized age, Exited adds an exit
    /// code if present, terminal states (Paused / Restarting / Created /
    /// Dead / Removing) render verbatim.
    #[test]
    fn status_message_renders_per_state_with_mock_clock() {
        let now = 1_700_000_300_000;
        let created = 1_700_000_000_000; // 300s = 5m ago

        assert_eq!(
            render_status_message("running", created, None, now),
            "Up 5m"
        );
        assert_eq!(
            render_status_message("exited", created, Some(0), now),
            "Exited (0) 5m ago"
        );
        assert_eq!(
            render_status_message("exited", created, Some(137), now),
            "Exited (137) 5m ago"
        );
        assert_eq!(
            render_status_message("paused", created, None, now),
            "Paused"
        );
        assert_eq!(
            render_status_message("restarting", created, None, now),
            "Restarting"
        );
        assert_eq!(
            render_status_message("created", created, None, now),
            "Created"
        );
        assert_eq!(render_status_message("dead", created, None, now), "Dead");
        assert_eq!(
            render_status_message("removing", created, None, now),
            "Removing"
        );

        // Created_at unset -> render with `?` rather than a bogus number.
        assert_eq!(render_status_message("running", 0, None, now), "Up ?");
    }

    /// `observed_at_ms` is stamped from the explicit `now_ms`
    /// arg so callers can inject deterministic clocks. Two snapshots
    /// derived at different `now_ms` get distinct observed_at_ms values.
    #[test]
    fn observed_at_ms_is_stamped_from_now_arg() {
        let snap = rich_snap("hello-web.1", "rt-abc", "running", 1_700_000_000_000);
        let first = derive_containers(std::slice::from_ref(&snap), 1_700_000_300_000);
        let later = derive_containers(&[snap], 1_700_000_900_000);
        assert_eq!(first[0].observed_at_ms, 1_700_000_300_000);
        assert_eq!(later[0].observed_at_ms, 1_700_000_900_000);
    }

    /// Stack + service derive from compose labels when
    /// present. Falls back to empty string (NOT the container name) so
    /// the controller can distinguish "no stack" from "stack named X".
    #[test]
    fn label_derived_stack_and_service_with_fallbacks() {
        // No labels: empty stack + empty service.
        let bare = ContainerSnapshot {
            name: "ad-hoc".into(),
            image: "alpine".into(),
            state: "running".into(),
            labels: HashMap::new(),
            id: "rt-bare".into(),
            created_at_ms: 0,
            exit_code: None,
            ..Default::default()
        };
        let infos = derive_containers(&[bare], 1_700_000_000_000);
        assert_eq!(infos[0].stack, "");
        assert_eq!(infos[0].service, "");

        // `isengard.stack` label overrides compose project.
        let mut labels = HashMap::new();
        labels.insert("com.docker.compose.project".into(), "compose-name".into());
        labels.insert("isengard.stack".into(), "override-name".into());
        labels.insert("com.docker.compose.service".into(), "svc-name".into());
        let labelled = ContainerSnapshot {
            name: "labelled".into(),
            image: "alpine".into(),
            state: "running".into(),
            labels,
            id: "rt-labelled".into(),
            created_at_ms: 0,
            exit_code: None,
            ..Default::default()
        };
        let infos = derive_containers(&[labelled], 1_700_000_000_000);
        assert_eq!(infos[0].stack, "override-name");
        assert_eq!(infos[0].service, "svc-name");

        // Containers without a runtime id are dropped (we can't mint a
        // stable operator id without one).
        let idless = ContainerSnapshot {
            name: "no-id".into(),
            image: "alpine".into(),
            state: "running".into(),
            labels: HashMap::new(),
            id: String::new(),
            created_at_ms: 0,
            exit_code: None,
            ..Default::default()
        };
        let infos = derive_containers(&[idless], 1_700_000_000_000);
        assert!(infos.is_empty());
    }

    /// A representative `ContainerInspectResponse` maps to the expected
    /// rich-detail fields on `ContainerSnapshot`. Locks the bollard ->
    /// snapshot translation so a compose synthesizer downstream gets the
    /// shape it expects.
    #[test]
    fn enrich_from_inspect_populates_rich_fields() {
        use bollard::secret::{
            ContainerConfig, ContainerInspectResponse, EndpointSettings, HealthConfig, HostConfig,
            MountPoint, MountPointTypeEnum, NetworkSettings as BollardNetworkSettings, PortBinding,
            RestartPolicy as BollardRestartPolicy, RestartPolicyNameEnum,
        };

        let mut port_bindings: std::collections::HashMap<String, Option<Vec<PortBinding>>> =
            std::collections::HashMap::new();
        port_bindings.insert(
            "80/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("8080".to_string()),
            }]),
        );
        port_bindings.insert(
            "53/udp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some("5353".to_string()),
            }]),
        );

        let mut networks: std::collections::HashMap<String, EndpointSettings> =
            std::collections::HashMap::new();
        networks.insert("frontend".to_string(), EndpointSettings::default());
        networks.insert("backend".to_string(), EndpointSettings::default());

        let inspect = ContainerInspectResponse {
            id: Some("abc123".into()),
            name: Some("/web".into()),
            config: Some(ContainerConfig {
                image: Some("nginx:1.25".into()),
                env: Some(vec!["FOO=bar".into(), "PATH=/usr/bin".into()]),
                cmd: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
                entrypoint: Some(vec!["/docker-entrypoint.sh".into()]),
                working_dir: Some("/srv".into()),
                user: Some("nginx".into()),
                healthcheck: Some(HealthConfig {
                    test: Some(vec!["CMD".into(), "curl".into(), "-f".into(), "/".into()]),
                    interval: Some(30_000_000_000),
                    timeout: Some(5_000_000_000),
                    retries: Some(3),
                    start_period: Some(10_000_000_000),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                restart_policy: Some(BollardRestartPolicy {
                    name: Some(RestartPolicyNameEnum::ON_FAILURE),
                    maximum_retry_count: Some(5),
                }),
                ..Default::default()
            }),
            network_settings: Some(BollardNetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            mounts: Some(vec![
                MountPoint {
                    typ: Some(MountPointTypeEnum::BIND),
                    source: Some("/host/data".into()),
                    destination: Some("/data".into()),
                    rw: Some(false),
                    ..Default::default()
                },
                MountPoint {
                    typ: Some(MountPointTypeEnum::VOLUME),
                    name: Some("dbvol".into()),
                    destination: Some("/var/lib/mysql".into()),
                    rw: Some(true),
                    ..Default::default()
                },
                MountPoint {
                    typ: Some(MountPointTypeEnum::TMPFS),
                    destination: Some("/run".into()),
                    rw: Some(true),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let mut snap = ContainerSnapshot {
            name: "web".into(),
            image: "nginx:1.25".into(),
            state: "running".into(),
            id: "abc123".into(),
            ..Default::default()
        };
        enrich_snapshot_from_inspect(&mut snap, &inspect);

        // Env: passed through verbatim, both entries present.
        assert_eq!(snap.env.len(), 2);
        assert!(snap.env.contains(&"FOO=bar".to_string()));
        assert!(snap.env.contains(&"PATH=/usr/bin".to_string()));

        // Command + entrypoint: clone preserves order.
        assert_eq!(
            snap.command,
            Some(vec![
                "nginx".to_string(),
                "-g".to_string(),
                "daemon off;".to_string()
            ])
        );
        assert_eq!(
            snap.entrypoint,
            Some(vec!["/docker-entrypoint.sh".to_string()])
        );
        assert_eq!(snap.working_dir.as_deref(), Some("/srv"));
        assert_eq!(snap.user.as_deref(), Some("nginx"));

        // Healthcheck: nanoseconds preserved.
        let hc = snap.healthcheck.as_ref().expect("healthcheck populated");
        assert_eq!(hc.test, vec!["CMD", "curl", "-f", "/"]);
        assert_eq!(hc.interval_ns, 30_000_000_000);
        assert_eq!(hc.timeout_ns, 5_000_000_000);
        assert_eq!(hc.retries, 3);
        assert_eq!(hc.start_period_ns, 10_000_000_000);

        // Restart: on-failure with N -> "on-failure:N".
        assert_eq!(snap.restart_policy.as_deref(), Some("on-failure:5"));

        // Networks: alphabetised.
        assert_eq!(snap.networks, vec!["backend", "frontend"]);

        // Ports: two mappings (one tcp, one udp). Order is by sorted key.
        assert_eq!(snap.ports.len(), 2);
        let udp = snap.ports.iter().find(|p| p.protocol == "udp").unwrap();
        assert_eq!(udp.host_ip, "127.0.0.1");
        assert_eq!(udp.host_port, 5353);
        assert_eq!(udp.container_port, 53);
        let tcp = snap.ports.iter().find(|p| p.protocol == "tcp").unwrap();
        assert_eq!(tcp.host_ip, "0.0.0.0");
        assert_eq!(tcp.host_port, 8080);
        assert_eq!(tcp.container_port, 80);

        // Mounts: bind / volume / tmpfs all distinct, source picked
        // correctly for volume (the volume name, not the host path).
        assert_eq!(snap.mounts.len(), 3);
        let bind = snap.mounts.iter().find(|m| m.kind == "bind").unwrap();
        assert_eq!(bind.source, "/host/data");
        assert_eq!(bind.target, "/data");
        assert!(bind.read_only);
        let vol = snap.mounts.iter().find(|m| m.kind == "volume").unwrap();
        assert_eq!(vol.source, "dbvol");
        assert_eq!(vol.target, "/var/lib/mysql");
        assert!(!vol.read_only);
        let tmpfs = snap.mounts.iter().find(|m| m.kind == "tmpfs").unwrap();
        assert_eq!(tmpfs.source, "");
        assert_eq!(tmpfs.target, "/run");
    }

    /// Restart policy variants map to the expected compose-style
    /// strings, including the unset case (`EMPTY` -> `None`) and the
    /// retry-less `on-failure` (no suffix).
    #[test]
    fn enrich_restart_policy_variants() {
        use bollard::secret::{
            ContainerInspectResponse, HostConfig, RestartPolicy as BollardRestartPolicy,
            RestartPolicyNameEnum,
        };

        let mk = |name: RestartPolicyNameEnum, retries: Option<i64>| ContainerInspectResponse {
            host_config: Some(HostConfig {
                restart_policy: Some(BollardRestartPolicy {
                    name: Some(name),
                    maximum_retry_count: retries,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        for (name, retries, expected) in [
            (RestartPolicyNameEnum::ALWAYS, None, Some("always")),
            (
                RestartPolicyNameEnum::UNLESS_STOPPED,
                None,
                Some("unless-stopped"),
            ),
            (RestartPolicyNameEnum::NO, None, Some("no")),
            (RestartPolicyNameEnum::ON_FAILURE, None, Some("on-failure")),
            (
                RestartPolicyNameEnum::ON_FAILURE,
                Some(0),
                Some("on-failure"),
            ),
            (RestartPolicyNameEnum::EMPTY, None, None),
        ] {
            let inspect = mk(name, retries);
            let mut snap = ContainerSnapshot::default();
            enrich_snapshot_from_inspect(&mut snap, &inspect);
            assert_eq!(
                snap.restart_policy.as_deref(),
                expected,
                "name={name:?} retries={retries:?}"
            );
        }
    }
}
