//! Wire-format DTOs for the dashboard REST API.

use chrono::{DateTime, Utc};
use isengard_core::Event;
use isengard_core::policy::ResolvedPolicy;
use isengard_storage::{ContainerRow, EventRow, Host, RoutingRule, Service, Stack};
use serde::{Deserialize, Serialize};

use crate::deployments::DeploymentDto;

#[derive(Debug, Clone, Serialize)]
/// HostDto.
pub struct HostDto {
    /// `id` field.
    pub id: String,
    /// `fingerprint` field.
    pub fingerprint: String,
    /// `hostname` field.
    pub hostname: String,
    /// `os` field.
    pub os: String,
    /// `arch` field.
    pub arch: String,
    /// `agent_version` field.
    pub agent_version: String,
    /// `docker_version` field.
    pub docker_version: String,
    /// `enrolled_at` field.
    pub enrolled_at: DateTime<Utc>,
    /// `last_seen_at` field.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Wisp: which runtime backend this host's agent is
    /// driving (`docker`, `wisp`, ...). `"docker"` for pre-0.5
    /// agents that never gossiped the field. Read from
    /// `metadata.runtime_backend` on the host row.
    pub runtime_backend: String,
    /// Operator-facing dial target (e.g.
    /// `dirdmaster@10.17.0.125`). Captured by the CLI at enroll time
    /// from the operator's active docker context URL; overridable
    /// via `isd ssh hosts set <agent> --dial <target>`. `None`
    /// for hosts that predate the column or were enrolled by an
    /// older CLI build.
    pub dial_target: Option<String>,
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
            enrolled_at: DateTime::<Utc>::from_timestamp(h.enrolled_at, 0).unwrap_or_else(Utc::now),
            last_seen_at: h
                .last_seen_at
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
            runtime_backend,
            dial_target: h.dial_target,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
/// EventDto.
pub struct EventDto {
    /// `id` field.
    pub id: i64,
    /// `kind` field.
    pub kind: String,
    /// `host_id` field.
    pub host_id: Option<String>,
    /// `container_name` field.
    pub container_name: Option<String>,
    /// `image` field.
    pub image: Option<String>,
    /// `old_digest` field.
    pub old_digest: Option<String>,
    /// `new_digest` field.
    pub new_digest: Option<String>,
    /// `error` field.
    pub error: Option<String>,
    /// `summary` field.
    pub summary: String,
    /// `metadata` field.
    pub metadata: serde_json::Value,
    /// `occurred_at` field.
    pub occurred_at: DateTime<Utc>,
    /// `received_at` field.
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
    /// `kind` field.
    pub kind: String,
    /// `host_id` field.
    pub host_id: Option<String>,
    /// `container_name` field.
    pub container_name: Option<String>,
    /// `image` field.
    pub image: Option<String>,
    /// `summary` field.
    pub summary: String,
    /// `occurred_at` field.
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
/// EnrollmentDto.
pub struct EnrollmentDto {
    /// `agent_id` field.
    pub agent_id: String,
    /// `enrollment_token` field.
    pub enrollment_token: String,
    /// `install_command` field.
    pub install_command: String,
}

#[derive(Debug, Clone, Serialize)]
/// SettingsDto.
pub struct SettingsDto {
    /// `values` field.
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
/// PatchSettingsBody.
pub struct PatchSettingsBody {
    /// `values` field.
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
/// EnrollRequest.
pub struct EnrollRequest {
    /// `hostname` field.
    pub hostname: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
/// PatchHostRequest.
pub struct PatchHostRequest {
    /// Operator-facing dial target to set on the host row. Send
    /// `null` (or omit) to leave the value unchanged; send an empty
    /// string to clear it. Captured by the CLI at enroll time and
    /// overridable via `isd ssh hosts set <agent> --dial <target>`.
    #[serde(default)]
    pub dial_target: Option<String>,
    /// Operator-facing host name to set on the host row. Send `null`
    /// (or omit) to leave the value unchanged. Empty string is
    /// rejected with 400 since `hostname` is part of the display
    /// path. Captured by the CLI at enroll time from the target
    /// docker daemon's `info.name` (or `--name <NAME>` override) so
    /// `isd ssh hosts` shows a real label instead of the agent's
    /// container hash.
    #[serde(default)]
    pub hostname: Option<String>,
}

#[derive(Debug, Serialize)]
/// DeleteResponse.
pub struct DeleteResponse {
    /// `deleted` field.
    pub deleted: bool,
}

#[derive(Debug, Deserialize, Default)]
/// EventsQuery.
pub struct EventsQuery {
    /// `limit` field.
    pub limit: Option<i64>,
    /// `kind` field.
    pub kind: Option<String>,
    /// `host_id` field.
    pub host_id: Option<String>,
    /// Filter events whose `metadata.deployment.id`
    /// matches this id. Used by the History tab row-expand timeline.
    pub deployment_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
/// HostsQuery.
pub struct HostsQuery {
    /// `state` field.
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
/// StackDto.
pub struct StackDto {
    /// `id` field.
    pub id: String,
    /// `host_id` field.
    pub host_id: String,
    /// `name` field.
    pub name: String,
    /// One of "compose", "manual", "inferred".
    pub source: String,
    /// `discovered_at` field.
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
/// ServiceDto.
pub struct ServiceDto {
    /// Service primary key (i64) rendered as a string for JSON consistency.
    pub id: String,
    /// `host_id` field.
    pub host_id: String,
    /// Hostname resolved from inventory; `None` when the host could not be
    /// looked up (deleted out-of-band, or batch translation skipped it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// `stack_id` field.
    pub stack_id: Option<String>,
    /// `name` field.
    pub name: String,
    /// `image` field.
    pub image: String,
    /// v0.5.3: one of `"pulling"`, `"creating"`, `"running"`,
    /// `"restarting"`, `"stopped"`, `"failed"`, `"unknown"`. Older
    /// agents may still emit pre-extension strings (`"created"`,
    /// `"exited"`, `"dead"`); the controller normalises those via
    /// `ServiceState::from_str` before persistence.
    pub state: String,
    /// `last_seen_at` field.
    pub last_seen_at: DateTime<Utc>,
    /// `deploy_strategy_override` field.
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
    /// `service` field.
    pub service: ServiceDto,
    /// `other_instances` field.
    pub other_instances: Vec<ServiceDto>,
    /// `effective_policy` field.
    pub effective_policy: ResolvedPolicy,
    /// `last_deployment` field.
    pub last_deployment: Option<DeploymentDto>,
    /// `recent_events` field.
    pub recent_events: Vec<EventDto>,
    /// `routing_rules` field.
    pub routing_rules: Vec<RoutingRule>,
}

#[derive(Debug, Clone, Serialize)]
/// SparklineDto.
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
            dial_target: None,
        };
        let dto: HostDto = h.into();
        assert_eq!(dto.enrolled_at.timestamp(), 1714521600);
        assert_eq!(dto.last_seen_at.unwrap().timestamp(), 1714525200);
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
        };

