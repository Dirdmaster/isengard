//! `placements` + `agent_labels` DAO. Phase 0.14.
//!
//! Two tables, two surfaces:
//!
//! - `agent_labels`: per-host label key/value pairs reported on every
//!   heartbeat. Replaced wholesale via [`replace_agent_labels`] when the
//!   agent's set changes. The scheduler reads via [`list_agent_labels`]
//!   without a heartbeat round-trip.
//! - `placements`: scheduler-owned assignment of a service replica to a
//!   host. One row per `(service_id, replica_index)` in steady state.
//!   Inserts go through [`upsert_placement`]; the scheduler diffs against
//!   `list_placements_by_service` on every reconcile tick.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::error::{Error, Result};
use crate::host::HostId;
use crate::service::ServiceId;

/// The state of a single placement row. The lifecycle is:
///
/// ```text
/// pending -> active -> draining -> (deleted)
///                  \-> failed (terminal until operator clears)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlacementState {
    Pending,
    Active,
    Draining,
    Failed,
}

impl PlacementState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "active" => Ok(Self::Active),
            "draining" => Ok(Self::Draining),
            "failed" => Ok(Self::Failed),
            other => Err(Error::Decode {
                reason: format!("unknown placement state '{other}'"),
            }),
        }
    }
}

/// One row from the `placements` table. `replica_index` is zero-based.
/// `last_event` is a free-form JSON string the scheduler writes when it
/// emits a placement.* event so an operator can correlate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRow {
    pub id: i64,
    pub service_id: ServiceId,
    pub host_id: HostId,
    pub replica_index: u32,
    pub state: PlacementState,
    pub assigned_at: DateTime<Utc>,
    pub last_event: Option<String>,
}

/// Insert / upsert payload. `replica_index` is the caller's responsibility:
/// the scheduler computes it before calling here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertPlacement {
    pub service_id: ServiceId,
    pub host_id: HostId,
    pub replica_index: u32,
    pub state: PlacementState,
    pub last_event: Option<String>,
}

impl crate::inventory::Inventory {
    // ===== agent_labels =====

    /// Read the full label set for a host. Returns an empty map when no rows
    /// exist (older agents, or pre-first-heartbeat).
    pub async fn list_agent_labels(&self, host_id: HostId) -> Result<BTreeMap<String, String>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let rows =
            sqlx::query(r#"SELECT key, value FROM agent_labels WHERE host_id = ? ORDER BY key"#)
                .bind(&host_bytes)
                .fetch_all(self.pool())
                .await?;
        let mut out = BTreeMap::new();
        for r in rows {
            let key: String = r.try_get("key")?;
            let value: String = r.try_get("value")?;
            out.insert(key, value);
        }
        Ok(out)
    }

