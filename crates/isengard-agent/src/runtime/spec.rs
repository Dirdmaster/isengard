//! Backend-agnostic types describing what to run, what's running, and what
//! the runtime tells us about it.
//!
//! Compose / logs / deploy paths speak one shape across the agent. Bollard
//! is the only backend today; fields are filled in by mapping helpers in
//! `bollard_backend.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Describes a container the agent wants to create. Backend-agnostic shape;
/// each [`super::RuntimeBackend`] translates to its native config (bollard
/// `Config<String>` for dockerd, wisp `BundleBuilder` + `NetworkSpec` for
/// wisp).
///
/// Dispatch B serialises this on disk (under
/// `<state_dir>/containers/<id>/spec.json`) so [`super::RuntimeBackend::inspect_container`]
/// and the agent's restart-policy watcher can recover labels and
/// healthcheck info without re-running compose. All sub-types carry
/// `Serialize + Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCreateSpec {
    /// Final container name (e.g. `<stack>_<service>_1` from compose).
    pub container_name: String,
    /// Image reference. Pinned to a digest by the time it gets here when the
    /// caller went through the deployment driver.
    pub image: String,
    /// Owning compose stack name. Mirrors `com.docker.compose.project`.
    pub stack: String,
    /// Service name within the stack. Mirrors `com.docker.compose.service`.
    pub service: String,
    /// `cmd` override. `None` keeps the image-baked command.
    pub command: Option<Vec<String>>,
    /// `entrypoint` override. `None` keeps the image-baked entrypoint.
    pub entrypoint: Option<Vec<String>>,
    /// `KEY=VALUE` environment, materialised as a sorted map for determinism.
    pub env: BTreeMap<String, String>,
    /// Container labels: compose-derived plus any `isengard.*` keys.
    pub labels: BTreeMap<String, String>,
    /// Bind / volume / tmpfs entries.
    pub mounts: Vec<MountSpec>,
    /// Port bindings.
    pub ports: Vec<PortSpec>,
    /// Networks the container joins.
    pub networks: Vec<String>,
    /// Restart policy applied to the container.
    pub restart: RestartPolicy,
    /// Optional healthcheck. `None` lets the runtime use the image default.
    pub healthcheck: Option<HealthcheckSpec>,
    /// Optional `USER` override.
    pub user: Option<String>,
    /// Optional working directory.
    pub working_dir: Option<String>,
    /// Optional hostname inside the container.
    pub hostname: Option<String>,
    /// Optional cgroup / namespace limits.
    pub linux_resources: Option<LinuxResources>,
    /// Secrets to bind-mount in from tmpfs after fetching from the controller.
    pub secrets: Vec<SecretMount>,
}

/// Volume / bind / tmpfs mount entry.
///
/// Dispatch A uses bind + tmpfs since they're what compose_apply already
/// emits; volume drivers come later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Host-side path (bind) or volume name (volume) or unused (tmpfs).
    pub source: String,
    /// In-container target path.
    pub target: String,
    /// Mount kind. Determines how the runtime interprets `source`.
    pub kind: MountKind,
    /// Read-only mount when true.
    pub read_only: bool,
}

/// Kind of mount entry. Determines how `source` is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountKind {
    /// Bind mount: `source` is a host filesystem path.
    Bind,
    /// Named docker volume: `source` is the volume name.
    Volume,
    /// In-memory tmpfs mount: `source` is unused.
    Tmpfs,
}

/// Port publishing entry. `host_ip` is None when bound to all interfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortSpec {
    /// Host interface address to bind on. `None` binds all interfaces.
    pub host_ip: Option<std::net::IpAddr>,
    /// Host-side port number.
    pub host_port: u16,
    /// Container-side port number.
    pub container_port: u16,
    /// Transport protocol.
    pub protocol: PortProtocol,
}

/// Transport protocol for a [`PortSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortProtocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

/// Restart policy the runtime should apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    /// Never restart.
    No,
    /// Always restart, even on clean exit.
    Always,
    /// Restart only when the container exits non-zero.
    OnFailure {
        /// Optional max retry count.
        max_retries: Option<u32>,
    },
    /// Restart unless the container was explicitly stopped.
    UnlessStopped,
}

/// Container healthcheck (docker / compose `healthcheck:` block).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthcheckSpec {
    /// Command argv. First element is `"CMD"` / `"CMD-SHELL"` / `"NONE"`.
    pub test: Vec<String>,
    /// Interval between probes.
    pub interval: Duration,
    /// Per-probe timeout.
    pub timeout: Duration,
    /// Consecutive failures before the container is marked unhealthy.
    pub retries: u32,
    /// Grace period after container start during which failures don't count.
    pub start_period: Duration,
}