        let dto: StackDto = s.into();
        assert_eq!(dto.id, "7");
        assert_eq!(dto.host_id, ulid::Ulid::from(host_id).to_string());
        assert_eq!(dto.name, "wordpress");
        assert_eq!(dto.source, "compose");
    }

    /// Wisp: host metadata's `runtime_backend` key
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
            dial_target: None,
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
            dial_target: None,
        };
        let dto: HostDto = h.into();
        assert_eq!(dto.runtime_backend, "docker");
    }
}

/// Container list / get response shape.
///
/// `host_offline` and `host_offline_secs` are derived at the API
/// boundary by joining containers to hosts and comparing
/// `hosts.last_seen_at` to a configured threshold (see
/// [`HOST_OFFLINE_THRESHOLD_SECS`] in `containers.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct ContainerDto {
    /// 16-char hex digest. The default `isd ps` column shows the first
    /// 12 chars; `--no-trunc` shows the full value.
    pub id: String,
    /// `runtime_container_id` field.
    pub runtime_container_id: String,
    /// `image` field.
    pub image: String,
    /// `command` field.
    pub command: Option<String>,
    /// `state` field.
    pub state: String,
    /// `status_message` field.
    pub status_message: Option<String>,
    /// `names` field.
    pub names: String,
    /// `stack` field.
    pub stack: Option<String>,
    /// `service` field.
    pub service: Option<String>,
    /// `host_id` field.
    pub host_id: String,
    /// `host_name` field.
    pub host_name: Option<String>,
    /// True when the host's last heartbeat is older than the
    /// configured threshold. `state` is left as last reported; the
    /// client renders a `(host offline 30s)` qualifier in STATUS.
    pub host_offline: bool,
    /// Seconds since the host's last heartbeat. 0 when the host is
    /// currently considered online.
    pub host_offline_secs: i64,
    /// `created_at` field.
    pub created_at: Option<DateTime<Utc>>,
    /// `first_seen_at` field.
    pub first_seen_at: DateTime<Utc>,
    /// `last_seen_at` field.
    pub last_seen_at: DateTime<Utc>,
    /// `removed_at` field.
    pub removed_at: Option<DateTime<Utc>>,
}

impl ContainerDto {
    /// Project a storage `ContainerRow` to its wire DTO. `host_name`
    /// and `host_offline*` come from a host lookup at the handler
    /// level (the DAO does not join, so the dashboard handler computes
    /// these once per request).
    pub fn from_row(
        row: ContainerRow,
        host_name: Option<String>,
        host_offline: bool,
        host_offline_secs: i64,
    ) -> Self {
        Self {
            id: row.id,
            runtime_container_id: row.runtime_container_id,
            image: row.image,
            command: row.command,
            state: row.state,
            status_message: row.status_message,
            names: row.names,
            stack: row.stack,
            service: row.service,
            host_id: ulid::Ulid::from(row.host_id).to_string(),
            host_name,
            host_offline,
            host_offline_secs,
            created_at: row
                .created_at
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
            first_seen_at: DateTime::<Utc>::from_timestamp(row.first_seen_at, 0)
                .unwrap_or_else(Utc::now),
            last_seen_at: DateTime::<Utc>::from_timestamp(row.last_seen_at, 0)
                .unwrap_or_else(Utc::now),
            removed_at: row
                .removed_at
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
        }
    }
}
