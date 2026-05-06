//! Webhook DAO: CRUD over `webhooks` and `webhook_deliveries`.
//!
//! See spec §"Storage" + §"DAO" of
//! `docs/superpowers/specs/2026-05-06-phase-12a-outbound-webhooks-design.md`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Error, Result};

/// Wildcard token in `event_kinds` that matches every event.
pub const KIND_WILDCARD: &str = "*";

/// Where a `webhook_deliveries` row originated. Phase 12b/c (#54 #55).
///
/// `Webhook` rows reference a `webhooks(id)` row (the 12a shape). `Lifecycle`
/// and `Gate` rows carry their URL+secret inline because there is no parent
/// row: lifecycle hooks are configured per-container via Docker labels, and
/// gates are configured per-policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverySource {
    /// 12a outbound webhook with a parent `webhooks` row.
    Webhook,
    /// 12b container lifecycle hook (`isengard.hooks.*` labels).
    Lifecycle,
    /// 12c external-action gate evaluation (sync POST + decision parse).
    Gate,
}

impl DeliverySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Lifecycle => "lifecycle",
            Self::Gate => "gate",
        }
    }
}

impl FromStr for DeliverySource {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "webhook" => Self::Webhook,
            "lifecycle" => Self::Lifecycle,
            "gate" => Self::Gate,
            other => {
                return Err(Error::Decode {
                    reason: format!("unknown delivery source: {other}"),
                });
            }
        })
    }
}

/// State machine for `webhook_deliveries.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Awaiting first attempt or scheduled retry.
    Pending,
    /// Successfully delivered (2xx response).
    Success,
    /// Permanent failure (4xx response). No retry.
    Failed,
    /// Retried up to the cap and still failed.
    Exhausted,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Exhausted => "exhausted",
        }
    }
}

impl FromStr for DeliveryStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "pending" => Self::Pending,
            "success" => Self::Success,
            "failed" => Self::Failed,
            "exhausted" => Self::Exhausted,
            other => {
                return Err(Error::Decode {
                    reason: format!("unknown delivery status: {other}"),
                });
            }
        })
    }
}

/// A row from the `webhooks` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub secret: String,
    /// Comma-separated list of event kinds. `"*"` matches all.
    pub event_kinds: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert payload for a webhook.
#[derive(Debug, Clone)]
pub struct InsertWebhook {
    pub url: String,
    pub secret: String,
    pub event_kinds: String,
    pub enabled: bool,
}

/// Update payload. Any `Some` field overwrites; `None` keeps the existing value.
#[derive(Debug, Clone, Default)]
pub struct UpdateWebhook {
    pub url: Option<String>,
    pub secret: Option<String>,
    pub event_kinds: Option<String>,
    pub enabled: Option<bool>,
}

/// A row from the `webhook_deliveries` table.
///
/// Post-Phase-12b/c the table holds three kinds of deliveries (see
/// [`DeliverySource`]). For `Webhook` rows the URL+secret are looked up via
/// `webhook_id`; for `Lifecycle` and `Gate` rows they are stored on the row
/// itself in `url` + `secret`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: i64,
    /// `Some` for `source=webhook` rows, `None` for lifecycle / gate rows.
    pub webhook_id: Option<i64>,
    pub source: DeliverySource,
    /// Inline destination URL. Used by lifecycle / gate rows; `None` for
    /// `webhook` rows (worker resolves via `webhook_id`).
    pub url: Option<String>,
    /// Inline HMAC secret. Used by lifecycle / gate rows; `None` for
    /// `webhook` rows.
    pub secret: Option<String>,
    pub event_kind: String,
    pub payload_json: String,
    pub status: DeliveryStatus,
    pub attempts: i64,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Insert payload for a `source=webhook` delivery row (Phase 12a path).
#[derive(Debug, Clone)]
pub struct InsertDelivery {
    pub webhook_id: i64,
    pub event_kind: String,
    pub payload_json: String,
}

/// Insert payload for a `source=lifecycle` delivery row (Phase 12b).
#[derive(Debug, Clone)]
pub struct InsertLifecycleDelivery {
    pub url: String,
    pub secret: Option<String>,
    pub event_kind: String,
    pub payload_json: String,
}