/// Linux cgroup limits and namespace caps. All fields optional: unset means
/// "use docker default".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinuxResources {
    /// Memory limit in bytes.
    pub memory_max_bytes: Option<u64>,
    /// Memory + swap limit in bytes.
    pub memory_swap_max_bytes: Option<u64>,
    /// CFS quota in microseconds.
    pub cpu_quota_us: Option<i64>,
    /// CFS period in microseconds.
    pub cpu_period_us: Option<u64>,
    /// CPU shares (relative weight, default 1024).
    pub cpu_shares: Option<u64>,
    /// Max number of PIDs in the container.
    pub pids_max: Option<i64>,
}

/// Request to mount one secret value at `target` inside the container. The
/// agent's `secret_fetch` materializes the bytes onto a tmpfs path; the
/// backend translates the entry into a bind-mount.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMount {
    /// Secret name as referenced by `services.<svc>.secrets`.
    pub source: String,
    /// In-container target path.
    pub target: PathBuf,
    /// File mode applied to the materialised file (e.g. `0o400`).
    pub mode: u32,
}

/// Options for [`super::RuntimeBackend::stream_logs`]. `timestamps` keeps the
/// daemon-stamped RFC3339 prefix on each line; the agent's higher-level
/// `LogSource` decoder relies on it.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Follow new output past the current tail.
    pub follow: bool,
    /// Number of trailing lines to start with. `None` means start from now.
    pub tail: Option<u32>,
    /// Only include lines newer than this many seconds ago.
    pub since_seconds: Option<i64>,
    /// Prefix every line with its RFC3339 timestamp.
    pub timestamps: bool,
}

/// One frame of log output from a container, as the backend produces it.
/// Bytes preserve the original framing (timestamp prefix included when
/// `LogOptions.timestamps`); the consumer splits on newlines.
#[derive(Debug, Clone)]
pub struct LogChunk {
    /// Which stream this chunk came from.
    pub source: LogSource,
    /// Raw bytes as the runtime produced them.
    pub bytes: bytes::Bytes,
}

/// Which output stream a [`LogChunk`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// Container stdout.
    Stdout,
    /// Container stderr.
    Stderr,
}

/// A runtime event observed by [`super::RuntimeBackend::stream_events`].
#[derive(Debug, Clone)]
pub struct RuntimeEvent {
    /// Container id the event applies to.
    pub container_id: String,
    /// What happened.
    pub event_type: RuntimeEventType,
    /// When it happened, as the runtime reported.
    pub timestamp: SystemTime,
}

/// What changed in a [`RuntimeEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEventType {
    /// Container started.
    Start,
    /// Container stopped (operator-initiated).
    Stop,
    /// Container exited.
    Die {
        /// Process exit code, when the runtime recorded one.
        exit_code: Option<i32>,
    },
    /// Container healthcheck transitioned to healthy.
    HealthcheckPassed,
    /// Container healthcheck transitioned to unhealthy.
    HealthcheckFailed,
}

/// Filter passed to [`super::RuntimeBackend::list_containers`].
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only containers in this compose stack.
    pub stack: Option<String>,
    /// Only containers carrying this label key.
    pub label_key: Option<String>,
    /// Include stopped containers when true.
    pub all: bool,
}

/// Backend-agnostic snapshot of one container. Distinct from the legacy
/// heartbeat-oriented `crate::container_snapshot::ContainerSnapshot`
/// (which carries only the bits the controller's stacks/services
/// projection needs). This shape is the runtime view: lifecycle state,
/// network attachments, exit details.
///
/// Extended with `env`, `port_bindings`, and `restart` so the
/// compose reconciler can detect drift through the trait without
/// reaching for backend-native inspect responses. Bollard fills these
/// from `inspect_container`; wisp from the persisted spec.
#[derive(Debug, Clone)]
pub struct ContainerSnapshot {
    /// Backend-assigned container id.
    pub id: String,
    /// Container name with the leading slash already trimmed.
    pub name: String,
    /// Image reference. Resolved digest when available.
    pub image: String,
    /// Lifecycle state at the moment of the snapshot.
    pub state: ContainerState,
    /// Compose stack name when the container carries
    /// `com.docker.compose.project`.
    pub stack: Option<String>,
    /// Compose service name when the container carries
    /// `com.docker.compose.service`.
    pub service: Option<String>,
    /// Container labels.
    pub labels: BTreeMap<String, String>,
    /// Creation timestamp.
    pub created_at: SystemTime,
    /// Last start timestamp, when the container has ever run.
    pub started_at: Option<SystemTime>,
    /// Last exit timestamp, when the container has exited at least once.
    pub finished_at: Option<SystemTime>,
    /// Last exit code, when recorded.
    pub exit_code: Option<i32>,
    /// Number of times the runtime restarted the container.
    pub restart_count: u32,
    /// Network attachments and port bindings.
    pub network_settings: NetworkSettings,
    /// Environment variables visible to the running container, parsed
    /// out of `KEY=VALUE` strings. Populated by inspect-driven backends
    /// (bollard) and from the persisted spec (wisp). Empty when the
    /// backend can't read env without an extra round-trip (list calls
    /// for bollard skip env; the agent inspects per container when it
    /// needs drift detection).
    pub env: BTreeMap<String, String>,
    /// Compose-style published port strings (e.g. `"8080:80"`,
    /// `"127.0.0.1:80"`). Order-insensitive on the diff path.
    pub port_bindings: Vec<String>,
    /// Effective `restart:` string (`"always"`, `"on-failure"`,
    /// `"unless-stopped"`, `"no"`) or `None` when the runtime has no
    /// recorded policy.
    pub restart: Option<String>,
}

