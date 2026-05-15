//! Stack entity: a logical grouping of services (typically a Docker Compose
//! project) that lives on one host.

use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Surrogate primary key for stacks. The `stacks` table uses an autoincrementing
/// integer because there is no natural identifier — the `(host_id, name)` pair
/// is unique but heavy to use as a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StackId(pub i64);

impl std::fmt::Display for StackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StackSource {
    /// Discovered via `com.docker.compose.project=<name>` label.
    Compose,
    /// User-provided override via `isengard.stack=<name>` label.
    Manual,
    /// Synthesized — single-service stack named after the container, used when
    /// neither label is present.
    Inferred,
}

impl StackSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Manual => "manual",
            Self::Inferred => "inferred",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "compose" => Some(Self::Compose),
            "manual" => Some(Self::Manual),
            "inferred" => Some(Self::Inferred),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub id: StackId,
    pub host_id: HostId,
    pub name: String,
    pub source: StackSource,
    pub discovered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStack {
    pub host_id: HostId,
    pub name: String,
    pub source: StackSource,
}

/// v0.3c compose import: snapshot of the YAML stored on a stack row.
/// Returned by [`crate::Inventory::get_stack_compose`]; surfaced as the
/// body of `GET /api/v1/stacks/<id>/compose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackComposeRow {
    pub yaml: String,
    pub sha256: String,
    /// RFC3339 string: when the agent wrote this YAML to disk.
    pub imported_at: String,
}

// Track A teardown (2026-05-15): the manifest bundle struct is gone.
// Stack secret bindings (the only piece that survives) are read via
// Inventory::list_stack_secrets; callers that used to consume
// StackManifestBundle either drop the call or switch to that accessor.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_id_round_trips_through_json() {
        let id = StackId(42);
        let s = serde_json::to_string(&id).unwrap();
        let back: StackId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn stack_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&StackSource::Compose).unwrap(),
            "\"compose\""
        );
        assert_eq!(
            serde_json::to_string(&StackSource::Manual).unwrap(),
            "\"manual\""
        );
        assert_eq!(
            serde_json::to_string(&StackSource::Inferred).unwrap(),
            "\"inferred\""
        );
    }
}
