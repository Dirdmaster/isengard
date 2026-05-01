//! Fleet: a user-defined tag for grouping hosts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fleet {
    pub name: String,
    pub created_at: DateTime<Utc>,
}
