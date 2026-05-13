//! Service entity: one container belonging to a host (and optionally a stack).

use crate::host::HostId;
use crate::stack::StackId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceId(pub i64);

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Lifecycle state for a service as observed by an agent's heartbeat.
///
/// v0.5.3: extended beyond `Running`/`Stopped`/`Restarting`/`Unknown` so
/// mid-startup states surface correctly in `isd ps` and `isd deploy --watch`
/// instead of collapsing to `Unknown`. Specifically, wisp's
/// `ContainerState::Created` (bundle staged + cgroup ready, process not
/// yet forked) used to map to `Unknown`; now it lands on `Creating`.
///
/// ## Wire format + back-compat
///
/// The on-disk representation is the lowercase variant name (`as_str`).
/// Old binaries reading new strings fall through `from_str`'s default arm
/// and decode as `Unknown` (forward-compatible by construction). New
/// binaries reading old `"unknown"` rows preserve them as `Unknown`.
///
/// SQLite column type is TEXT, so no schema migration is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    /// Image pull in progress. Reserved for the eventual Channel B
    /// (runtime-event) integration; today no backend emits this directly.
    Pulling,
    /// Bundle staged and the runtime created the container but the
    /// process is not yet forked. Maps from
    /// `bollard::ContainerStateStatusEnum::CREATED`.
    Creating,
    /// Process is alive.
    Running,
    /// Runtime is cycling the container (compose restart policy fired,
    /// or wisp/bollard reported `Restarting`).
    Restarting,
    /// Process exited (zero or non-zero) and the runtime has not been
    /// asked to restart it.
    Stopped,
    /// Terminal error during creation or start. Reserved for the
    /// runtime-event path; the polling-based reconciler today reports a
    /// failed container as `Stopped` because it cannot distinguish the
    /// "exited cleanly" case from the "never started" case without the
    /// exit code.
    Failed,
    /// Fallback for unrecognised state strings (including states written
    /// by future agent versions that this binary doesn't know about).
    Unknown,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pulling => "pulling",
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "pulling" => Self::Pulling,
            // Accept both `creating` (the new canonical string) and the
            // docker-compatible `created` that older agent heartbeats
            // (and the legacy docker-socket snapshot path) still emit.
            "creating" | "created" => Self::Creating,
            "running" => Self::Running,
            "restarting" => Self::Restarting,
            "stopped" | "exited" => Self::Stopped,
            "failed" | "dead" => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub id: ServiceId,
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub name: String,
    pub image: String,
    pub state: ServiceState,
    pub last_seen_at: DateTime<Utc>,
    pub deploy_strategy_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertService {
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub name: String,
    pub image: String,
    pub state: ServiceState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::EnrollHost;
    use crate::inventory::Inventory;
    use crate::stack::{InsertStack, StackSource};

    async fn setup() -> (Inventory, ServiceId) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                hostname: "h".into(),
                fingerprint: "fp".into(),
                fleet: "default".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "24.0".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let svc_id = inv
            .insert_service(InsertService {
                host_id,
                stack_id: Some(stack_id),
                name: "web".into(),
                image: "nginx:alpine".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        (inv, svc_id)
    }

    #[tokio::test]
    async fn override_starts_null() {
        let (inv, svc_id) = setup().await;
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override, None);
    }

    #[tokio::test]
    async fn set_and_get_override() {
        let (inv, svc_id) = setup().await;
        inv.set_service_deploy_strategy_override(svc_id, Some("blue-green"))
            .await
            .unwrap();
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override.as_deref(), Some("blue-green"));

        inv.set_service_deploy_strategy_override(svc_id, None)
            .await
            .unwrap();
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override, None);
    }

    /// v0.5.3: every variant round-trips through `as_str` / `from_str`.
    /// Guards against the wire-format regression that surfaced as 4/8
    /// services showing `unknown` in `isd ps` on the lausanne deploy:
    /// adding a variant without wiring it through the string conversion
    /// silently degraded back to `Unknown`.
    #[test]
    fn service_state_string_round_trip_for_every_variant() {
        for state in [
            ServiceState::Pulling,
            ServiceState::Creating,
            ServiceState::Running,
            ServiceState::Restarting,
            ServiceState::Stopped,
            ServiceState::Failed,
            ServiceState::Unknown,
        ] {
            let s = state.as_str();
            let back = ServiceState::from_str(s);
            assert_eq!(back, state, "{s} did not round-trip");
        }
    }

    /// v0.5.3: pre-extension agents emit docker-compatible state strings
    /// (`created`, `exited`, `dead`). They map into the v0.5.3 enum at
    /// the controller boundary so heartbeats from old agents stop
    /// landing on `Unknown`.
    #[test]
    fn service_state_accepts_docker_compatible_aliases() {
        assert_eq!(ServiceState::from_str("created"), ServiceState::Creating);
        assert_eq!(ServiceState::from_str("exited"), ServiceState::Stopped);
        assert_eq!(ServiceState::from_str("dead"), ServiceState::Failed);
        // Anything else still falls back to Unknown so future variants
        // emitted by a newer agent don't crash an older controller.
        assert_eq!(
            ServiceState::from_str("paused"),
            ServiceState::Unknown,
            "paused has no v0.5.3 mapping yet",
        );
        assert_eq!(ServiceState::from_str(""), ServiceState::Unknown);
        assert_eq!(ServiceState::from_str("garbage"), ServiceState::Unknown);
    }

    #[tokio::test]
    async fn lookup_by_name_returns_matching_service() {
        let (inv, svc_id) = setup().await;
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        let by_name = inv
            .get_service_by_name(svc.host_id, svc.stack_id, "web")
            .await
            .unwrap()
            .expect("service found by name");
        assert_eq!(by_name.id, svc_id);

        let missing = inv
            .get_service_by_name(svc.host_id, svc.stack_id, "doesnotexist")
            .await
            .unwrap();
        assert!(missing.is_none());
    }
}
