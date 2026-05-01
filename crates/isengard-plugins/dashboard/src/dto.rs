//! Wire-format DTOs for the dashboard REST API.

use chrono::{DateTime, Utc};
use isengard_core::Event;
use isengard_storage::{EventRow, Host};
use serde::{Deserialize, Serialize};

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
}

impl From<Host> for HostDto {
    fn from(h: Host) -> Self {
        Self {
            id: ulid::Ulid::from(h.id).to_string(),
            fingerprint: h.fingerprint,
            hostname: h.hostname,
            os: h.os,
            arch: h.arch,
            agent_version: h.agent_version,
            docker_version: h.docker_version,
            fleet: "default".to_string(), // 5d migration adds the real field
            enrolled_at: DateTime::<Utc>::from_timestamp(h.enrolled_at, 0).unwrap_or_else(Utc::now),
            last_seen_at: h
                .last_seen_at
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
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
    pub host_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub fleet: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    pub enrollment_token: String,
    pub install_command: String,
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
}

#[derive(Debug, Deserialize, Default)]
pub struct HostsQuery {
    pub fleet: Option<String>,
    pub state: Option<String>,
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
}
