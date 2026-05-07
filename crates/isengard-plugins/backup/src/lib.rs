//! Isengard `backup` plugin (controller-side).
//!
//! Phase 11a scope: SQLite WAL snapshot, age passphrase encryption, pluggable
//! S3-compatible + local destinations, interval scheduler, LRU retention.
//!
//! Restore lives in 11b. The plugin only reads from the controller's storage;
//! it never writes to it (other than the `backup_runs` history rows).

#![allow(clippy::result_large_err)]

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use isengard_controller::ControllerHandles;
use isengard_core::{
    Capability, CoreError, Plugin, PluginContext, PluginRegistration, Result as CoreResult,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{info, warn};

pub mod config;
pub mod destination;
pub mod encrypt;
pub mod restore;
pub mod runner;
pub mod snapshot;

use crate::config::BackupConfig;
use crate::runner::BackupRunner;

const PLUGIN_NAME: &str = "backup";

/// Process-wide handle to the runner. Set by `Plugin::start` after the
/// controller hands us a `ControllerHandles` bundle. The dashboard plugin's
/// REST handlers consult this via `BackupPlugin::runner()` so they can
/// trigger run-now without holding their own pool.
static RUNNER_CELL: OnceCell<Arc<BackupRunner>> = OnceCell::const_new();

/// Public accessor used by the dashboard plugin's REST handlers. Returns
/// None if the backup plugin has not started yet.
pub fn runner_handle() -> Option<Arc<BackupRunner>> {
    RUNNER_CELL.get().cloned()
}

/// Backup plugin entry point.
pub struct BackupPlugin {
    initialized: bool,
    scheduler_task: Option<JoinHandle<()>>,
}

impl BackupPlugin {
    pub fn new() -> Self {
        Self {
            initialized: false,
            scheduler_task: None,
        }
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

fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

async fn open_pool(db_path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal);
    SqlitePool::connect_with(opts).await
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
        self.initialized = true;
        info!("backup plugin initialised");
        Ok(())
    }

    async fn start(&mut self, ctx: &PluginContext) -> CoreResult<()> {
        if !self.initialized {
            return Err(init_err("start called before init"));
        }
        let handles = ctx
            .bus
            .clone()
            .ok_or_else(|| start_err("backup started without ControllerHandles"))?
            .downcast::<ControllerHandles>()
            .map_err(|_| start_err("bus on PluginContext was not ControllerHandles"))?;

        // The backup plugin opens its own pool against the same DB file so it
        // can hold an IMMEDIATE-tx lock during a snapshot without contending
        // with the live inventory pool's writers.
        let pool = open_pool(&handles.db_path)
            .await
            .map_err(|e| start_err(format!("backup pool: {e}")))?;

        let runner = Arc::new(BackupRunner::new(
            handles.inventory.clone(),
            pool,
            handles.db_path.clone(),
        ));
        let _ = RUNNER_CELL.set(runner.clone());

        let task = spawn_scheduler(runner);
        self.scheduler_task = Some(task);

        info!("backup plugin started; scheduler armed");
        Ok(())
    }

    async fn stop(&mut self) -> CoreResult<()> {
        if let Some(t) = self.scheduler_task.take() {
            t.abort();
        }
        info!("backup plugin stopped");
        Ok(())
    }
}

/// Spawn the scheduler loop. The loop:
/// 1. Reads the current config.
/// 2. Computes the next run time from `last_successful_backup_run` + interval.
/// 3. Sleeps until then; on wake-up, fires `runner.run_once()`.
/// 4. Loops.
///
/// If the plugin is disabled or no destination is configured, the loop sleeps
/// for `MIN_INTERVAL_SECS` and re-checks. This lets the operator flip the
/// switch in the UI and have the next cycle pick it up without restarting
/// the controller.
fn spawn_scheduler(runner: Arc<BackupRunner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let cfg = match BackupConfig::load(&runner.inventory).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "scheduler: failed to load config; retrying in 60s");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };

            if !cfg.enabled {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }

            let last = match runner.inventory.last_successful_backup_run().await {
                Ok(o) => o.and_then(|r| r.finished_at).or_else(|| {
                    // No successful run yet; treat as "now" so we wait one
                    // full interval before the first attempt.
                    Some(chrono::Utc::now())
                }),
                Err(e) => {
                    warn!(error = %e, "scheduler: failed to read last run; retrying in 60s");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };

            let next = runner::next_run_at(chrono::Utc::now(), last, cfg.interval_secs);
            let now = chrono::Utc::now();
            let wait = (next - now).num_seconds().max(0) as u64;
            if wait > 0 {
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }

            if let Err(e) = runner.run_once().await {
                warn!(error = %e, "scheduler: run_once failed; will retry next interval");
            }
        }
    })
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
        assert!(p.scheduler_task.is_none());
    }

    #[test]
    fn name_is_stable() {
        let p = BackupPlugin::new();
        assert_eq!(p.name(), "backup");
    }
}