/// Lifecycle state of a container as the runtime reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    /// Created but never started.
    Created,
    /// Currently running.
    Running,
    /// Restarting between exits.
    Restarting,
    /// Paused (SIGSTOP).
    Paused,
    /// Exited cleanly or with an error.
    Exited,
    /// Dead (unrecoverable).
    Dead,
}

/// Network attachments and port bindings for a container.
#[derive(Debug, Clone, Default)]
pub struct NetworkSettings {
    /// `network name -> attached IP`.
    pub ip_addresses: BTreeMap<String, std::net::IpAddr>,
    /// `"80/tcp" -> [host bindings]`.
    pub ports: BTreeMap<String, Vec<HostPort>>,
    /// Runtime network mode. Docker fills this from HostConfig.network_mode or
    /// NetworkSettings.Networks; Wisp can set it from its persisted spec.
    pub mode: ContainerNetworkMode,
}

/// Result of reconciling a route target onto the runtime's ingress fabric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressEndpoint {
    /// A reachable upstream IP and the network path used to reach it.
    Ready {
        /// IP address the proxy can dial.
        ip: std::net::IpAddr,
        /// Runtime network path used for the dial.
        mode: IngressEndpointMode,
    },
    /// The route exists, but the runtime cannot currently provide a reachable endpoint.
    Unresolved(UnresolvedIngressReason),
}

/// How the proxy reaches an ingress endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressEndpointMode {
    /// Container is attached to the Isengard ingress network.
    IsengardNetwork,
    /// Container uses the host network namespace; proxy targets the Docker host gateway.
    HostNetwork,
    /// Caller supplied a literal container IP in ProxyConfig.
    ProvidedIp,
}

/// Stable unresolved route reason surfaced in logs, proxy 503s, and later UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedIngressReason {
    /// The runtime has no container matching the requested reference.
    ContainerMissing,
    /// The container exists but is not currently running.
    ContainerStopped,
    /// The ingress network could not be created.
    IngressNetworkCreateFailed,
    /// The container could not be attached to the ingress network.
    IngressNetworkAttachFailed,
    /// The runtime could not find an IP address usable by the proxy.
    NoUsableContainerIp,
    /// The container uses `network_mode: none`, so no ingress path is available.
    UnsupportedNetworkModeNone,
    /// The requested container port is invalid for proxy routing.
    InvalidContainerPort,
}

impl UnresolvedIngressReason {
    /// Return a stable snake_case identifier for external surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContainerMissing => "container_missing",
            Self::ContainerStopped => "container_stopped",
            Self::IngressNetworkCreateFailed => "ingress_network_create_failed",
            Self::IngressNetworkAttachFailed => "ingress_network_attach_failed",
            Self::NoUsableContainerIp => "no_usable_container_ip",
            Self::UnsupportedNetworkModeNone => "unsupported_network_mode_none",
            Self::InvalidContainerPort => "invalid_container_port",
        }
    }
}

/// Docker/Wisp network mode classification used by ingress reconciliation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContainerNetworkMode {
    /// Container uses a bridge-style network namespace.
    Bridge,
    /// Container shares the host network namespace.
    Host,
    /// Container has networking disabled.
    None,
    /// Runtime did not report a recognized network mode.
    #[default]
    Unknown,
}

/// One host-side `host_ip:host_port` binding for a container port.
#[derive(Debug, Clone)]
pub struct HostPort {
    /// Host interface IP.
    pub host_ip: std::net::IpAddr,
    /// Host port.
    pub host_port: u16,
}

/// Result of [`super::RuntimeBackend::run_healthcheck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// Probe is still in the start grace period.
    Starting,
    /// Probe passed.
    Healthy,
    /// Probe failed enough consecutive times to be considered unhealthy.
    Unhealthy,
}
