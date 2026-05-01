//! Queued action for a host. The agent pulls these on its next heartbeat.

use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostActionId(pub i64);

impl std::fmt::Display for HostActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostActionKind {
    ForceUpdate { stack_name: Option<String> },
    Decommission,
}

impl HostActionKind {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::ForceUpdate { .. } => "force_update",
            Self::Decommission => "decommission",
        }
    }

    pub fn payload_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAction {
    pub id: HostActionId,
    pub host_id: HostId,
    pub kind: HostActionKind,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub result: Option<String>,
}
