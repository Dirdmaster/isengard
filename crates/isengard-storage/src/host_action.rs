#![doc = include_str!("../docs/host-actions.md")]

use crate::error::{Error, Result};
use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Surrogate key for `host_actions` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostActionId(pub i64);

impl std::fmt::Display for HostActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Typed payload for an agent-pull action.
///
/// Stored in the `payload_json` column verbatim; the `kind` column
/// holds the discriminant for cheap filtering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostActionKind {
    /// Force the agent to re-check images. `stack_name = None` means
    /// every stack on the host.
    ForceUpdate {
        /// Restrict to a single stack; `None` means every stack.
        stack_name: Option<String>,
    },
    /// Decommission the host: revoke certs, drop the row, close the
    /// connection.
    Decommission,
}

impl HostActionKind {
    /// Wire-format `kind` column value.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::ForceUpdate { .. } => "force_update",
            Self::Decommission => "decommission",
        }
    }

    /// Serialize the kind to its `payload_json` representation. Falls
    /// back to `{}` only if the serde-derived encoding fails, which
    /// shouldn't happen for these closed variants.
    pub fn payload_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

/// One agent-pull row read back from `host_actions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAction {
    /// Surrogate key.
    pub id: HostActionId,
    /// Host the action targets.
    pub host_id: HostId,
    /// Typed payload.
    pub kind: HostActionKind,
    /// When the row was queued.
    pub created_at: DateTime<Utc>,
    /// When the agent picked it up.
    pub delivered_at: Option<DateTime<Utc>>,
    /// Result string written by the agent at delivery time.
    pub result: Option<String>,
}

// ---------------------------------------------------------------------------
// Pending approvals
// ---------------------------------------------------------------------------

/// Wire-format `kind` column value for pending-approval rows.
pub const APPROVAL_KIND: &str = "update_pending_approval";

/// Lifecycle state of a `update_pending_approval` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// Waiting on an operator decision.
    PendingOpen,
    /// Operator approved; controller can dispatch the apply.
    PendingApproved,
    /// Operator rejected.
    PendingRejected,
    /// Expired without a decision (auto-transition).
    PendingExpired,
    /// Snoozed for a fixed window.
    PendingSnoozed,
}

impl ApprovalState {
    /// Canonical snake_case spelling for the SQLite column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PendingOpen => "pending_open",
            Self::PendingApproved => "pending_approved",
            Self::PendingRejected => "pending_rejected",
            Self::PendingExpired => "pending_expired",
            Self::PendingSnoozed => "pending_snoozed",
        }
    }

    /// True for every terminal state (anything other than `PendingOpen`).
    pub fn is_decided(self) -> bool {
        !matches!(self, Self::PendingOpen)
    }
}

impl FromStr for ApprovalState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "pending_open" => Self::PendingOpen,
            "pending_approved" => Self::PendingApproved,
            "pending_rejected" => Self::PendingRejected,
            "pending_expired" => Self::PendingExpired,
            "pending_snoozed" => Self::PendingSnoozed,
            other => {
                return Err(Error::Decode {
                    reason: format!("unknown approval state: {other}"),
                });
            }
        })
    }
}

/// Filter coarseness used by `Inventory::list_pending_approvals`.
///
/// `Open` matches `pending_open`. `Decided` matches the four terminal
/// states (`approved`, `rejected`, `expired`, `snoozed`). `All` is
/// unbounded.
///
/// [`Inventory`]: crate::Inventory
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStateFilter {
    /// Only rows in `pending_open`. Default.
    #[default]
    Open,
    /// Any terminal state.
    Decided,
    /// Any state.
    All,
}

/// Typed body for `kind = "update_pending_approval"` rows.
///
/// Stored verbatim in the existing `payload_json` column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpdateApprovalBody {
    /// Host the update targets.
    pub host_id: HostId,
    /// Stack name.
    pub stack: String,
    /// Service name.
    pub service: String,
    /// Container name on the host.
    pub container_name: String,
    /// Image reference (without digest).
    pub image: String,
    /// Currently-deployed digest.
    pub current_digest: String,
    /// Proposed digest.
    pub proposed_digest: String,
    /// Optional URL to a diff or release notes.
    pub diff_url: Option<String>,
    /// Notifier channel that surfaced the approval.
    pub approver_channel: Option<String>,
}

/// What an operator (or the auto-expire task) decides to do with an
/// open approval row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Apply the proposed digest.
    Approve,
    /// Keep the current digest.
    Reject,
    /// Postpone the decision; the wrapped `u32` is the snooze window
    /// in hours.
    SnoozeHours(u32),
}