/// Insert payload for a `source=gate` delivery row (Phase 12c).
#[derive(Debug, Clone)]
pub struct InsertGateDelivery {
    pub url: String,
    pub secret: Option<String>,
    pub event_kind: String,
    pub payload_json: String,
}

impl crate::inventory::Inventory {
    pub async fn insert_webhook(&self, ins: InsertWebhook) -> Result<Webhook> {
        let res = sqlx::query(
            r#"
            INSERT INTO webhooks (url, secret, event_kinds, enabled)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(&ins.url)
        .bind(&ins.secret)
        .bind(&ins.event_kinds)
        .bind(if ins.enabled { 1i64 } else { 0i64 })
        .execute(self.pool())
        .await?;

        let id = res.last_insert_rowid();
        self.get_webhook(id).await?.ok_or_else(|| Error::Decode {
            reason: format!("webhook id={id} not found after insert"),
        })
    }

    pub async fn get_webhook(&self, id: i64) -> Result<Option<Webhook>> {
        let row = sqlx::query(
            r#"
            SELECT id, url, secret, event_kinds, enabled, created_at, updated_at
            FROM webhooks WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(row_to_webhook).transpose()
    }

    pub async fn list_webhooks(&self) -> Result<Vec<Webhook>> {
        let rows = sqlx::query(
            r#"
            SELECT id, url, secret, event_kinds, enabled, created_at, updated_at
            FROM webhooks ORDER BY id
            "#,
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_webhook).collect()
    }

    pub async fn list_enabled_webhooks(&self) -> Result<Vec<Webhook>> {
        let rows = sqlx::query(
            r#"
            SELECT id, url, secret, event_kinds, enabled, created_at, updated_at
            FROM webhooks WHERE enabled = 1 ORDER BY id
            "#,
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_webhook).collect()
    }

    pub async fn update_webhook(&self, id: i64, body: UpdateWebhook) -> Result<Option<Webhook>> {
        // Coalesce: SQLite's COALESCE(?, col) keeps the existing value when
        // the bound param is NULL, so we don't need a custom dynamic query.
        let enabled_param: Option<i64> = body.enabled.map(|b| if b { 1 } else { 0 });
        let res = sqlx::query(
            r#"
            UPDATE webhooks SET
                url         = COALESCE(?, url),
                secret      = COALESCE(?, secret),
                event_kinds = COALESCE(?, event_kinds),
                enabled     = COALESCE(?, enabled),
                updated_at  = strftime('%Y-%m-%dT%H:%M:%fZ','now')
            WHERE id = ?
            "#,
        )
        .bind(body.url.as_deref())
        .bind(body.secret.as_deref())
        .bind(body.event_kinds.as_deref())
        .bind(enabled_param)
        .bind(id)
        .execute(self.pool())
        .await?;

        if res.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_webhook(id).await
    }

    /// Returns true iff a row was actually deleted.
    pub async fn delete_webhook(&self, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM webhooks WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn insert_delivery(&self, ins: InsertDelivery) -> Result<WebhookDelivery> {
        let res = sqlx::query(
            r#"
            INSERT INTO webhook_deliveries
              (webhook_id, source, event_kind, payload_json, status)
            VALUES (?, 'webhook', ?, ?, 'pending')
            "#,
        )
        .bind(ins.webhook_id)
        .bind(&ins.event_kind)
        .bind(&ins.payload_json)
        .execute(self.pool())
        .await?;

        let id = res.last_insert_rowid();
        self.get_delivery(id).await?.ok_or_else(|| Error::Decode {
            reason: format!("delivery id={id} not found after insert"),
        })
    }

    /// Phase 12b: insert a delivery row for a container lifecycle hook.
    /// `webhook_id` is left NULL; `url` + `secret` carry the destination
    /// directly so the worker can dispatch without a parent row.
    pub async fn insert_lifecycle_delivery(
        &self,
        ins: InsertLifecycleDelivery,
    ) -> Result<WebhookDelivery> {
        let res = sqlx::query(
            r#"
            INSERT INTO webhook_deliveries
              (webhook_id, source, url, secret, event_kind, payload_json, status)
            VALUES (NULL, 'lifecycle', ?, ?, ?, ?, 'pending')
            "#,
        )
        .bind(&ins.url)
        .bind(ins.secret.as_deref())
        .bind(&ins.event_kind)
        .bind(&ins.payload_json)
        .execute(self.pool())
        .await?;

        let id = res.last_insert_rowid();
        self.get_delivery(id).await?.ok_or_else(|| Error::Decode {
            reason: format!("lifecycle delivery id={id} not found after insert"),
        })
    }

    /// Phase 12c: insert a delivery row representing one external-gate
    /// evaluation. The row is the audit trail; the synchronous evaluator
    /// stamps the outcome via `mark_delivery_*` paths just like webhook rows.
    pub async fn insert_gate_delivery(
        &self,
        ins: InsertGateDelivery,
    ) -> Result<WebhookDelivery> {
        let res = sqlx::query(
            r#"
            INSERT INTO webhook_deliveries
              (webhook_id, source, url, secret, event_kind, payload_json, status)
            VALUES (NULL, 'gate', ?, ?, ?, ?, 'pending')
            "#,
        )
        .bind(&ins.url)
        .bind(ins.secret.as_deref())
        .bind(&ins.event_kind)
        .bind(&ins.payload_json)
        .execute(self.pool())
        .await?;

        let id = res.last_insert_rowid();
        self.get_delivery(id).await?.ok_or_else(|| Error::Decode {
            reason: format!("gate delivery id={id} not found after insert"),
        })
    }

    pub async fn get_delivery(&self, id: i64) -> Result<Option<WebhookDelivery>> {
        let row = sqlx::query(
            r#"
            SELECT id, webhook_id, source, url, secret, event_kind, payload_json,
                   status, attempts, last_attempt_at, last_error,
                   next_retry_at, created_at
            FROM webhook_deliveries WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(row_to_delivery).transpose()
    }

    pub async fn list_deliveries(
        &self,
        webhook_id: i64,
        status_filter: Option<DeliveryStatus>,
        limit: i64,
    ) -> Result<Vec<WebhookDelivery>> {
        let rows = match status_filter {
            Some(s) => {
                sqlx::query(
                    r#"
                    SELECT id, webhook_id, source, url, secret, event_kind, payload_json,
                           status, attempts, last_attempt_at, last_error,
                           next_retry_at, created_at
                    FROM webhook_deliveries
                    WHERE webhook_id = ? AND status = ?
                    ORDER BY id DESC LIMIT ?
                    "#,
                )
                .bind(webhook_id)
                .bind(s.as_str())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
            None => {
                sqlx::query(
                    r#"
                    SELECT id, webhook_id, source, url, secret, event_kind, payload_json,
                           status, attempts, last_attempt_at, last_error,
                           next_retry_at, created_at
                    FROM webhook_deliveries
                    WHERE webhook_id = ?
                    ORDER BY id DESC LIMIT ?
                    "#,
                )
                .bind(webhook_id)
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
        };
        rows.into_iter().map(row_to_delivery).collect()
    }

    /// Phase 12b/c: list deliveries filtered by source. Used by the
    /// dashboard "Lifecycle hooks" / "Gates" tabs to show traffic that has
    /// no parent webhook row.
    pub async fn list_deliveries_by_source(
        &self,
        source: DeliverySource,
        limit: i64,
    ) -> Result<Vec<WebhookDelivery>> {
        let rows = sqlx::query(
            r#"
            SELECT id, webhook_id, source, url, secret, event_kind, payload_json,
                   status, attempts, last_attempt_at, last_error,
                   next_retry_at, created_at
            FROM webhook_deliveries
            WHERE source = ?
            ORDER BY id DESC LIMIT ?
            "#,
        )
        .bind(source.as_str())
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_delivery).collect()
    }

    /// Pull pending deliveries due now (or with no scheduled time yet).
    /// Caller is responsible for dispatching and writing back state.
    pub async fn claim_pending_deliveries(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<WebhookDelivery>> {
        let now_s = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let rows = sqlx::query(
            r#"
            SELECT id, webhook_id, source, url, secret, event_kind, payload_json,
                   status, attempts, last_attempt_at, last_error,
                   next_retry_at, created_at
            FROM webhook_deliveries
            WHERE status = 'pending'
              AND (next_retry_at IS NULL OR next_retry_at <= ?)
            ORDER BY id LIMIT ?
            "#,
        )
        .bind(&now_s)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_delivery).collect()
    }

    pub async fn mark_delivery_success(
        &self,
        id: i64,
        now: DateTime<Utc>,
        attempts: i64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE webhook_deliveries SET
                status          = 'success',
                attempts        = ?,
                last_attempt_at = ?,
                last_error      = NULL,
                next_retry_at   = NULL
            WHERE id = ?
            "#,
        )
        .bind(attempts)
        .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_delivery_pending(
        &self,
        id: i64,
        now: DateTime<Utc>,
        attempts: i64,
        next_retry_at: DateTime<Utc>,
        err: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE webhook_deliveries SET
                status          = 'pending',
                attempts        = ?,
                last_attempt_at = ?,
                last_error      = ?,
                next_retry_at   = ?
            WHERE id = ?
            "#,
        )
        .bind(attempts)
        .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(err)
        .bind(next_retry_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_delivery_failed(
        &self,
        id: i64,
        now: DateTime<Utc>,
        attempts: i64,
        err: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE webhook_deliveries SET
                status          = 'failed',
                attempts        = ?,
                last_attempt_at = ?,
                last_error      = ?,
                next_retry_at   = NULL
            WHERE id = ?
            "#,
        )
        .bind(attempts)
        .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(err)
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_delivery_exhausted(
        &self,
        id: i64,
        now: DateTime<Utc>,
        attempts: i64,
        err: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE webhook_deliveries SET
                status          = 'exhausted',
                attempts        = ?,
                last_attempt_at = ?,
                last_error      = ?,
                next_retry_at   = NULL
            WHERE id = ?
            "#,
        )
        .bind(attempts)
        .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(err)
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
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

fn parse_dt_opt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    s.map(parse_dt).transpose()
}

fn row_to_webhook(r: sqlx::sqlite::SqliteRow) -> Result<Webhook> {
    use sqlx::Row;
    let enabled_int: i64 = r.try_get("enabled")?;
    Ok(Webhook {
        id: r.try_get("id")?,
        url: r.try_get("url")?,
        secret: r.try_get("secret")?,
        event_kinds: r.try_get("event_kinds")?,
        enabled: enabled_int != 0,
        created_at: parse_dt(r.try_get("created_at")?)?,
        updated_at: parse_dt(r.try_get("updated_at")?)?,
    })
}

fn row_to_delivery(r: sqlx::sqlite::SqliteRow) -> Result<WebhookDelivery> {
    use sqlx::Row;
    let status_s: String = r.try_get("status")?;
    let status: DeliveryStatus = status_s.parse()?;
    let source_s: String = r.try_get("source")?;
    let source: DeliverySource = source_s.parse()?;
    Ok(WebhookDelivery {
        id: r.try_get("id")?,
        webhook_id: r.try_get("webhook_id")?,
        source,
        url: r.try_get("url")?,
        secret: r.try_get("secret")?,
        event_kind: r.try_get("event_kind")?,
        payload_json: r.try_get("payload_json")?,
        status,
        attempts: r.try_get("attempts")?,
        last_attempt_at: parse_dt_opt(r.try_get("last_attempt_at")?)?,
        last_error: r.try_get("last_error")?,
        next_retry_at: parse_dt_opt(r.try_get("next_retry_at")?)?,
        created_at: parse_dt(r.try_get("created_at")?)?,
    })
}

/// Match a webhook's `event_kinds` filter against an event kind. The filter
/// is a comma-separated list; whitespace is trimmed; `*` matches all.
pub fn kind_matches(filter: &str, kind: &str) -> bool {
    for tok in filter.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if t == KIND_WILDCARD || t == kind {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_status_round_trips_through_str() {
        for s in [
            DeliveryStatus::Pending,
            DeliveryStatus::Success,
            DeliveryStatus::Failed,
            DeliveryStatus::Exhausted,
        ] {
            let parsed: DeliveryStatus = s.as_str().parse().expect("roundtrip");
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn kind_matches_wildcard() {
        assert!(kind_matches("*", "anything.here"));
    }

    #[test]
    fn kind_matches_exact_token() {
        assert!(kind_matches(
            "update.success,update.failed",
            "update.failed"
        ));
        assert!(!kind_matches("update.success", "update.failed"));
    }

    #[test]
    fn kind_matches_trims_whitespace() {
        assert!(kind_matches(
            " update.success , update.failed ",
            "update.success"
        ));
    }

    #[test]
    fn kind_matches_empty_filter_is_no_match() {
        assert!(!kind_matches("", "anything"));
        assert!(!kind_matches(",,,", "anything"));
    }
}
