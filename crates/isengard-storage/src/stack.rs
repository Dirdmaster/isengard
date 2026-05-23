//! Stack entity: a logical grouping of services on one host.
//!
//! Migration `0003` lands the `stacks` table; `0024` adds the
//! `compose_yaml` columns for the v0.3c compose import path; `0028`
//! lands the `stack.toml` manifest columns and the
//! `stack_hooks`/`stack_secrets` sidecar tables. A stack is what an
//! operator names with `isd stack up` and what `isd ps` groups by.

use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Surrogate primary key for stacks.
///
/// The `stacks` table uses an autoincrementing integer because there is
/// no natural identifier: the `(host_id, name)` pair is unique but
/// heavy to use as a foreign key everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StackId(pub i64);

impl std::fmt::Display for StackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a stack's identity came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StackSource {
    /// Discovered via `com.docker.compose.project=<name>` label.
    Compose,
    /// User-provided override via `isengard.stack=<name>` label.
    Manual,
    /// Synthesized: single-service stack named after the container,
    /// used when neither label is present.
    Inferred,
}

impl StackSource {
    /// Canonical lowercase spelling matching the SQLite column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Manual => "manual",
            Self::Inferred => "inferred",
        }
    }

    /// Parse a `stack_sources.source` string back into the enum.
    /// Returns `None` for unknown values rather than guessing.
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

/// One row from the `stacks` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    /// Surrogate key.
    pub id: StackId,
    /// Host the stack lives on.
    pub host_id: HostId,
    /// Operator-visible stack name (compose project or manual override).
    pub name: String,
    /// How the stack was identified.
    pub source: StackSource,
    /// When the controller first saw this stack.
    pub discovered_at: DateTime<Utc>,
    /// Verbatim `stack.toml`. NULL for legacy compose-only stacks.
    #[serde(default)]
    pub manifest_toml: Option<String>,
    /// Sha256 hex of `manifest_toml`. NULL when manifest absent.
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    /// RFC3339 timestamp when the manifest was last written.
    #[serde(default)]
    pub manifest_imported_at: Option<String>,
    /// Stack-level deploy strategy override. NULL means the
    /// per-service phase 10g labels drive behavior.
    #[serde(default)]
    pub deploy_strategy: Option<String>,
}

/// Insert payload for a stack row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStack {
    /// Host the stack lives on.
    pub host_id: HostId,
    /// Stack name (the operator-visible key).
    pub name: String,
    /// Source attribution.
    pub source: StackSource,
}

/// Snapshot of the YAML stored on a stack row.
///
/// Returned by [`crate::Inventory::get_stack_compose`]; surfaced as the
/// body of `GET /api/v1/stacks/<id>/compose`. v0.3c compose import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackComposeRow {
    /// Verbatim compose YAML the agent reverse-engineered from the
    /// running containers.
    pub yaml: String,
    /// Sha256 hex of `yaml`.
    pub sha256: String,
    /// RFC3339 string: when the agent wrote this YAML to disk.
    pub imported_at: String,
}

/// One hook row read back from `stack_hooks`.
///
/// Ordered by `(on_event, ordinal)`. Used by the agent payload builder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackHook {
    /// Lifecycle event name (`pre-deploy`, `post-deploy`, `failure`).
    pub on_event: String,
    /// Argv to run. First element is the program; rest are arguments.
    pub cmd: Vec<String>,
    /// Per-hook timeout in milliseconds.
    pub timeout_ms: i64,
    /// What to do on non-zero exit: `abort`, `continue`.
    pub on_error: String,
}

/// Full manifest read-back for `GET /api/v1/stacks/<id>`.
///
/// Bundles the TOML body plus the resolved secret-name and hook lists
/// in a single shape so the API handler can serialize one struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackManifestBundle {
    /// Verbatim `stack.toml`.
    pub manifest_toml: Option<String>,
    /// Sha256 hex of the manifest.
    pub manifest_sha256: Option<String>,
    /// RFC3339 import timestamp.
    pub manifest_imported_at: Option<String>,
    /// Stack-level strategy override (NULL == defer to service labels).
    pub deploy_strategy: Option<String>,
    /// Secret names bound to the stack.
    pub secrets: Vec<String>,
    /// Hook rows ordered by `(on_event, ordinal)`.
    pub hooks: Vec<StackHook>,
}

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
