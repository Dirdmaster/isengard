//! Isengard `backup` plugin (controller-side).
//!
//! Phase 11a scope: SQLite WAL snapshot, age passphrase encryption, pluggable
//! S3-compatible + local destinations, interval scheduler, LRU retention.
//!
//! Restore lives in 11b. The plugin only reads from the controller's storage;
//! it never writes to it (other than the `backup_runs` history rows).

#![allow(clippy::result_large_err)]

use async_trait::async_trait;
use isengard_core::{
    Capability, CoreError, Plugin, PluginContext, PluginRegistration, Result as CoreResult,
};
use tracing::info;

pub mod snapshot;

const PLUGIN_NAME: &str = "backup";

/// Backup plugin entry point. Phase 11a wires init/start/stop without a
/// scheduler task yet (lands in C5). The plugin is intentionally a no-op
/// when not configured: it logs once, then idles.
pub struct BackupPlugin {
    /// True once `init` has parsed the (possibly empty) plugin config slice.
    initialized: bool,
}

impl BackupPlugin {
    pub fn new() -> Self {
        Self { initialized: false }
    }
}

impl Default for BackupPlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

#[async_trait]
impl Plugin for BackupPlugin {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, _ctx: &PluginContext) -> CoreResult<()> {
        // Config is read from the `settings` table by the runner (added in
        // C5). Nothing to validate from `ctx.config` for v1.
        self.initialized = true;
        info!("backup plugin initialised (idle until configured via settings)");
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> CoreResult<()> {
        if !self.initialized {
            return Err(init_err("start called before init"));
        }
        // Scheduler task lands in C5. C2 ships scaffolding only.
        info!("backup plugin started (scheduler pending C5)");
        Ok(())
    }

    async fn stop(&mut self) -> CoreResult<()> {
        info!("backup plugin stopped");
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration {
        name: PLUGIN_NAME,
        capabilities: &[Capability::Controller],
        constructor: || Box::new(BackupPlugin::new()) as Box<dyn Plugin>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_uninitialized() {
        let p = BackupPlugin::new();
        assert!(!p.initialized);
    }

    #[test]
    fn name_is_stable() {
        let p = BackupPlugin::new();
        assert_eq!(p.name(), "backup");
    }
}