impl ApprovalDecision {
    /// State this decision drives the row into.
    pub fn target_state(&self) -> ApprovalState {
        match self {
            Self::Approve => ApprovalState::PendingApproved,
            Self::Reject => ApprovalState::PendingRejected,
            Self::SnoozeHours(_) => ApprovalState::PendingSnoozed,
        }
    }
}

/// Filter shape passed to `Inventory::list_pending_approvals`.
///
/// [`Inventory`]: crate::Inventory
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalFilter {
    /// Restrict by state coarseness.
    pub state: Option<ApprovalStateFilter>,
    /// Restrict to one host.
    pub host_id: Option<HostId>,
    /// Restrict to one stack name.
    pub stack: Option<String>,
    /// Restrict to one service name.
    pub service: Option<String>,
    /// Inclusive lower bound on `created_at`.
    pub since: Option<DateTime<Utc>>,
    /// Optional SQL `LIMIT`.
    pub limit: Option<u32>,
}

/// Typed view of one `host_actions` row whose kind is
/// `update_pending_approval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApprovalRow {
    /// External ULID identifier (TEXT). The integer `host_actions.id`
    /// is kept internal to the DAO; callers use this string id
    /// everywhere.
    pub action_id: String,
    /// Current lifecycle state.
    pub state: ApprovalState,
    /// Decoded body.
    pub body: UpdateApprovalBody,
    /// When the row stops being decidable.
    pub expires_at: DateTime<Utc>,
    /// When the row reached a terminal state.
    pub decided_at: Option<DateTime<Utc>>,
    /// Operator (or `system:auto-expire`) that decided.
    pub decided_by: Option<String>,
    /// Auxiliary JSON (notifier message ids, etc).
    pub metadata_json: Option<serde_json::Value>,
    /// When the row was inserted.
    pub created_at: DateTime<Utc>,
    /// When the row was last touched.
    pub updated_at: DateTime<Utc>,
}

/// Insert payload. The DAO mints the ULID; the caller supplies semantics.
#[derive(Debug, Clone)]
pub struct InsertPendingApproval {
    /// Typed body.
    pub body: UpdateApprovalBody,
    /// Hard expiry.
    pub expires_at: DateTime<Utc>,
    /// Notifier channel that surfaced the approval.
    pub approver_channel: Option<String>,
}

/// Returned by `Inventory::decide_pending_approval`.
///
/// Bundles the freshly-transitioned row with a flag the dashboard
/// layer uses to decide whether to dispatch a downstream
/// `apply_update` action.
///
/// [`Inventory`]: crate::Inventory
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecidedApproval {
    /// The row as it stands after the transition.
    pub row: PendingApprovalRow,
    /// `true` iff the decision was `Approve`. Caller is responsible
    /// for queuing the actual `apply_update` HostAction.
    pub should_dispatch_apply_update: bool,
}

// ---------------------------------------------------------------------------
// DAO methods
// ---------------------------------------------------------------------------

