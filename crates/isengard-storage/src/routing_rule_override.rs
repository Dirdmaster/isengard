//! Per-field UI overrides for label-source routing rules.
//!
//! When a routing rule originates from a container label
//! ([`crate::RoutingRuleSource::Label`]), the dashboard lets an
//! operator nudge individual fields without losing the binding to
//! the label. Each tweak lands as a row in `routing_rule_overrides`
//! keyed by `(routing_rule_id, field)` with the JSON value the
//! operator picked.

use crate::error::Result;
use crate::routing_rule::RoutingRuleId;
use serde::{Deserialize, Serialize};

/// One row from `routing_rule_overrides`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRuleOverride {
    /// Parent routing rule.
    pub routing_rule_id: RoutingRuleId,
    /// Field name the override applies to (e.g. `auth`, `protocol`).
    pub field: String,
    /// Override value. Schema is field-dependent.
    pub value_json: serde_json::Value,
}

impl crate::inventory::Inventory {
    /// Upsert one override for `(rule_id, field)`. Bumps `created_at`
    /// on conflict; the column doubles as a "last-changed" timestamp.
    pub async fn upsert_routing_rule_override(
        &self,
        rule_id: RoutingRuleId,
        field: &str,
        value: serde_json::Value,
    ) -> Result<()> {
        let v = value.to_string();
        sqlx::query(
            r#"
            INSERT INTO routing_rule_overrides (routing_rule_id, field, value_json)
            VALUES (?, ?, ?)
            ON CONFLICT (routing_rule_id, field) DO UPDATE SET
              value_json = excluded.value_json,
              created_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(rule_id.0)
        .bind(field)
        .bind(v)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Every override row for a given rule.
    pub async fn list_routing_rule_overrides(
        &self,
        rule_id: RoutingRuleId,
    ) -> Result<Vec<RoutingRuleOverride>> {
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT routing_rule_id, field, value_json \
             FROM routing_rule_overrides \
             WHERE routing_rule_id = ?",
        )
        .bind(rule_id.0)
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let rule_id_i64: i64 = r.try_get("routing_rule_id")?;
            let field: String = r.try_get("field")?;
            let value_str: String = r.try_get("value_json")?;
            out.push(RoutingRuleOverride {
                routing_rule_id: RoutingRuleId(rule_id_i64),
                field,
                value_json: serde_json::from_str(&value_str).map_err(|e| crate::Error::Decode {
                    reason: e.to_string(),
                })?,
            });
        }
        Ok(out)
    }
}
