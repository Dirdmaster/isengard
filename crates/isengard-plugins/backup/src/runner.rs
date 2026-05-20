//! Backup runner.
//!
//! Orchestrates snapshot, encrypt, upload, retention prune; records
//! each step in `backup_runs`. The runner is the shared code path
//! between the scheduler (interval timer) and the REST `run-now`
//! endpoint.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use isengard_storage::{BackupRunId, Inventory};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::config::{BackupConfig, DestinationConfig};
use crate::destination::{BackupDestination, LocalDestination, S3Config, S3Destination};
use crate::encrypt::encrypt_with_passphrase;
use crate::restore::{RestoreError, RestoreOutcome, restore_from_destination};
use crate::snapshot::create_snapshot;

/// Env var the operator must set so the controller can encrypt
/// snapshots. Leaving this unset is a hard failure for any
/// scheduled or manual run.
pub const PASSPHRASE_ENV: &str = "ISENGARD_BACKUP_PASSPHRASE";

/// Errors that can happen during a single run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Plugin is disabled in config.
    #[error("backup is disabled in config")]
    Disabled,

    /// No destination configured.
    #[error("no destination configured")]
    NoDestination,

    /// Passphrase env var not set.
    #[error("passphrase not set: export {0}=<value>")]
    NoPassphrase(&'static str),

    /// Snapshot stage failed.
    #[error("snapshot: {0}")]
    Snapshot(#[from] crate::snapshot::SnapshotError),

    /// Encryption stage failed.
    #[error("encrypt: {0}")]
    Encrypt(#[from] crate::encrypt::EncryptError),

    /// Destination upload or list failed.
    #[error("destination: {0}")]
    Destination(#[from] crate::destination::DestinationError),

    /// Storage DAO failed (e.g. inserting the run row).
    #[error("storage: {0}")]
    Storage(#[from] isengard_storage::Error),

    /// Filesystem IO failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Bundle of references the runner needs.
pub struct BackupRunner {
    /// Inventory the runner reads from and records history into.
    pub inventory: Arc<Inventory>,
    /// Dedicated SQLite pool used to take the snapshot.
    pub pool: SqlitePool,
    /// On-disk path of the live DB file.
    pub db_path: PathBuf,
}

impl BackupRunner {
    /// Builds a runner from the controller's handles.
    pub fn new(inventory: Arc<Inventory>, pool: SqlitePool, db_path: PathBuf) -> Self {
        Self {
            inventory,
            pool,
            db_path,
        }
    }

    /// Resolves a [`DestinationConfig`] into a boxed
    /// [`BackupDestination`].
    ///
    /// Visible for tests and for the dashboard's "test connection"
    /// handler.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NoDestination`] when the config is
    /// [`DestinationConfig::None`].
    pub fn build_destination(
        &self,
        cfg: &DestinationConfig,
    ) -> Result<Box<dyn BackupDestination>, RunError> {
        match cfg {
            DestinationConfig::None => Err(RunError::NoDestination),
            DestinationConfig::Local { root, prefix } => Ok(Box::new(LocalDestination::new(
                root.clone(),
                prefix.clone(),
            ))),
            DestinationConfig::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                secret_access_key,
            } => Ok(Box::new(S3Destination::new(S3Config {
                endpoint: endpoint.clone(),
                region: region.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
            }))),
        }
    }

    /// Picks an object name for a new snapshot.
    ///
    /// Embeds the timestamp in ISO 8601 basic format so retention
    /// can sort lexicographically.
    pub fn snapshot_name(now: chrono::DateTime<Utc>) -> String {
        format!("snapshot-{}.db.age", now.format("%Y%m%dT%H%M%SZ"))
    }

    /// Runs one full backup cycle.
    ///
    /// Disabled config returns [`RunError::Disabled`] without
    /// inserting a `backup_runs` row (no run was attempted). All
    /// other failure paths insert a row and transition it to
    /// `failed` so the operator sees the attempt in history.
    ///
    /// # Errors
    ///
    /// Returns the [`RunError`] that caused the run to fail. The
    /// row state is written before returning.
    pub async fn run_once(&self) -> Result<BackupRunId, RunError> {
        let cfg = BackupConfig::load(&self.inventory).await?;
        if !cfg.enabled {
            return Err(RunError::Disabled);
        }

        let started = Utc::now();
        let run_id = self.inventory.insert_backup_run(started).await?;

        let outcome = self.do_run_inner(&cfg).await;

        match outcome {
            Ok((object_name, size)) => {
                self.inventory
                    .finish_backup_run_success(run_id, Utc::now(), &object_name, size as i64)
                    .await?;
                info!(run_id = run_id.0, object = %object_name, size, "backup run succeeded");
                Ok(run_id)
            }
            Err(e) => {
                let msg = e.to_string();
                if let Err(e2) = self
                    .inventory
                    .finish_backup_run_failed(run_id, Utc::now(), &msg)
                    .await
                {
                    warn!(error = %e2, run_id = run_id.0, "failed to mark run as failed");
                }
                warn!(run_id = run_id.0, error = %msg, "backup run failed");
                Err(e)
            }
        }
    }

    /// Inner work: validates the destination and passphrase, then
    /// takes and ships the snapshot.
    ///
    /// Errors here surface as `failed` rows in `backup_runs`.
    async fn do_run_inner(&self, cfg: &BackupConfig) -> Result<(String, usize), RunError> {
        let dest = self.build_destination(&cfg.destination)?;
        let passphrase =
            std::env::var(PASSPHRASE_ENV).map_err(|_| RunError::NoPassphrase(PASSPHRASE_ENV))?;
        self.do_run(cfg, dest.as_ref(), &passphrase).await
    }

    /// Snapshot, encrypt, upload, prune.
    ///
    /// Retention prune failures log a warn and don't fail the run:
    /// the upload already succeeded; stale objects hanging around
    /// are harmless.
    async fn do_run(
        &self,
        cfg: &BackupConfig,
        dest: &dyn BackupDestination,
        passphrase: &str,
    ) -> Result<(String, usize), RunError> {
        let snap = create_snapshot(&self.pool, &self.db_path).await?;
        let plain = std::fs::read(snap.path())?;
        let cipher = encrypt_with_passphrase(&plain, passphrase)?;
        let name = Self::snapshot_name(Utc::now());
        dest.upload(&name, &cipher).await?;
        let size = cipher.len();

        if let Err(e) = self.prune_retention(dest, cfg.retention_keep).await {
            warn!(error = %e, "retention prune failed (upload succeeded)");
        }

        Ok((name, size))
    }

    /// Deletes everything past the `keep` most recent objects on
    /// `dest`.
    ///
    /// Sorts newest-first by name; names embed UTC timestamps, so
    /// lexical sort is chronological.
    async fn prune_retention(
        &self,
        dest: &dyn BackupDestination,
        keep: u32,
    ) -> Result<(), RunError> {
        let mut listed = dest.list().await?;
        if (listed.len() as u32) <= keep {
            return Ok(());
        }
        listed.sort_by(|a, b| b.name.cmp(&a.name));
        for obj in listed.into_iter().skip(keep as usize) {
            dest.delete(&obj.name).await?;
        }
        Ok(())
    }

    /// Runs a restore from the configured destination.
    ///
    /// Used by the dashboard's REST handler. Wraps
    /// [`restore_from_destination`] after looking up the source
    /// backup-run id (when the object name matches a known row) and
    /// resolving the destination from the persisted config.
    ///
    /// # Errors
    ///
    /// Bubbles any [`RestoreError`] from the underlying restore.
    /// Returns [`RestoreError::Swap`] when no destination is
    /// configured or when destination resolution fails.
    pub async fn restore_now(
        &self,
        object_name: &str,
        passphrase: &str,
        dry_run: bool,
    ) -> Result<RestoreOutcome, RestoreError> {
        let cfg = BackupConfig::load(&self.inventory)
            .await
            .map_err(RestoreError::Storage)?;
        let dest = match self.build_destination(&cfg.destination) {
            Ok(d) => d,
            Err(RunError::NoDestination) => {
                return Err(RestoreError::Swap(
                    "no destination configured (run setup first)".into(),
                ));
            }
            Err(other) => {
                return Err(RestoreError::Swap(format!(
                    "destination resolution failed: {other}"
                )));
            }
        };

        let runs = self.inventory.list_backup_runs(200).await.ok();
        let source_id = runs.and_then(|rs| {
            rs.into_iter()
                .find(|r| r.object_name.as_deref() == Some(object_name))
                .map(|r| r.id.0)
        });

        restore_from_destination(
            &self.inventory,
            &self.db_path,
            dest.as_ref(),
            object_name,
            source_id,
            passphrase,
            dry_run,
        )
        .await
    }
}

/// Computes the time the next scheduled run should fire.
///
/// `last_run_at = None` returns `now + interval` (treats startup as
/// "wait one full interval"). The interval clamps up to
/// [`crate::config::MIN_INTERVAL_SECS`].
pub fn next_run_at(
    now: chrono::DateTime<Utc>,
    last_run_at: Option<chrono::DateTime<Utc>>,
    interval_secs: u64,
) -> chrono::DateTime<Utc> {
    let interval =
        chrono::Duration::seconds(interval_secs.max(crate::config::MIN_INTERVAL_SECS) as i64);
    match last_run_at {
        Some(t) => t + interval,
        None => now + interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MIN_INTERVAL_SECS;
    use chrono::TimeZone;

    #[test]
    fn snapshot_name_embeds_iso_basic_timestamp() {
        let t = Utc.with_ymd_and_hms(2026, 5, 6, 12, 34, 56).unwrap();
        let name = BackupRunner::snapshot_name(t);
        assert_eq!(name, "snapshot-20260506T123456Z.db.age");
    }

    #[test]
    fn next_run_falls_after_interval() {
        let now = Utc.with_ymd_and_hms(2026, 5, 6, 0, 0, 0).unwrap();
        let later = next_run_at(now, Some(now), 3600);
        assert_eq!(later, now + chrono::Duration::seconds(3600));
    }

    #[test]
    fn next_run_with_no_history_uses_now_plus_interval() {
        let now = Utc.with_ymd_and_hms(2026, 5, 6, 0, 0, 0).unwrap();
        let later = next_run_at(now, None, 3600);
        assert_eq!(later, now + chrono::Duration::seconds(3600));
    }

    #[test]
    fn next_run_clamps_short_interval() {
        let now = Utc.with_ymd_and_hms(2026, 5, 6, 0, 0, 0).unwrap();
        let later = next_run_at(now, None, 5);
        assert!(later >= now + chrono::Duration::seconds(MIN_INTERVAL_SECS as i64));
    }
}
