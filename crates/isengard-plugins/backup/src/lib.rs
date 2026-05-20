#![doc = include_str!("../docs/_crate.md")]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]
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

/// Stable plugin name surfaced to the controller and host registry.
const PLUGIN_NAME: &str = "backup";

/// Process-wide handle to the runner.
///
/// Set by [`Plugin::start`] after the controller hands a
/// [`ControllerHandles`] to the plugin. The dashboard plugin's REST
/// handlers consult this via [`runner_handle`] so they can trigger
/// run-now without holding their own pool.
static RUNNER_CELL: OnceCell<Arc<BackupRunner>> = OnceCell::const_new();

/// Returns the process-wide [`BackupRunner`] handle.
///
/// Returns `None` until the plugin has been started.
pub fn runner_handle() -> Option<Arc<BackupRunner>> {
    RUNNER_CELL.get().cloned()
}

/// Controller-side backup plugin instance.
///
/// Holds the initialization flag and the join handle for the spawned
/// scheduler task.
pub struct BackupPlugin {
    /// `true` once [`Plugin::init`] has run.
    initialized: bool,
    /// Join handle for the scheduler task.
    scheduler_task: Option<JoinHandle<()>>,
}

impl BackupPlugin {
    /// Builds an empty plugin. The scheduler is spawned in
    /// [`Plugin::start`].
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

/// Wraps any displayable error into [`CoreError::InitFailed`] for the
/// backup plugin.
fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Wraps any displayable error into [`CoreError::StartFailed`] for the
/// backup plugin.
fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Opens a dedicated SQLite pool against the same DB file the
/// controller is using.
///
/// The plugin opens its own pool so it can hold an IMMEDIATE-tx lock
/// during a snapshot without contending with the live inventory
/// pool's writers.
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

    /// Opens the plugin's dedicated pool, builds the runner, stores
    /// it in [`RUNNER_CELL`], and spawns the scheduler loop.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InitFailed`] if start runs before init.
    /// Returns [`CoreError::StartFailed`] when the plugin context
    /// lacks a [`ControllerHandles`] or when the dedicated pool fails
    /// to open.
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

/// Spawns the scheduler loop.
///
/// The loop:
///
/// 1. Reads the current config.
/// 2. Computes the next run time from
///    `last_successful_backup_run + interval`.
/// 3. Sleeps until then; on wake-up fires [`BackupRunner::run_once`].
/// 4. Loops.
///
/// When the plugin is disabled or no destination is configured the
/// loop sleeps for 60s and re-checks: the operator can flip the
/// toggle and have the next cycle pick it up without a controller
/// restart.
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
                Ok(o) => o
                    .and_then(|r| r.finished_at)
                    .or_else(|| Some(chrono::Utc::now())),
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
