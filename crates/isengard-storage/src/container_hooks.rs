//! `container_hooks` DAO. Phase 12b (#54).
//!
//! Stores per-container lifecycle hook configuration parsed from
//! `isengard.hooks.*` Docker labels by the controller-side
//! `HookLabelIngest`. Two read paths:
//!
//! - The lifecycle subscriber on the webhooks plugin reads
//!   `(host_id, container_name)` rows whenever a `deployment.*` event fires
//!   so it can enqueue a delivery to the matching URL.
//! - The dashboard's "Lifecycle hooks" view lists rows by host so operators
//!   can confirm what's configured.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::host::HostId;

/// One row in `container_hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerHooks {
    pub id: i64,
    pub host_id: HostId,
    pub container_id: String,
    pub container_name: String,
    pub pre_deploy_url: Option<String>,
    pub post_deploy_url: Option<String>,
    pub on_failure_url: Option<String>,
    pub secret: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Upsert payload. Empty (all `None`) means "delete this row" via the
/// `delete_*` methods rather than upserting an empty shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertContainerHooks {
    pub host_id: HostId,
    pub container_id: String,
    pub container_name: String,
    pub pre_deploy_url: Option<String>,
    pub post_deploy_url: Option<String>,
    pub on_failure_url: Option<String>,
    pub secret: Option<String>,
}

impl UpsertContainerHooks {
    /// Returns true iff at least one of the URL fields is set. The ingest
    /// uses this to decide between upsert (some URL present) and delete
    /// (no URLs at all).
    pub fn has_any_url(&self) -> bool {
        self.pre_deploy_url.is_some()
            || self.post_deploy_url.is_some()
            || self.on_failure_url.is_some()
    }
}

impl crate::inventory::Inventory {
    /// Upsert by `(host_id, container_name)`. Updates `container_id` if it
    /// has changed (rebuild after a `docker compose up` recreates the
    /// container with a new id but the same name).
    pub async fn upsert_container_hooks(
        &self,
        ins: UpsertContainerHooks,
    ) -> Result<ContainerHooks> {
        let host_bytes = ins.host_id.0.to_bytes().to_vec();
        sqlx::query(
            r#"
            INSERT INTO container_hooks
              (host_id, container_id, container_name,
               pre_deploy_url, post_deploy_url, on_failure_url, secret)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(host_id, container_name) DO UPDATE SET
              container_id    = excluded.container_id,
              pre_deploy_url  = excluded.pre_deploy_url,
              post_deploy_url = excluded.post_deploy_url,
              on_failure_url  = excluded.on_failure_url,
              secret          = excluded.secret,
              updated_at      = strftime('%Y-%m-%dT%H:%M:%fZ','now')
            "#,
        )
        .bind(&host_bytes)
        .bind(&ins.container_id)
        .bind(&ins.container_name)
        .bind(ins.pre_deploy_url.as_deref())
        .bind(ins.post_deploy_url.as_deref())
        .bind(ins.on_failure_url.as_deref())
        .bind(ins.secret.as_deref())
        .execute(self.pool())
        .await?;

        self.get_container_hooks(ins.host_id, &ins.container_name)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!(
                    "container_hooks for {}/{} not found after upsert",
                    ins.host_id, ins.container_name
                ),
            })
    }

    pub async fn get_container_hooks(
        &self,
        host_id: HostId,
        container_name: &str,
    ) -> Result<Option<ContainerHooks>> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let row = sqlx::query(
            r#"
            SELECT id, host_id, container_id, container_name,
                   pre_deploy_url, post_deploy_url, on_failure_url, secret,
                   created_at, updated_at
            FROM container_hooks
            WHERE host_id = ? AND container_name = ?
            "#,
        )
        .bind(&host_bytes)
        .bind(container_name)
        .fetch_optional(self.pool())
        .await?;
        row.map(row_to_hooks).transpose()
    }

    pub async fn list_container_hooks_by_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<ContainerHooks>> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, container_id, container_name,
                   pre_deploy_url, post_deploy_url, on_failure_url, secret,
                   created_at, updated_at
            FROM container_hooks
            WHERE host_id = ?
            ORDER BY container_name
            "#,
        )
        .bind(&host_bytes)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_hooks).collect()
    }

    pub async fn delete_container_hooks_by_name(
        &self,
        host_id: HostId,
        container_name: &str,
    ) -> Result<bool> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let r =
            sqlx::query(r#"DELETE FROM container_hooks WHERE host_id = ? AND container_name = ?"#)
                .bind(&host_bytes)
                .bind(container_name)
                .execute(self.pool())
                .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete_container_hooks_by_id(
        &self,
        host_id: HostId,
        container_id: &str,
    ) -> Result<bool> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let r =
            sqlx::query(r#"DELETE FROM container_hooks WHERE host_id = ? AND container_id = ?"#)
                .bind(&host_bytes)
                .bind(container_id)
                .execute(self.pool())
                .await?;
        Ok(r.rows_affected() > 0)
    }
}

fn row_to_hooks(r: sqlx::sqlite::SqliteRow) -> Result<ContainerHooks> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = r.try_get("host_id")?;
    let host_arr: [u8; 16] = host_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Decode {
            reason: format!(
                "container_hooks.host_id: expected 16 bytes, got {}",
                host_bytes.len()
            ),
        })?;
    let host_id = HostId(ulid::Ulid::from_bytes(host_arr));
    Ok(ContainerHooks {
        id: r.try_get("id")?,
        host_id,
        container_id: r.try_get("container_id")?,
        container_name: r.try_get("container_name")?,
        pre_deploy_url: r.try_get("pre_deploy_url")?,
        post_deploy_url: r.try_get("post_deploy_url")?,
        on_failure_url: r.try_get("on_failure_url")?,
        secret: r.try_get("secret")?,
        created_at: parse_dt(r.try_get("created_at")?)?,
        updated_at: parse_dt(r.try_get("updated_at")?)?,
    })
}

fn parse_dt(s: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&s)
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                .map(|n| n.and_utc().fixed_offset())
        })
        .map_err(|e| Error::Decode {
            reason: format!("bad timestamp '{s}': {e}"),
        })
        .map(|dt| dt.with_timezone(&Utc))
}
