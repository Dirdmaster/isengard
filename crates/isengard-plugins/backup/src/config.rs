//! Phase 11a: backup configuration persisted via the existing settings table.
//!
//! Keys live under `backup.config.*`. The runner and the dashboard's REST
//! handlers both read and write through `BackupConfig::load` / `save`. The
//! encryption passphrase itself is never persisted; `passphrase_fingerprint`
//! holds the 12-hex-char SHA-256 prefix of what the operator pasted at setup
//! so the UI can confirm the running env-var matches.

use isengard_storage::Inventory;
use serde::{Deserialize, Serialize};

const KEY_ENABLED: &str = "backup.config.enabled";
const KEY_DESTINATION: &str = "backup.config.destination";
const KEY_INTERVAL: &str = "backup.config.interval_secs";
const KEY_RETENTION: &str = "backup.config.retention_keep";
const KEY_FINGERPRINT: &str = "backup.config.passphrase_fingerprint";

/// Default scheduler interval (24 hours, in seconds).
pub const DEFAULT_INTERVAL_SECS: u64 = 86_400;

/// Default retention count (keep the most recent N snapshots).
pub const DEFAULT_RETENTION_KEEP: u32 = 14;

/// Soft floor on the scheduler interval. Values lower than this clamp up to
/// avoid hammering the destination if a misconfiguration sets a tiny number.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// Top-level backup configuration. Persisted as a fan of settings keys
/// (rather than one big JSON blob) so individual fields can be PATCHed
/// without reading the whole struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupConfig {
    pub enabled: bool,
    pub destination: DestinationConfig,
    pub interval_secs: u64,
    pub retention_keep: u32,
    /// Empty string when no passphrase has been set up.
    pub passphrase_fingerprint: String,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: DestinationConfig::None,
            interval_secs: DEFAULT_INTERVAL_SECS,
            retention_keep: DEFAULT_RETENTION_KEEP,
            passphrase_fingerprint: String::new(),
        }
    }
}

/// Destination kind + per-kind config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DestinationConfig {
    /// No destination configured. Snapshots cannot run.
    None,
    /// Local filesystem destination. `root` must exist and be writable.
    Local { root: String, prefix: String },
    /// S3-compatible destination. `secret_access_key` is write-only via PUT;
    /// GET responses replace it with `***`.
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        access_key_id: String,
        #[serde(default)]
        secret_access_key: String,
    },
}

impl BackupConfig {
    /// Load the full config, falling back to defaults for any missing key.
    pub async fn load(inv: &Inventory) -> Result<Self, isengard_storage::Error> {
        let mut cfg = BackupConfig::default();

        if let Some(v) = inv.get_setting(KEY_ENABLED).await? {
            cfg.enabled = v.as_bool().unwrap_or(false);
        }
        if let Some(v) = inv.get_setting(KEY_DESTINATION).await? {
            if let Ok(d) = serde_json::from_value::<DestinationConfig>(v) {
                cfg.destination = d;
            }
        }
        if let Some(v) = inv.get_setting(KEY_INTERVAL).await? {
            if let Some(n) = v.as_u64() {
                cfg.interval_secs = n.max(MIN_INTERVAL_SECS);
            }
        }
        if let Some(v) = inv.get_setting(KEY_RETENTION).await? {
            if let Some(n) = v.as_u64() {
                cfg.retention_keep = n.clamp(1, 1000) as u32;
            }
        }
        if let Some(v) = inv.get_setting(KEY_FINGERPRINT).await? {
            if let Some(s) = v.as_str() {
                cfg.passphrase_fingerprint = s.to_string();
            }
        }

        Ok(cfg)
    }

    /// Persist the full config. Each field becomes its own settings row.
    pub async fn save(&self, inv: &Inventory) -> Result<(), isengard_storage::Error> {
        inv.set_setting(KEY_ENABLED, &serde_json::Value::Bool(self.enabled))
            .await?;
        inv.set_setting(
            KEY_DESTINATION,
            &serde_json::to_value(&self.destination).expect("destination json"),
        )
        .await?;
        inv.set_setting(
            KEY_INTERVAL,
            &serde_json::Value::Number(self.interval_secs.max(MIN_INTERVAL_SECS).into()),
        )
        .await?;
        inv.set_setting(
            KEY_RETENTION,
            &serde_json::Value::Number(self.retention_keep.into()),
        )
        .await?;
        inv.set_setting(
            KEY_FINGERPRINT,
            &serde_json::Value::String(self.passphrase_fingerprint.clone()),
        )
        .await?;
        Ok(())
    }
}
