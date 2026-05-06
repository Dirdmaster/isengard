//! Policy DAO: CRUD over the `policies` table.
//!
//! See spec §"DAO (in `isengard-storage::policy`)" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! `Policy` and `PolicyScopeType` live in `isengard-core::policy`; this
//! module owns the row shape and the DAO methods on `Inventory`. The
//! scope-type enum is re-exported below so existing call sites
//! (`isengard_storage::policy::PolicyScopeType`) keep compiling.

use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use isengard_core::policy::Policy;
// Re-export so external crates can keep their existing import path.
pub use isengard_core::policy::PolicyScopeType;
use serde::{Deserialize, Serialize};

/// A row from the `policies` table. `body` is the parsed `Policy`; the raw
/// JSON column is dropped on read (the resolver only ever needs the typed
/// view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRow {
    pub id: i64,
    pub scope_type: PolicyScopeType,
    pub scope_key: String,
    pub body: Policy,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert payload. Distinct from `PolicyRow` so the caller doesn't have to
/// invent an id or pretend to know the timestamps.
#[derive(Debug, Clone)]
pub struct InsertPolicy {
    pub scope_type: PolicyScopeType,
    pub scope_key: String,
    pub body: Policy,
}

impl crate::inventory::Inventory {
    pub async fn insert_policy(&self, ins: InsertPolicy) -> Result<PolicyRow> {
        let body_json = serde_json::to_string(&ins.body).map_err(|e| Error::Decode {
            reason: format!("serializing Policy body: {e}"),
        })?;
        sqlx::query(
            r#"
            INSERT INTO policies (scope_type, scope_key, body_json)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(ins.scope_type.as_str())
        .bind(&ins.scope_key)
        .bind(&body_json)
        .execute(self.pool())
        .await?;

        self.get_policy(ins.scope_type, &ins.scope_key)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!(
                    "policy ({}, {}) not found after insert",
                    ins.scope_type.as_str(),
                    ins.scope_key
                ),
            })
    }

    pub async fn get_policy(
        &self,
        scope_type: PolicyScopeType,
        scope_key: &str,
    ) -> Result<Option<PolicyRow>> {
        let row = sqlx::query(
            r#"
            SELECT id, scope_type, scope_key, body_json, created_at, updated_at
            FROM policies
            WHERE scope_type = ? AND scope_key = ?
            "#,
        )
        .bind(scope_type.as_str())
        .bind(scope_key)
        .fetch_optional(self.pool())
        .await?;

        row.map(row_to_policy).transpose()
    }

    /// List every policy row, ordered by scope rank (Global, Fleet, Stack,
    /// Service, Container) and then by id within a rank for stable ordering.
    pub async fn list_policies(&self) -> Result<Vec<PolicyRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, scope_type, scope_key, body_json, created_at, updated_at
            FROM policies
            ORDER BY
                CASE scope_type
                    WHEN 'global' THEN 0
                    WHEN 'fleet' THEN 1
                    WHEN 'stack' THEN 2
                    WHEN 'service' THEN 3
                    WHEN 'container' THEN 4
                    ELSE 99
                END,
                id
            "#,
        )
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(row_to_policy).collect()
    }

    /// Insert if absent, replace body otherwise. `created_at` is preserved on
    /// update; `updated_at` is bumped to "now".
    pub async fn upsert_policy(
        &self,
        scope_type: PolicyScopeType,
        scope_key: &str,
        body: &Policy,
    ) -> Result<PolicyRow> {
        let body_json = serde_json::to_string(body).map_err(|e| Error::Decode {
            reason: format!("serializing Policy body: {e}"),
        })?;
        sqlx::query(
            r#"
            INSERT INTO policies (scope_type, scope_key, body_json)
            VALUES (?, ?, ?)
            ON CONFLICT(scope_type, scope_key) DO UPDATE SET
                body_json  = excluded.body_json,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(scope_type.as_str())
        .bind(scope_key)
        .bind(&body_json)
        .execute(self.pool())
        .await?;

        self.get_policy(scope_type, scope_key)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!(
                    "policy ({}, {scope_key}) not found after upsert",
                    scope_type.as_str()
                ),
            })
    }

    /// Returns true iff a row was actually deleted.
    pub async fn delete_policy(
        &self,
        scope_type: PolicyScopeType,
        scope_key: &str,
    ) -> Result<bool> {
        let r = sqlx::query("DELETE FROM policies WHERE scope_type = ? AND scope_key = ?")
            .bind(scope_type.as_str())
            .bind(scope_key)
            .execute(self.pool())
            .await?;
        Ok(r.rows_affected() > 0)
    }
}

fn row_to_policy(r: sqlx::sqlite::SqliteRow) -> Result<PolicyRow> {
    use sqlx::Row;
    let scope_type_s: String = r.try_get("scope_type")?;
    let scope_type: PolicyScopeType =
        scope_type_s
            .parse()
            .map_err(
                |e: isengard_core::policy::UnknownPolicyScopeType| Error::Decode {
                    reason: e.to_string(),
                },
            )?;
    let body_json: String = r.try_get("body_json")?;
    let body: Policy = serde_json::from_str(&body_json).map_err(|e| Error::Decode {
        reason: format!("deserializing Policy body: {e}"),
    })?;

    let parse_dt = |s: String| -> Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&s)
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                    .map(|n| n.and_utc().fixed_offset())
            })
            .map_err(|e| Error::Decode {
                reason: format!("bad timestamp '{s}': {e}"),
            })
            .map(|dt| dt.with_timezone(&Utc))
    };

    Ok(PolicyRow {
        id: r.try_get("id")?,
        scope_type,
        scope_key: r.try_get("scope_key")?,
        body,
        created_at: parse_dt(r.try_get("created_at")?)?,
        updated_at: parse_dt(r.try_get("updated_at")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_type_round_trips_through_str() {
        for s in [
            PolicyScopeType::Global,
            PolicyScopeType::Fleet,
            PolicyScopeType::Stack,
            PolicyScopeType::Service,
            PolicyScopeType::Container,
        ] {
            let parsed: PolicyScopeType = s.as_str().parse().expect("parse roundtrip");
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn scope_type_rank_orders_by_specificity() {
        assert!(PolicyScopeType::Global.rank() < PolicyScopeType::Fleet.rank());
        assert!(PolicyScopeType::Fleet.rank() < PolicyScopeType::Stack.rank());
        assert!(PolicyScopeType::Stack.rank() < PolicyScopeType::Service.rank());
        assert!(PolicyScopeType::Service.rank() < PolicyScopeType::Container.rank());
    }

    #[test]
    fn unknown_scope_type_string_errors() {
        let err = "frobozz".parse::<PolicyScopeType>().unwrap_err();
        assert!(format!("{err:#}").contains("unknown policy scope_type"));
    }

    #[tokio::test]
    async fn invalid_scope_type_rejected_by_check_constraint() {
        let inv = crate::inventory::Inventory::open_in_memory()
            .await
            .expect("open");
        let body_json = serde_json::to_string(&isengard_core::policy::Policy::default()).unwrap();
        let res =
            sqlx::query("INSERT INTO policies (scope_type, scope_key, body_json) VALUES (?, ?, ?)")
                .bind("invalid")
                .bind("anything")
                .bind(&body_json)
                .execute(inv.pool())
                .await;
        let err = res.expect_err("CHECK constraint should reject 'invalid' scope_type");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("check") || msg.contains("constraint"),
            "expected a CHECK constraint failure, got: {err:?}"
        );
    }
}
