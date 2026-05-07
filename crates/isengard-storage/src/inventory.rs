//! `Inventory`: the public CRUD surface over the `hosts` table.

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

use crate::error::{Error, Result};
use crate::fleet::Fleet;
use crate::host::{EnrollHost, Host, HostId};
use crate::host_action::{HostAction, HostActionId, HostActionKind};
use crate::service::{InsertService, Service, ServiceId, ServiceState};
use crate::setting::Setting;
use crate::stack::{InsertStack, Stack, StackComposeRow, StackId, StackSource};

/// Wraps a `sqlx::SqlitePool` opened against a single `.db` file.
/// Cheap to clone (the pool is `Arc`-backed inside).
#[derive(Debug, Clone)]
pub struct Inventory {
    pool: SqlitePool,
}

impl Inventory {
    /// Open (or create) the database at `path` and run all pending migrations.
    /// The parent directory must exist; the file is created if missing.
    pub async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(opts).await?;
        sqlx::migrate!().run(&pool).await?;

        Ok(Self { pool })
    }

    /// Open an in-memory database. Useful for tests; the data is wiped when
    /// the `Inventory` is dropped.
    pub async fn open_in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    /// Insert a new host. Returns the freshly assigned `HostId`. The
    /// `enrolled_at` timestamp is set to "now" (Unix seconds).
    pub async fn enroll_host(&self, req: EnrollHost) -> Result<HostId> {
        let id = HostId::new();
        let id_bytes: &[u8] = &id.to_bytes();
        let enrolled_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Ensure the fleet exists. Caller passes the fleet name (required —
        // there is no implicit 'default' fleet).
        sqlx::query("INSERT OR IGNORE INTO fleets (name) VALUES (?)")
            .bind(&req.fleet)
            .execute(&self.pool)
            .await?;

        sqlx::query(
            r#"
            INSERT INTO hosts (
                id, fingerprint, hostname, os, arch,
                agent_version, docker_version, enrolled_at, fleet
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id_bytes)
        .bind(&req.fingerprint)
        .bind(&req.hostname)
        .bind(&req.os)
        .bind(&req.arch)
        .bind(&req.agent_version)
        .bind(&req.docker_version)
        .bind(enrolled_at)
        .bind(&req.fleet)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Look up a host by id. Returns `None` if no row matches.
    pub async fn get_host(&self, id: HostId) -> Result<Option<Host>> {
        let id_bytes: &[u8] = &id.to_bytes();

        let row: Option<HostRow> = sqlx::query_as(
            r#"
            SELECT id, fingerprint, hostname, os, arch,
                   agent_version, docker_version, enrolled_at, last_seen_at, metadata, fleet
            FROM hosts
            WHERE id = ?
            "#,
        )
        .bind(id_bytes)
        .fetch_optional(&self.pool)
        .await?;

        row.map(decode_host).transpose()
    }

    /// Update `last_seen_at` for a host. No-op if the host doesn't exist.
    /// Returns whether a row was actually updated.
    pub async fn touch_host(&self, id: HostId, ts: i64) -> Result<bool> {
        let id_bytes: &[u8] = &id.to_bytes();
        let result = sqlx::query("UPDATE hosts SET last_seen_at = ? WHERE id = ?")
            .bind(ts)
            .bind(id_bytes)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Remove a host from the inventory. Returns true if a row was deleted.
    pub async fn delete_host(&self, id: HostId) -> Result<bool> {
        let id_bytes: &[u8] = &id.to_bytes();
        let result = sqlx::query("DELETE FROM hosts WHERE id = ?")
            .bind(id_bytes)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Return every host, ordered by `last_seen_at DESC` (recently active first;
    /// hosts never seen sort to the bottom).
    pub async fn list_hosts(&self) -> Result<Vec<Host>> {
        let rows: Vec<HostRow> = sqlx::query_as(
            r#"
            SELECT id, fingerprint, hostname, os, arch,
                   agent_version, docker_version, enrolled_at, last_seen_at, metadata, fleet
            FROM hosts
            ORDER BY last_seen_at DESC NULLS LAST, enrolled_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(decode_host).collect()
    }

    /// Borrow the underlying pool. Used by inventory methods (and tests that
    /// want to peek at table state).
    #[allow(dead_code)] // only consumed by tests; lib build sees it as unused
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn insert_stack(&self, req: InsertStack) -> Result<StackId> {
        // Upsert: SQLite's INSERT ... ON CONFLICT DO UPDATE doesn't return the
        // existing id, so we INSERT OR IGNORE then SELECT to fetch the id.
        sqlx::query("INSERT OR IGNORE INTO stacks (host_id, name, source) VALUES (?, ?, ?)")
            .bind(req.host_id.to_bytes().as_slice())
            .bind(&req.name)
            .bind(req.source.as_str())
            .execute(&self.pool)
            .await?;

        let row = sqlx::query("SELECT id FROM stacks WHERE host_id = ? AND name = ?")
            .bind(req.host_id.to_bytes().as_slice())
            .bind(&req.name)
            .fetch_one(&self.pool)
            .await?;
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        Ok(StackId(id))
    }

    pub async fn list_stacks(&self, host_id: Option<HostId>) -> Result<Vec<Stack>> {
        let rows = match host_id {
            Some(h) => {
                sqlx::query("SELECT id, host_id, name, source, discovered_at FROM stacks WHERE host_id = ? ORDER BY name")
                    .bind(h.to_bytes().as_slice())
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("SELECT id, host_id, name, source, discovered_at FROM stacks ORDER BY name")
                    .fetch_all(&self.pool)
                    .await?
            }
        };

        rows.into_iter().map(stack_from_row).collect()
    }

    pub async fn get_stack(&self, id: StackId) -> Result<Option<Stack>> {
        let row =
            sqlx::query("SELECT id, host_id, name, source, discovered_at FROM stacks WHERE id = ?")
                .bind(id.0)
                .fetch_optional(&self.pool)
                .await?;
        row.map(stack_from_row).transpose()
    }

    pub async fn delete_stack(&self, id: StackId) -> Result<bool> {
        let result = sqlx::query("DELETE FROM stacks WHERE id = ?")
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// v0.3c compose import: persist the YAML the agent reverse-engineers
    /// from the running containers in `stack`. `imported_at` is RFC3339.
    /// Idempotent: callers may invoke with the same payload on every
    /// reconnect without churning the row.
    pub async fn set_stack_compose(
        &self,
        host_id: HostId,
        stack_name: &str,
        compose_yaml: &str,
        sha256: &str,
        imported_at: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE stacks
               SET compose_yaml         = ?,
                   compose_sha256       = ?,
                   compose_imported_at  = ?
             WHERE host_id = ? AND name = ?
            "#,
        )
        .bind(compose_yaml)
        .bind(sha256)
        .bind(imported_at)
        .bind(host_id.to_bytes().as_slice())
        .bind(stack_name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// v0.3c compose import: read the stored YAML for a stack. Returns
    /// `Ok(None)` if the row exists but no import has been recorded yet.
    pub async fn get_stack_compose(&self, id: StackId) -> Result<Option<StackComposeRow>> {
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT compose_yaml, compose_sha256, compose_imported_at FROM stacks WHERE id = ?",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let yaml: Option<String> = row.try_get("compose_yaml")?;
        let sha: Option<String> = row.try_get("compose_sha256")?;
        let imported_at: Option<String> = row.try_get("compose_imported_at")?;
        match (yaml, sha, imported_at) {
            (Some(yaml), Some(sha256), Some(imported_at)) => Ok(Some(StackComposeRow {
                yaml,
                sha256,
                imported_at,
            })),
            _ => Ok(None),
        }
    }

    pub async fn set_host_fleet(&self, id: HostId, fleet: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE hosts SET fleet = ? WHERE id = ?")
            .bind(fleet)
            .bind(id.to_bytes().as_slice())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn insert_service(&self, req: InsertService) -> Result<ServiceId> {
        use sqlx::Row;
        sqlx::query(
            "INSERT INTO services (host_id, stack_id, name, image, state, last_seen_at)
             VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(host_id, name) DO UPDATE SET
                 stack_id     = excluded.stack_id,
                 image        = excluded.image,
                 state        = excluded.state,
                 last_seen_at = CURRENT_TIMESTAMP",
        )
        .bind(req.host_id.to_bytes().as_slice())
        .bind(req.stack_id.map(|s| s.0))
        .bind(&req.name)
        .bind(&req.image)
        .bind(req.state.as_str())
        .execute(&self.pool)
        .await?;

        let row = sqlx::query("SELECT id FROM services WHERE host_id = ? AND name = ?")
            .bind(req.host_id.to_bytes().as_slice())
            .bind(&req.name)
            .fetch_one(&self.pool)
            .await?;
        Ok(ServiceId(row.try_get("id")?))
    }

    pub async fn list_services(&self, stack_id: Option<StackId>) -> Result<Vec<Service>> {
        let rows = match stack_id {
            Some(s) => {
                sqlx::query(
                    "SELECT id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override \
                     FROM services WHERE stack_id = ? ORDER BY name",
                )
                .bind(s.0)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override \
                     FROM services ORDER BY name",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(service_from_row).collect()
    }

    pub async fn get_service(&self, id: ServiceId) -> Result<Option<Service>> {
        let row = sqlx::query(
            "SELECT id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override \
             FROM services WHERE id = ?",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.map(service_from_row).transpose()
    }

    pub async fn get_service_by_name(
        &self,
        host_id: HostId,
        stack_id: Option<StackId>,
        service_name: &str,
    ) -> Result<Option<Service>> {
        let row = match stack_id {
            Some(sid) => {
                sqlx::query(
                    "SELECT id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override \
                     FROM services \
                     WHERE host_id = ? AND stack_id = ? AND name = ?",
                )
                .bind(host_id.to_bytes().as_slice())
                .bind(sid.0)
                .bind(service_name)
                .fetch_optional(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override \
                     FROM services \
                     WHERE host_id = ? AND stack_id IS NULL AND name = ?",
                )
                .bind(host_id.to_bytes().as_slice())
                .bind(service_name)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        row.map(service_from_row).transpose()
    }

    pub async fn set_service_deploy_strategy_override(
        &self,
        service_id: ServiceId,
        override_value: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE services SET deploy_strategy_override = ? WHERE id = ?")
            .bind(override_value)
            .bind(service_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_service(&self, id: ServiceId) -> Result<bool> {
        let r = sqlx::query("DELETE FROM services WHERE id = ?")
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn queue_action(
        &self,
        host_id: HostId,
        kind: HostActionKind,
    ) -> Result<HostActionId> {
        let kind_str = kind.kind_str();
        let payload = kind.payload_json();
        let r =
            sqlx::query("INSERT INTO host_actions (host_id, kind, payload_json) VALUES (?, ?, ?)")
                .bind(host_id.to_bytes().as_slice())
                .bind(kind_str)
                .bind(payload)
                .execute(&self.pool)
                .await?;
        Ok(HostActionId(r.last_insert_rowid()))
    }

    pub async fn pending_actions(&self, host_id: HostId) -> Result<Vec<HostAction>> {
        let rows = sqlx::query(
            "SELECT id, host_id, kind, payload_json, created_at, delivered_at, result \
             FROM host_actions WHERE host_id = ? AND delivered_at IS NULL ORDER BY id",
        )
        .bind(host_id.to_bytes().as_slice())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(host_action_from_row).collect()
    }

    pub async fn mark_action_delivered(&self, id: HostActionId, result: &str) -> Result<()> {
        sqlx::query(
            "UPDATE host_actions SET delivered_at = CURRENT_TIMESTAMP, result = ? WHERE id = ?",
        )
        .bind(result)
        .bind(id.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_fleet(&self, name: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO fleets (name) VALUES (?)")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_fleets(&self) -> Result<Vec<Fleet>> {
        use sqlx::Row;
        let rows = sqlx::query("SELECT name, created_at FROM fleets ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Fleet {
                    name: r.try_get("name")?,
                    created_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }

    pub async fn delete_fleet(&self, name: &str) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hosts WHERE fleet = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await?;
        if count > 0 {
            return Err(Error::Conflict(format!(
                "fleet '{name}' has {count} hosts assigned"
            )));
        }
        let r = sqlx::query("DELETE FROM fleets WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        let json = serde_json::to_string(value).map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
        sqlx::query(
            "INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(key)
        .bind(&json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        use sqlx::Row;
        let row = sqlx::query("SELECT value_json FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(r) => {
                let json: String = r.try_get("value_json")?;
                Ok(Some(serde_json::from_str(&json).map_err(|e| {
                    Error::Decode {
                        reason: e.to_string(),
                    }
                })?))
            }
            None => Ok(None),
        }
    }

    pub async fn list_settings(&self) -> Result<Vec<Setting>> {
        use sqlx::Row;
        let rows = sqlx::query("SELECT key, value_json, updated_at FROM settings ORDER BY key")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let json: String = r.try_get("value_json")?;
                let value = serde_json::from_str(&json).map_err(|e| Error::Decode {
                    reason: e.to_string(),
                })?;
                Ok(Setting {
                    key: r.try_get("key")?,
                    value,
                    updated_at: r.try_get("updated_at")?,
                })
            })
            .collect()
    }
}

fn stack_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Stack> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    if host_bytes.len() != 16 {
        return Err(Error::InvalidHostId(host_bytes.len()));
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);
    let source_str: String = row.try_get("source")?;
    let source = StackSource::from_str(&source_str).ok_or_else(|| Error::Decode {
        reason: format!("unknown stack source: {source_str}"),
    })?;
    let discovered_at: chrono::DateTime<chrono::Utc> = row.try_get("discovered_at")?;
    Ok(Stack {
        id: StackId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        name: row.try_get("name")?,
        source,
        discovered_at,
    })
}

fn service_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Service> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    if host_bytes.len() != 16 {
        return Err(Error::InvalidHostId(host_bytes.len()));
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);
    Ok(Service {
        id: ServiceId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        stack_id: row.try_get::<Option<i64>, _>("stack_id")?.map(StackId),
        name: row.try_get("name")?,
        image: row.try_get("image")?,
        state: ServiceState::from_str(&row.try_get::<String, _>("state")?),
        last_seen_at: row.try_get("last_seen_at")?,
        deploy_strategy_override: row
            .try_get::<Option<String>, _>("deploy_strategy_override")
            .unwrap_or(None),
    })
}

fn host_action_from_row(row: sqlx::sqlite::SqliteRow) -> Result<HostAction> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    if host_bytes.len() != 16 {
        return Err(Error::InvalidHostId(host_bytes.len()));
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);
    let payload: String = row.try_get("payload_json")?;
    let kind: HostActionKind = serde_json::from_str(&payload).map_err(|e| Error::Decode {
        reason: format!("bad host_action payload: {e}"),
    })?;
    Ok(HostAction {
        id: HostActionId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        kind,
        created_at: row.try_get("created_at")?,
        delivered_at: row.try_get("delivered_at")?,
        result: row.try_get("result")?,
    })
}

type HostRow = (
    Vec<u8>,     // id
    String,      // fingerprint
    String,      // hostname
    String,      // os
    String,      // arch
    String,      // agent_version
    String,      // docker_version
    i64,         // enrolled_at
    Option<i64>, // last_seen_at
    String,      // metadata (json text)
    String,      // fleet
);

fn decode_host(row: HostRow) -> Result<Host> {
    let id_bytes: [u8; 16] = row
        .0
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidHostId(row.0.len()))?;
    let metadata: serde_json::Value = serde_json::from_str(&row.9).map_err(|e| Error::Decode {
        reason: format!("metadata json: {e}"),
    })?;

    Ok(Host {
        id: HostId::from_bytes(id_bytes),
        fingerprint: row.1,
        hostname: row.2,
        os: row.3,
        arch: row.4,
        agent_version: row.5,
        docker_version: row.6,
        enrolled_at: row.7,
        last_seen_at: row.8,
        metadata,
        fleet: row.10,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn open_creates_file_and_runs_migrations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("isengard.db");

        let inv = Inventory::open(&path).await.expect("open");
        assert!(path.exists(), "db file should be created");

        // Migration should have created the hosts table — check by querying
        // sqlite_master (sqlite's catalog).
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='hosts'",
        )
        .fetch_one(inv.pool())
        .await
        .expect("query");
        assert_eq!(row.0, 1, "hosts table should exist after migrate");
    }

    #[tokio::test]
    async fn open_in_memory_runs_migrations() {
        let inv = Inventory::open_in_memory().await.expect("open");
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='hosts_last_seen_at_idx'",
        )
        .fetch_one(inv.pool())
        .await
        .expect("query");
        assert_eq!(row.0, 1, "last_seen_at index should exist");
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("isengard.db");

        let _inv1 = Inventory::open(&path).await.expect("open 1");
        // Reopen the same file — migrations should be a no-op the second time.
        let _inv2 = Inventory::open(&path).await.expect("open 2");
    }

    fn sample_enrollment() -> EnrollHost {
        EnrollHost {
            fingerprint: "ada-lovelace.example".into(),
            hostname: "ada-lovelace".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0-alpha".into(),
            docker_version: "27.4.0".into(),
            fleet: "default".into(),
        }
    }

    #[tokio::test]
    async fn enroll_then_get_round_trips() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let req = sample_enrollment();
        let id = inv.enroll_host(req.clone()).await.unwrap();

        let got = inv.get_host(id).await.unwrap().expect("host should exist");

        assert_eq!(got.id, id);
        assert_eq!(got.fingerprint, req.fingerprint);
        assert_eq!(got.hostname, req.hostname);
        assert_eq!(got.os, req.os);
        assert_eq!(got.arch, req.arch);
        assert_eq!(got.agent_version, req.agent_version);
        assert_eq!(got.docker_version, req.docker_version);
        assert!(got.enrolled_at > 0);
        assert_eq!(got.last_seen_at, None);
        assert_eq!(got.metadata, serde_json::json!({}));
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_id() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let result = inv.get_host(HostId::new()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn duplicate_fingerprint_is_rejected() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let req = sample_enrollment();
        let _ = inv.enroll_host(req.clone()).await.unwrap();
        let err = inv
            .enroll_host(req)
            .await
            .expect_err("dup fingerprint must error");
        assert!(matches!(err, Error::Db(_)), "unexpected error: {err:?}");
    }

    #[tokio::test]
    async fn touch_updates_last_seen_for_known_host() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let id = inv.enroll_host(sample_enrollment()).await.unwrap();

        let updated = inv.touch_host(id, 1_700_000_000).await.unwrap();
        assert!(updated);

        let host = inv.get_host(id).await.unwrap().unwrap();
        assert_eq!(host.last_seen_at, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn touch_unknown_host_returns_false() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let updated = inv.touch_host(HostId::new(), 1_700_000_000).await.unwrap();
        assert!(!updated);
    }

    #[tokio::test]
    async fn list_returns_recently_seen_first() {
        let inv = Inventory::open_in_memory().await.unwrap();

        // Enroll two hosts with different fingerprints.
        let mut req_a = sample_enrollment();
        req_a.fingerprint = "host-a.example".into();
        let id_a = inv.enroll_host(req_a).await.unwrap();

        let mut req_b = sample_enrollment();
        req_b.fingerprint = "host-b.example".into();
        let id_b = inv.enroll_host(req_b).await.unwrap();

        // Touch B more recently than A.
        inv.touch_host(id_a, 1_700_000_000).await.unwrap();
        inv.touch_host(id_b, 1_700_000_500).await.unwrap();

        let listed = inv.list_hosts().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, id_b, "more recent host should come first");
        assert_eq!(listed[1].id, id_a);
    }

    #[tokio::test]
    async fn list_empty_inventory_returns_empty_vec() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let listed = inv.list_hosts().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn delete_host_removes_entry() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let enroll = EnrollHost {
            fingerprint: "fp-delete".into(),
            hostname: "h1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "test".into(),
            docker_version: "test".into(),
            fleet: "default".into(),
        };
        let id = inv.enroll_host(enroll).await.unwrap();
        let removed = inv.delete_host(id).await.unwrap();
        assert!(removed);
        assert!(inv.get_host(id).await.unwrap().is_none());
        let removed_again = inv.delete_host(id).await.unwrap();
        assert!(!removed_again);
    }

    #[tokio::test]
    async fn insert_stack_is_idempotent_per_host_and_name() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp1".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();

        let id1 = inv
            .insert_stack(InsertStack {
                host_id,
                name: "wordpress".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let id2 = inv
            .insert_stack(InsertStack {
                host_id,
                name: "wordpress".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        assert_eq!(
            id1, id2,
            "second insert with same (host_id, name) should return the same id"
        );

        let listed = inv.list_stacks(Some(host_id)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "wordpress");
    }

    #[tokio::test]
    async fn set_host_fleet_updates_existing_host() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp1".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();

        let updated = inv.set_host_fleet(host_id, "prod").await.unwrap();
        assert!(updated, "set_host_fleet should return true when row exists");

        let host = inv.get_host(host_id).await.unwrap().unwrap();
        assert_eq!(host.fleet, "prod");

        let missing = HostId::new();
        let updated = inv.set_host_fleet(missing, "prod").await.unwrap();
        assert!(
            !updated,
            "set_host_fleet should return false when row does not exist"
        );
    }

    #[tokio::test]
    async fn insert_service_upserts_by_host_and_name() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = test_enroll_h(&inv, "h1").await;

        let id1 = inv
            .insert_service(InsertService {
                host_id,
                stack_id: None,
                name: "web".into(),
                image: "nginx:1".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();

        let id2 = inv
            .insert_service(InsertService {
                host_id,
                stack_id: None,
                name: "web".into(),
                image: "nginx:2".into(),
                state: ServiceState::Restarting,
            })
            .await
            .unwrap();

        assert_eq!(id1, id2);

        let svcs = inv.list_services(None).await.unwrap();
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].image, "nginx:2");
        assert!(matches!(svcs[0].state, ServiceState::Restarting));
    }

    #[tokio::test]
    async fn queue_and_deliver_host_action() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = test_enroll_h(&inv, "h1").await;

        let action_id = inv
            .queue_action(host_id, HostActionKind::ForceUpdate { stack_name: None })
            .await
            .unwrap();

        let pending = inv.pending_actions(host_id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, action_id);

        inv.mark_action_delivered(action_id, "ok").await.unwrap();
        let pending = inv.pending_actions(host_id).await.unwrap();
        assert!(
            pending.is_empty(),
            "delivered actions are not returned by pending_actions"
        );
    }

    #[tokio::test]
    async fn fleets_create_list_delete() {
        let inv = Inventory::open_in_memory().await.unwrap();

        inv.create_fleet("staging").await.unwrap();
        inv.create_fleet("prod").await.unwrap();

        let fleets = inv.list_fleets().await.unwrap();
        let names: std::collections::HashSet<_> = fleets.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains("staging"));
        assert!(names.contains("prod"));
        assert!(
            !names.contains("default"),
            "default fleet must NOT exist before any host is enrolled"
        );

        let deleted = inv.delete_fleet("staging").await.unwrap();
        assert!(deleted);

        let host_id = test_enroll_h(&inv, "h-prod").await;
        inv.set_host_fleet(host_id, "prod").await.unwrap();
        let err = inv.delete_fleet("prod").await;
        assert!(err.is_err(), "fleet with hosts should not be deletable");
    }

    #[tokio::test]
    async fn settings_round_trip() {
        let inv = Inventory::open_in_memory().await.unwrap();

        inv.set_setting("notifier.telegram.enabled", &serde_json::json!(true))
            .await
            .unwrap();

        let v = inv.get_setting("notifier.telegram.enabled").await.unwrap();
        assert_eq!(v, Some(serde_json::json!(true)));

        let missing = inv.get_setting("nope").await.unwrap();
        assert!(missing.is_none());
    }

    async fn test_enroll_h(inv: &Inventory, hostname: &str) -> HostId {
        inv.enroll_host(EnrollHost {
            fingerprint: format!("fp-{hostname}"),
            hostname: hostname.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap()
    }
}
