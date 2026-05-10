//! Wire-format DTOs for the dashboard REST API.

use chrono::{DateTime, Utc};
use isengard_core::Event;
use isengard_core::policy::ResolvedPolicy;
use isengard_storage::{EventRow, Host, RoutingRule, Service, Stack};
use serde::{Deserialize, Serialize};

use crate::deployments::DeploymentDto;

#[derive(Debug, Clone, Serialize)]
pub struct HostDto {
    pub id: String,
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
    pub fleet: String,
    pub enrolled_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Phase 0.5 wisp: which runtime backend this host's agent is
    /// driving (`docker`, `wisp`, ...). `"docker"` for pre-0.5
    /// agents that never gossiped the field. Read from
    /// `metadata.runtime_backend` on the host row.
    pub runtime_backend: String,
}

impl From<Host> for HostDto {
    fn from(h: Host) -> Self {
        // Pre-0.5 agents never set this; default to `docker` so the
        // dashboard / isd never renders an empty cell for hosts
        // running the legacy backend.
        let runtime_backend = h
            .metadata
            .get("runtime_backend")
            .and_then(|v| v.as_str())
            .unwrap_or("docker")
            .to_string();
        Self {
            id: ulid::Ulid::from(h.id).to_string(),
            fingerprint: h.fingerprint,
            hostname: h.hostname,
            os: h.os,
            arch: h.arch,
            agent_version: h.agent_version,
            docker_version: h.docker_version,
            fleet: h.fleet,
            enrolled_at: DateTime::<Utc>::from_timestamp(h.enrolled_at, 0).unwrap_or_else(Utc::now),
            last_seen_at: h
                .last_seen_at
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
            runtime_backend,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EventDto {
    pub id: i64,
    pub kind: String,
    pub host_id: Option<String>,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub summary: String,
    pub metadata: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

impl From<EventRow> for EventDto {
    fn from(r: EventRow) -> Self {
        let metadata = r
            .metadata_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        Self {
            id: r.id,
            kind: r.kind,
            host_id: r.host_id.map(|h| ulid::Ulid::from(h).to_string()),
            container_name: r.container_name,
            image: r.image,
            old_digest: r.old_digest,
            new_digest: r.new_digest,
            error: r.error,
            summary: r.summary,
            metadata,
            occurred_at: r.occurred_at,
            received_at: r.received_at,
        }
    }
}

/// Live event frame for WebSocket broadcasts.
#[derive(Debug, Clone, Serialize)]
pub struct LiveEventDto {
    pub kind: String,
    pub host_id: Option<String>,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub summary: String,
    pub occurred_at: DateTime<Utc>,
}

impl From<&Event> for LiveEventDto {
    fn from(e: &Event) -> Self {
        Self {
            kind: e.kind.clone(),
            host_id: e.host_id.map(|h| h.to_string()),
            container_name: e.container_name.clone(),
            image: e.image.clone(),
            summary: e.summary.clone(),
            occurred_at: e.occurred_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetDto {
    pub name: String,
    pub host_count: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateFleetBody {
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrollmentDto {
    pub agent_id: String,
    pub enrollment_token: String,
    pub install_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsDto {
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PatchSettingsBody {
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub fleet: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchHostRequest {
    pub fleet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub deleted: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    pub limit: Option<i64>,
    pub kind: Option<String>,
    pub host_id: Option<String>,
    /// Phase 10c (T4 refs #50): filter events whose `metadata.deployment.id`
    /// matches this id. Used by the History tab row-expand timeline.
    pub deployment_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HostsQuery {
    pub fleet: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StackDto {
    pub id: String,
    pub host_id: String,
    pub name: String,
    /// One of "compose", "manual", "inferred".
    pub source: String,
    pub discovered_at: DateTime<Utc>,
}

impl From<Stack> for StackDto {
    fn from(s: Stack) -> Self {
        Self {
            id: s.id.0.to_string(),
            host_id: ulid::Ulid::from(s.host_id).to_string(),
            name: s.name,
            source: s.source.as_str().to_string(),
            discovered_at: s.discovered_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceDto {
    /// Service primary key (i64) rendered as a string for JSON consistency.
    pub id: String,
    pub host_id: String,
    /// Hostname resolved from inventory; `None` when the host could not be
    /// looked up (deleted out-of-band, or batch translation skipped it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    pub stack_id: Option<String>,
    pub name: String,
    pub image: String,
    /// One of "running", "stopped", "restarting", "unknown".
    pub state: String,
    pub last_seen_at: DateTime<Utc>,
    pub deploy_strategy_override: Option<String>,
}

impl ServiceDto {
    /// Build a DTO from a storage row, optionally enriched with the host's
    /// hostname for display. Pass `None` for `hostname` when the caller
    /// cannot resolve it (e.g. batch endpoints that skip the inventory
    /// lookup).
    pub fn from_service(s: Service, hostname: Option<String>) -> Self {
        Self {
            id: s.id.to_string(),
            host_id: ulid::Ulid::from(s.host_id).to_string(),
            hostname,
            stack_id: s.stack_id.map(|sid| sid.0.to_string()),
            name: s.name,
            image: s.image,
            state: s.state.as_str().to_string(),
            last_seen_at: s.last_seen_at,
            deploy_strategy_override: s.deploy_strategy_override,
        }
    }
}

/// Envelope returned by `GET /api/v1/services/:stack_id/:service_name`.
///
/// Carries everything the service detail page needs in a single round-trip:
/// the primary container row, any other instances of the same service in
/// the same stack across other hosts, the resolved effective policy, the
/// most recent deployment for the service (if any), the last 50 events
/// scoped to the host (filtered to this container when possible), and the
/// fleet-wide routing rules attached to the service.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceDetailDto {
    pub service: ServiceDto,
    pub other_instances: Vec<ServiceDto>,
    pub effective_policy: ResolvedPolicy,
    pub last_deployment: Option<DeploymentDto>,
    pub recent_events: Vec<EventDto>,
    pub routing_rules: Vec<RoutingRule>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SparklineDto {
    /// Number of buckets (typically 24 for a 24h range, one per hour).
    pub buckets: Vec<u32>,
    /// Range queried, e.g. "24h".
    pub range: String,
    /// Sum of all buckets, for the row's "N events" summary.
    pub total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_dto_from_host_converts_enrolled_at() {
        use isengard_storage::HostId;
        let h = Host {
            id: HostId::new(),
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            enrolled_at: 1714521600, // 2024-05-01T00:00:00Z
            last_seen_at: Some(1714525200),
            metadata: serde_json::json!({}),
            fleet: "default".to_string(),
        };
        let dto: HostDto = h.into();
        assert_eq!(dto.enrolled_at.timestamp(), 1714521600);
        assert_eq!(dto.last_seen_at.unwrap().timestamp(), 1714525200);
        assert_eq!(dto.fleet, "default");
    }

    #[test]
    fn event_dto_serializes_metadata_as_json() {
        let row = EventRow {
            id: 7,
            kind: "container.checked".into(),
            host_id: None,
            container_name: Some("web".into()),
            image: Some("nginx".into()),
            old_digest: None,
            new_digest: None,
            error: None,
            summary: "checked".into(),
            metadata_json: Some(r#"{"k":"v"}"#.into()),
            occurred_at: Utc::now(),
            received_at: Utc::now(),
        };
        let dto: EventDto = row.into();
        assert_eq!(dto.id, 7);
        assert_eq!(dto.metadata["k"], "v");
    }

    #[test]
    fn stack_dto_maps_from_storage_stack() {
        use chrono::TimeZone;
        use isengard_storage::{HostId, Stack, StackId, StackSource};

        let host_id = HostId::new();
        let s = Stack {
            id: StackId(7),
            host_id,
            name: "wordpress".into(),
            source: StackSource::Compose,
            discovered_at: Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap(),
            manifest_toml: None,
            manifest_sha256: None,
            manifest_imported_at: None,
            deploy_strategy: None,
            manifest_fleet: None,
        };

        let dto: StackDto = s.into();
        assert_eq!(dto.id, "7");
        assert_eq!(dto.host_id, ulid::Ulid::from(host_id).to_string());
        assert_eq!(dto.name, "wordpress");
        assert_eq!(dto.source, "compose");
    }

    #[test]
    fn host_dto_carries_real_fleet() {
        use isengard_storage::{Host, HostId};
        let h = Host {
            id: HostId::new(),
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            enrolled_at: 0,
            last_seen_at: None,
            metadata: serde_json::json!({}),
            fleet: "prod".into(),
        };

        let dto: HostDto = h.into();
        assert_eq!(dto.fleet, "prod");
    }

    /// Phase 0.5 wisp: host metadata's `runtime_backend` key
    /// surfaces on the DTO so the dashboard / isd ps can render the
    /// column. Hosts whose agents never gossiped a backend (pre-0.5)
    /// default to `"docker"`.
    #[test]
    fn host_dto_exposes_runtime_backend_from_metadata() {
        use isengard_storage::{Host, HostId};
        let h = Host {
            id: HostId::new(),
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            enrolled_at: 0,
            last_seen_at: None,
            metadata: serde_json::json!({"runtime_backend": "wisp"}),
            fleet: "prod".into(),
        };
        let dto: HostDto = h.into();
        assert_eq!(dto.runtime_backend, "wisp");
    }

    #[test]
    fn host_dto_runtime_backend_defaults_to_docker_for_legacy_metadata() {
        use isengard_storage::{Host, HostId};
        let h = Host {
            id: HostId::new(),
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            enrolled_at: 0,
            last_seen_at: None,
            metadata: serde_json::json!({}),
            fleet: "prod".into(),
        };
        let dto: HostDto = h.into();
        assert_eq!(dto.runtime_backend, "docker");
    }
}