    /// Replace the host's label set wholesale. Deletes any rows not present
    /// in `labels`. Runs in a single transaction so an in-flight reader
    /// either sees the old set in full or the new set in full.
    pub async fn replace_agent_labels(
        &self,
        host_id: HostId,
        labels: &BTreeMap<String, String>,
    ) -> Result<()> {
        let host_bytes = host_id.to_bytes().to_vec();
        let mut tx = self.pool().begin().await?;
        sqlx::query(r#"DELETE FROM agent_labels WHERE host_id = ?"#)
            .bind(&host_bytes)
            .execute(&mut *tx)
            .await?;
        for (k, v) in labels {
            sqlx::query(r#"INSERT INTO agent_labels (host_id, key, value) VALUES (?, ?, ?)"#)
                .bind(&host_bytes)
                .bind(k)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // ===== placements =====

    /// List all placement rows for a service, ordered by replica_index.
    pub async fn list_placements_by_service(
        &self,
        service_id: ServiceId,
    ) -> Result<Vec<PlacementRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, service_id, host_id, replica_index, state, assigned_at, last_event
            FROM placements
            WHERE service_id = ?
            ORDER BY replica_index, id
            "#,
        )
        .bind(service_id.0)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_placement).collect()
    }

    /// List all placement rows for a host. Used by the scheduler when a host
    /// disconnects to find which services need re-placement.
    pub async fn list_placements_by_host(&self, host_id: HostId) -> Result<Vec<PlacementRow>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, service_id, host_id, replica_index, state, assigned_at, last_event
            FROM placements
            WHERE host_id = ?
            ORDER BY service_id, replica_index
            "#,
        )
        .bind(&host_bytes)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_placement).collect()
    }

    /// List every placement row in the database, ordered for deterministic
    /// scheduler bootstrap.
    pub async fn list_all_placements(&self) -> Result<Vec<PlacementRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, service_id, host_id, replica_index, state, assigned_at, last_event
            FROM placements
            ORDER BY service_id, replica_index, id
            "#,
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_placement).collect()
    }

    /// Upsert a placement keyed on `(service_id, host_id, replica_index)`.
    /// Sets `assigned_at` to "now" on insert; preserves the existing value
    /// on update so re-asserting a still-active placement does not bump the
    /// timestamp.
    pub async fn upsert_placement(&self, p: UpsertPlacement) -> Result<PlacementRow> {
        let host_bytes = p.host_id.to_bytes().to_vec();
        sqlx::query(
            r#"
            INSERT INTO placements (service_id, host_id, replica_index, state, last_event)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(service_id, host_id, replica_index) DO UPDATE SET
                state      = excluded.state,
                last_event = excluded.last_event
            "#,
        )
        .bind(p.service_id.0)
        .bind(&host_bytes)
        .bind(p.replica_index as i64)
        .bind(p.state.as_str())
        .bind(p.last_event.as_deref())
        .execute(self.pool())
        .await?;
        self.get_placement(p.service_id, p.host_id, p.replica_index)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!(
                    "placement for service {} host {} replica {} not found after upsert",
                    p.service_id, p.host_id, p.replica_index
                ),
            })
    }

    pub async fn get_placement(
        &self,
        service_id: ServiceId,
        host_id: HostId,
        replica_index: u32,
    ) -> Result<Option<PlacementRow>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let row = sqlx::query(
            r#"
            SELECT id, service_id, host_id, replica_index, state, assigned_at, last_event
            FROM placements
            WHERE service_id = ? AND host_id = ? AND replica_index = ?
            "#,
        )
        .bind(service_id.0)
        .bind(&host_bytes)
        .bind(replica_index as i64)
        .fetch_optional(self.pool())
        .await?;
        row.map(row_to_placement).transpose()
    }

    pub async fn delete_placement(
        &self,
        service_id: ServiceId,
        host_id: HostId,
        replica_index: u32,
    ) -> Result<bool> {
        let host_bytes = host_id.to_bytes().to_vec();
        let r = sqlx::query(
            r#"DELETE FROM placements WHERE service_id = ? AND host_id = ? AND replica_index = ?"#,
        )
        .bind(service_id.0)
        .bind(&host_bytes)
        .bind(replica_index as i64)
        .execute(self.pool())
        .await?;
        Ok(r.rows_affected() > 0)
    }
}

