//! Backend-agnostic types describing what to run, what's running, and what
//! the runtime tells us about it. Phase 0.4 introduces these so the agent's
//! compose / logs / deploy paths can speak one shape regardless of whether
//! the host's runtime is dockerd (bollard) or wisp.
//!
//! The design tries to be a least-common-denominator: every field is either
//! universal across both backends, or trivially translatable. Bollard fills
//! these in via mapping helpers in `bollard_backend.rs`; WispBackend (Phase
//! 0.4 dispatch B) does the same against wisp's own state-dir.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Describes a container the agent wants to create. Backend-agnostic shape;
/// each [`super::RuntimeBackend`] translates to its native config (bollard
/// `Config<String>` for dockerd, wisp `BundleBuilder` + `NetworkSpec` for
/// wisp).
///
/// Phase 0.4 dispatch B serialises this on disk (under
/// `<state_dir>/containers/<id>/spec.json`) so [`super::RuntimeBackend::inspect_container`]
/// and the agent's restart-policy watcher can recover labels and
/// healthcheck info without re-running compose. All sub-types thus carry
/// `Serialize + Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCreateSpec {
    pub container_name: String,
    pub image: String,
    pub stack: String,
    pub service: String,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub mounts: Vec<MountSpec>,
    pub ports: Vec<PortSpec>,
    pub networks: Vec<String>,
    pub restart: RestartPolicy,
    pub healthcheck: Option<HealthcheckSpec>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
    pub hostname: Option<String>,
    pub linux_resources: Option<LinuxResources>,
    pub secrets: Vec<SecretMount>,
}

/// Volume / bind / tmpfs mount entry. Phase 0.4 dispatch A uses bind + tmpfs
/// since they're what compose_apply already emits; volume drivers come later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    pub source: String,
    pub target: String,
    pub kind: MountKind,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountKind {
    Bind,
    Volume,
    Tmpfs,
}

/// Port publishing entry. `host_ip` is None when bound to all interfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortSpec {
    pub host_ip: Option<std::net::IpAddr>,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    No,
    Always,
    OnFailure { max_retries: Option<u32> },
    UnlessStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthcheckSpec {
    pub test: Vec<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
    pub start_period: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinuxResources {
    pub memory_max_bytes: Option<u64>,
    pub memory_swap_max_bytes: Option<u64>,
    pub cpu_quota_us: Option<i64>,
    pub cpu_period_us: Option<u64>,
    pub cpu_shares: Option<u64>,
    pub pids_max: Option<i64>,
}

/// Request to mount one secret value at `target` inside the container. The
/// agent's `secret_fetch` materializes the bytes onto a tmpfs path; the
/// backend translates the entry into a bind-mount.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMount {
    pub source: String,
    pub target: PathBuf,
    pub mode: u32,
}

/// Options for [`super::RuntimeBackend::stream_logs`]. `timestamps` keeps the
/// daemon-stamped RFC3339 prefix on each line; the agent's higher-level
/// `LogSource` decoder relies on it.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    pub follow: bool,
    pub tail: Option<u32>,
    pub since_seconds: Option<i64>,
    pub timestamps: bool,
}

/// One frame of log output from a container, as the backend produces it.
/// Bytes preserve the original framing (timestamp prefix included when
/// `LogOptions.timestamps`); the consumer splits on newlines.
#[derive(Debug, Clone)]
pub struct LogChunk {
    pub source: LogSource,
    pub bytes: bytes::Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct RuntimeEvent {
    pub container_id: String,
    pub event_type: RuntimeEventType,
    pub timestamp: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEventType {
    Start,
    Stop,
    Die { exit_code: Option<i32> },
    HealthcheckPassed,
    HealthcheckFailed,
}

/// Filter passed to [`super::RuntimeBackend::list_containers`].
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub stack: Option<String>,
    pub label_key: Option<String>,
    pub all: bool,
}

/// Backend-agnostic snapshot of one container. Distinct from the legacy
/// heartbeat-oriented `crate::container_snapshot::ContainerSnapshot`
/// (which carries only the bits the controller's stacks/services
/// projection needs). This shape is the runtime view: lifecycle state,
/// network attachments, exit details.
#[derive(Debug, Clone)]
pub struct ContainerSnapshot {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: ContainerState,
    pub stack: Option<String>,
    pub service: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub created_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
    pub exit_code: Option<i32>,
    pub restart_count: u32,
    pub network_settings: NetworkSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Restarting,
    Paused,
    Exited,
    Dead,
}

#[derive(Debug, Clone, Default)]
pub struct NetworkSettings {
    /// `network name -> attached IP`.
    pub ip_addresses: BTreeMap<String, std::net::IpAddr>,
    /// `"80/tcp" -> [host bindings]`.
    pub ports: BTreeMap<String, Vec<HostPort>>,
}

#[derive(Debug, Clone)]
pub struct HostPort {
    pub host_ip: std::net::IpAddr,
    pub host_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Starting,
    Healthy,
    Unhealthy,
}