impl crate::inventory::Inventory {
    /// Insert a brand-new pending-approval row in state `pending_open`.
    ///
    /// The ULID is minted internally and returned in the row.
    pub async fn insert_pending_approval(
        &self,
        ins: InsertPendingApproval,
    ) -> Result<PendingApprovalRow> {
        // The approver_channel parameter is supplied separately so callers can
        // pass it through the resolver. We mirror it onto the body if the body
        // didn't already have one populated.
        let mut body = ins.body;
        if body.approver_channel.is_none() {
            body.approver_channel = ins.approver_channel.clone();
        }

        let body_json = serde_json::to_string(&body).map_err(|e| Error::Decode {
            reason: format!("serializing UpdateApprovalBody: {e}"),
        })?;
        let action_id = ulid::Ulid::new().to_string();
        let expires_rfc = ins.expires_at.to_rfc3339();
        let now_rfc = Utc::now().to_rfc3339();

        // delivered_at = CURRENT_TIMESTAMP keeps these rows out of the agent's
        // pending_actions stream (which filters `delivered_at IS NULL`).
        sqlx::query(
            r#"
            INSERT INTO host_actions (
                host_id, kind, payload_json,
                action_id, state, expires_at, metadata_json,
                created_at, updated_at, delivered_at
            ) VALUES (
                ?, ?, ?,
                ?, ?, ?, NULL,
                ?, ?, ?
            )
            "#,
        )
        .bind(body.host_id.to_bytes().as_slice())
        .bind(APPROVAL_KIND)
        .bind(&body_json)
        .bind(&action_id)
        .bind(ApprovalState::PendingOpen.as_str())
        .bind(&expires_rfc)
        .bind(&now_rfc)
        .bind(&now_rfc)
        .bind(&now_rfc)
        .execute(self.pool())
        .await?;

        self.get_pending_approval(&action_id)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!("approval {action_id} missing immediately after insert"),
            })
    }

    /// Fetch a single approval row by external action id (ULID string).
    pub async fn get_pending_approval(
        &self,
        action_id: &str,
    ) -> Result<Option<PendingApprovalRow>> {
        let mut sql = String::from(APPROVAL_SELECT_SQL);
        sql.push_str(" WHERE action_id = ?");
        let row = sqlx::query(&sql)
            .bind(action_id)
            .fetch_optional(self.pool())
            .await?;
        row.map(approval_row_from_sqlite).transpose()
    }

    /// List approval rows matching `filter`, newest first.
    pub async fn list_pending_approvals(
        &self,
        filter: ApprovalFilter,
    ) -> Result<Vec<PendingApprovalRow>> {
        let mut sql = String::from(APPROVAL_SELECT_SQL);
        sql.push_str(" WHERE kind = ?");

        // Build dynamic predicates. Each one appends a placeholder we will
        // bind below in the same order.
        let state_filter = filter.state.unwrap_or_default();
        match state_filter {
            ApprovalStateFilter::Open => sql.push_str(" AND state = 'pending_open'"),
            ApprovalStateFilter::Decided => sql.push_str(
                " AND state IN ('pending_approved', 'pending_rejected', \
                 'pending_expired', 'pending_snoozed')",
            ),
            ApprovalStateFilter::All => {}
        }

        if filter.host_id.is_some() {
            sql.push_str(" AND host_id = ?");
        }
        if filter.stack.is_some() {
            sql.push_str(" AND json_extract(payload_json, '$.stack') = ?");
        }
        if filter.service.is_some() {
            sql.push_str(" AND json_extract(payload_json, '$.service') = ?");
        }
        if filter.since.is_some() {
            sql.push_str(" AND created_at >= ?");
        }
        sql.push_str(" ORDER BY created_at DESC");
        if filter.limit.is_some() {
            sql.push_str(" LIMIT ?");
        }

        let mut q = sqlx::query(&sql).bind(APPROVAL_KIND);
        if let Some(h) = filter.host_id {
            q = q.bind(h.to_bytes().to_vec());
        }
        if let Some(s) = &filter.stack {
            q = q.bind(s.clone());
        }
        if let Some(s) = &filter.service {
            q = q.bind(s.clone());
        }
        if let Some(ts) = filter.since {
            q = q.bind(ts.to_rfc3339());
        }
        if let Some(lim) = filter.limit {
            q = q.bind(lim as i64);
        }

        let rows = q.fetch_all(self.pool()).await?;
        rows.into_iter().map(approval_row_from_sqlite).collect()
    }

    /// Atomic transition `pending_open -> approved/rejected/snoozed`.
    ///
    /// Returns the freshly-decided row.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] if the row is already decided or
    /// doesn't exist; the message names the current state so the
    /// dashboard can render a precise error.
    pub async fn decide_pending_approval(
        &self,
        action_id: &str,
        decision: ApprovalDecision,
        decided_by: &str,
    ) -> Result<DecidedApproval> {
        let target = decision.target_state();
        let now_rfc = Utc::now().to_rfc3339();
        let result = sqlx::query(
            r#"
            UPDATE host_actions
               SET state       = ?,
                   decided_at  = ?,
                   decided_by  = ?,
                   updated_at  = ?
             WHERE action_id   = ?
               AND kind        = ?
               AND state       = 'pending_open'
            "#,
        )
        .bind(target.as_str())
        .bind(&now_rfc)
        .bind(decided_by)
        .bind(&now_rfc)
        .bind(action_id)
        .bind(APPROVAL_KIND)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the row doesn't exist or it isn't in pending_open. Look
            // it up to give the caller a precise error.
            return match self.get_pending_approval(action_id).await? {
                Some(row) => Err(Error::Conflict(format!(
                    "approval {action_id} is in state {} (not pending_open)",
                    row.state.as_str()
                ))),
                None => Err(Error::Conflict(format!("approval {action_id} not found"))),
            };
        }

        let row = self
            .get_pending_approval(action_id)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!("approval {action_id} vanished after decide"),
            })?;
        Ok(DecidedApproval {
            should_dispatch_apply_update: matches!(decision, ApprovalDecision::Approve),
            row,
        })
    }

    /// Bulk-transition every `pending_open` row whose `expires_at` is
    /// at or before `now` into `pending_expired`.
    ///
    /// Returns the rows that flipped so the caller can emit
    /// `update.expired` events.
    pub async fn expire_pending_approvals(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<PendingApprovalRow>> {
        let now_rfc = now.to_rfc3339();

        // SQLite doesn't support UPDATE ... RETURNING in every version we
        // support, so we collect the ids first, then update, then re-fetch.
        // The select + update are not in a transaction because expiry is
        // idempotent (a re-run picks up nothing new).
        let id_rows = sqlx::query(
            r#"
            SELECT action_id FROM host_actions
             WHERE kind = ?
               AND state = 'pending_open'
               AND expires_at <= ?
            "#,
        )
        .bind(APPROVAL_KIND)
        .bind(&now_rfc)
        .fetch_all(self.pool())
        .await?;

        if id_rows.is_empty() {
            return Ok(Vec::new());
        }

        sqlx::query(
            r#"
            UPDATE host_actions
               SET state      = 'pending_expired',
                   decided_at = ?,
                   decided_by = 'system:auto-expire',
                   updated_at = ?
             WHERE kind  = ?
               AND state = 'pending_open'
               AND expires_at <= ?
            "#,
        )
        .bind(&now_rfc)
        .bind(&now_rfc)
        .bind(APPROVAL_KIND)
        .bind(&now_rfc)
        .execute(self.pool())
        .await?;

        use sqlx::Row;
        let mut out = Vec::with_capacity(id_rows.len());
        for r in id_rows {
            let aid: String = r.try_get("action_id")?;
            if let Some(row) = self.get_pending_approval(&aid).await? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Idempotence helper.
    ///
    /// Is there already an open approval row for this
    /// `(host_id, stack, service, proposed_digest)` tuple? Returns
    /// the most recent open row if any.
    pub async fn find_open_approval_for_proposed_digest(
        &self,
        host_id: HostId,
        stack: &str,
        service: &str,
        proposed_digest: &str,
    ) -> Result<Option<PendingApprovalRow>> {
        let mut sql = String::from(APPROVAL_SELECT_SQL);
        sql.push_str(
            " WHERE kind = ? \
              AND state = 'pending_open' \
              AND host_id = ? \
              AND json_extract(payload_json, '$.stack') = ? \
              AND json_extract(payload_json, '$.service') = ? \
              AND json_extract(payload_json, '$.proposed_digest') = ? \
              ORDER BY created_at DESC LIMIT 1",
        );
        let row = sqlx::query(&sql)
            .bind(APPROVAL_KIND)
            .bind(host_id.to_bytes().as_slice())
            .bind(stack)
            .bind(service)
            .bind(proposed_digest)
            .fetch_optional(self.pool())
            .await?;
        row.map(approval_row_from_sqlite).transpose()
    }

    /// Stash Telegram notifier metadata on an existing approval row.
    ///
    /// T6 uses this to remember which message to edit when the
    /// approval is decided. Idempotent: re-calling overwrites prior
    /// values.
    pub async fn set_approval_message_metadata(
        &self,
        action_id: &str,
        chat_id: i64,
        message_id: i64,
    ) -> Result<()> {
        self.merge_approval_metadata(
            action_id,
            &[
                ("notifier_chat_id", serde_json::Value::from(chat_id)),
                ("notifier_message_id", serde_json::Value::from(message_id)),
            ],
        )
        .await
    }

    /// Stash Discord notifier metadata on an existing approval row.
    ///
    /// Used so the dashboard callback can edit the originating
    /// Discord message after a decision. Idempotent and disjoint from
    /// [`Self::set_approval_message_metadata`] so Telegram and
    /// Discord can coexist on the same row.
    pub async fn set_discord_approval_message_metadata(
        &self,
        action_id: &str,
        channel_id: i64,
        message_id: i64,
    ) -> Result<()> {
        self.merge_approval_metadata(
            action_id,
            &[
                (
                    "notifier_discord_channel_id",
                    serde_json::Value::from(channel_id),
                ),
                (
                    "notifier_discord_message_id",
                    serde_json::Value::from(message_id),
                ),
            ],
        )
        .await
    }

    /// Read-modify-write merge for the approval row's metadata_json.
    ///
    /// Each `(key, value)` pair is upserted into the JSON object.
    /// Other keys are preserved.
    async fn merge_approval_metadata(
        &self,
        action_id: &str,
        kvs: &[(&str, serde_json::Value)],
    ) -> Result<()> {
        // Fetch -> merge -> write rather than json_patch, which isn't
        // available across all sqlite versions. Approval rows are small.
        let existing = self
            .get_pending_approval(action_id)
            .await?
            .ok_or_else(|| Error::Conflict(format!("approval {action_id} not found")))?;
        let mut meta = existing
            .metadata_json
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if !meta.is_object() {
            meta = serde_json::Value::Object(serde_json::Map::new());
        }
        if let Some(obj) = meta.as_object_mut() {
            for (k, v) in kvs {
                obj.insert((*k).to_string(), v.clone());
            }
        }
        let meta_json = serde_json::to_string(&meta).map_err(|e| Error::Decode {
            reason: format!("serializing approval metadata: {e}"),
        })?;
        let now_rfc = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE host_actions SET metadata_json = ?, updated_at = ? \
             WHERE action_id = ? AND kind = ?",
        )
        .bind(&meta_json)
        .bind(&now_rfc)
        .bind(action_id)
        .bind(APPROVAL_KIND)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

/// Column list shared by every approval-row SELECT. Callers append a WHERE
/// clause and (optionally) ORDER/LIMIT.
const APPROVAL_SELECT_SQL: &str = "SELECT host_id, payload_json, action_id, state, expires_at, \
            decided_at, decided_by, metadata_json, created_at, updated_at \
     FROM host_actions";

/// Decode one `host_actions` row into a typed approval row.
fn approval_row_from_sqlite(row: sqlx::sqlite::SqliteRow) -> Result<PendingApprovalRow> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    if host_bytes.len() != 16 {
        return Err(Error::InvalidHostId(host_bytes.len()));
    }
    let payload: String = row.try_get("payload_json")?;
    let body: UpdateApprovalBody = serde_json::from_str(&payload).map_err(|e| Error::Decode {
        reason: format!("deserializing UpdateApprovalBody: {e}"),
    })?;

    let action_id: Option<String> = row.try_get("action_id")?;
    let action_id = action_id.ok_or_else(|| Error::Decode {
        reason: "approval row missing action_id".into(),
    })?;
    let state_s: Option<String> = row.try_get("state")?;
    let state = state_s
        .as_deref()
        .ok_or_else(|| Error::Decode {
            reason: "approval row missing state".into(),
        })
        .and_then(ApprovalState::from_str)?;

    let expires_at = parse_rfc_or_sqlite_dt(row.try_get::<Option<String>, _>("expires_at")?)?
        .ok_or_else(|| Error::Decode {
            reason: "approval row missing expires_at".into(),
        })?;
    let decided_at = parse_rfc_or_sqlite_dt(row.try_get::<Option<String>, _>("decided_at")?)?;
    let decided_by: Option<String> = row.try_get("decided_by")?;

    let metadata_raw: Option<String> = row.try_get("metadata_json")?;
    let metadata_json = match metadata_raw {
        Some(s) if !s.is_empty() => Some(serde_json::from_str(&s).map_err(|e| Error::Decode {
            reason: format!("deserializing approval metadata: {e}"),
        })?),
        _ => None,
    };

    let created_at = parse_rfc_or_sqlite_dt(Some(row.try_get::<String, _>("created_at")?))?
        .ok_or_else(|| Error::Decode {
            reason: "approval row missing created_at".into(),
        })?;
    let updated_at = parse_rfc_or_sqlite_dt(row.try_get::<Option<String>, _>("updated_at")?)?
        .unwrap_or(created_at);

    Ok(PendingApprovalRow {
        action_id,
        state,
        body,
        expires_at,
        decided_at,
        decided_by,
        metadata_json,
        created_at,
        updated_at,
    })
}

/// Parse a sqlite TEXT timestamp that may be RFC3339 ("...Z") or sqlite's
/// default `YYYY-MM-DD HH:MM:SS` format. Mirrors the helper in policy.rs.
fn parse_rfc_or_sqlite_dt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    let Some(raw) = s else { return Ok(None) };
    let dt = DateTime::parse_from_rfc3339(&raw)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%d %H:%M:%S").map(|n| n.and_utc())
        })
        .map_err(|e| Error::Decode {
            reason: format!("bad timestamp '{raw}': {e}"),
        })?;
    Ok(Some(dt))
}