fn row_to_placement(r: sqlx::sqlite::SqliteRow) -> Result<PlacementRow> {
    let host_bytes: Vec<u8> = r.try_get("host_id")?;
    let host_id = HostId::from_db_bytes(host_bytes)?;
    let state_s: String = r.try_get("state")?;
    let assigned_s: String = r.try_get("assigned_at")?;
    let assigned_at = parse_dt(&assigned_s)?;
    let replica_i: i64 = r.try_get("replica_index")?;
    let service_id_i: i64 = r.try_get("service_id")?;
    Ok(PlacementRow {
        id: r.try_get("id")?,
        service_id: ServiceId(service_id_i),
        host_id,
        replica_index: replica_i as u32,
        state: PlacementState::parse(&state_s)?,
        assigned_at,
        last_event: r.try_get("last_event")?,
    })
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|n| n.and_utc())
        .map_err(|e| Error::Decode {
            reason: format!("bad placements.assigned_at '{s}': {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::EnrollHost;
    use crate::inventory::Inventory;
    use crate::service::{InsertService, ServiceState};
    use crate::stack::{InsertStack, StackSource};

    async fn setup() -> (Inventory, HostId, ServiceId) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                hostname: "alice".into(),
                fingerprint: "fp-alice".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.14.0".into(),
                docker_version: "24.0".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "web".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let service_id = inv
            .insert_service(InsertService {
                host_id,
                stack_id: Some(stack_id),
                name: "nginx".into(),
                image: "nginx:alpine".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        (inv, host_id, service_id)
    }

    #[tokio::test]
    async fn agent_labels_round_trip() {
        let (inv, host_id, _) = setup().await;
        let mut labels = BTreeMap::new();
        labels.insert("role".into(), "worker".into());
        labels.insert("tier".into(), "gpu".into());

        inv.replace_agent_labels(host_id, &labels).await.unwrap();
        let got = inv.list_agent_labels(host_id).await.unwrap();
        assert_eq!(got, labels);
    }

    #[tokio::test]
    async fn agent_labels_replace_deletes_missing_keys() {
        let (inv, host_id, _) = setup().await;
        let mut first = BTreeMap::new();
        first.insert("role".into(), "worker".into());
        first.insert("tier".into(), "gpu".into());
        inv.replace_agent_labels(host_id, &first).await.unwrap();

        let mut second = BTreeMap::new();
        second.insert("role".into(), "control".into());
        inv.replace_agent_labels(host_id, &second).await.unwrap();

        let got = inv.list_agent_labels(host_id).await.unwrap();
        assert_eq!(got, second);
        assert!(!got.contains_key("tier"));
    }

    #[tokio::test]
    async fn agent_labels_empty_when_none() {
        let (inv, host_id, _) = setup().await;
        let got = inv.list_agent_labels(host_id).await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn placement_backfill_on_migration() {
        // Insert a service via the normal API; the migration runs at open
        // time, so the backfill happens in setup(). Verify the placements
        // row exists for the freshly-inserted service. Note: backfill runs
        // at migration time only; services inserted after open() do NOT get
        // an automatic placements row (they go through the scheduler).
        let inv = Inventory::open_in_memory().await.unwrap();
        // No rows yet -> no placements.
        let all = inv.list_all_placements().await.unwrap();
        assert!(all.is_empty());

        // Insert a host and service. These do NOT get a backfilled row
        // because the migration already ran.
        let host_id = inv
            .enroll_host(EnrollHost {
                hostname: "x".into(),
                fingerprint: "fp-x".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.14.0".into(),
                docker_version: "24.0".into(),
            })
            .await
            .unwrap();
        inv.insert_service(InsertService {
            host_id,
            stack_id: None,
            name: "y".into(),
            image: "x:latest".into(),
            state: ServiceState::Running,
        })
        .await
        .unwrap();
        let all = inv.list_all_placements().await.unwrap();
        assert!(all.is_empty(), "post-migration inserts don't backfill");
    }

    #[tokio::test]
    async fn placement_upsert_inserts_and_updates() {
        let (inv, host_id, service_id) = setup().await;
        let row = inv
            .upsert_placement(UpsertPlacement {
                service_id,
                host_id,
                replica_index: 0,
                state: PlacementState::Pending,
                last_event: None,
            })
            .await
            .unwrap();
        assert_eq!(row.state, PlacementState::Pending);

        // Update.
        let row = inv
            .upsert_placement(UpsertPlacement {
                service_id,
                host_id,
                replica_index: 0,
                state: PlacementState::Active,
                last_event: Some("{\"kind\":\"placement.created\"}".into()),
            })
            .await
            .unwrap();
        assert_eq!(row.state, PlacementState::Active);
        assert!(row.last_event.as_deref().unwrap().contains("placement"));
    }

    #[tokio::test]
    async fn placement_list_and_delete() {
        let (inv, host_id, service_id) = setup().await;
        inv.upsert_placement(UpsertPlacement {
            service_id,
            host_id,
            replica_index: 0,
            state: PlacementState::Active,
            last_event: None,
        })
        .await
        .unwrap();
        inv.upsert_placement(UpsertPlacement {
            service_id,
            host_id,
            replica_index: 1,
            state: PlacementState::Pending,
            last_event: None,
        })
        .await
        .unwrap();

        let by_service = inv.list_placements_by_service(service_id).await.unwrap();
        assert_eq!(by_service.len(), 2);
        assert_eq!(by_service[0].replica_index, 0);
        assert_eq!(by_service[1].replica_index, 1);

        let by_host = inv.list_placements_by_host(host_id).await.unwrap();
        assert_eq!(by_host.len(), 2);

        let deleted = inv.delete_placement(service_id, host_id, 1).await.unwrap();
        assert!(deleted);

        let by_service = inv.list_placements_by_service(service_id).await.unwrap();
        assert_eq!(by_service.len(), 1);
        assert_eq!(by_service[0].replica_index, 0);
    }
}
