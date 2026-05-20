//! Settings: per-installation JSON key/value store.
//!
//! Used by the notifier plugins and other features that need durable
//! configuration without earning a dedicated table. Read via
//! [`crate::Inventory::get_setting`], write via
//! [`crate::Inventory::set_setting`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row from the `settings` table.
///
/// The shape is intentionally narrow: a string key, an arbitrary JSON
/// value, and the timestamp of the last write. Callers serialize their
/// typed config into `value` and decide what schema lives there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// Setting key, e.g. `notifier.telegram.enabled`.
    pub key: String,
    /// JSON value bound to `key`. The DAO does not interpret the shape.
    pub value: serde_json::Value,
    /// When the row was last written (RFC3339).
    pub updated_at: DateTime<Utc>,
}
