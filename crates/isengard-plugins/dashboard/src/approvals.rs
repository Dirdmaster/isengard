//! REST endpoints for the pending-approval queue (Phase 9 Plan B, T4).
//!
//! See spec §"REST: dashboard plugin" of
//! `docs/superpowers/specs/2026-05-06-phase-9e-9f-approval-flow-design.md`.
//!
//! Mounted under `/api/v1` by `lib.rs`. Routes:
//!
//! | Method | Path                            | Purpose                                  |
//! |--------|---------------------------------|------------------------------------------|
//! | GET    | `/approvals?state=&host_id=...` | List, default `state=open`, newest-first |
//! | GET    | `/approvals/:id`                | Single row by ULID action_id             |
//! | POST   | `/approvals/:id`                | Decide (approve/reject/snooze)           |
//! | POST   | `/notifier/callback/telegram`   | Telegram webhook callback for inline btn |
//!
//! Decision flow:
//! 1. `decide_pending_approval` atomically transitions `pending_open` to one
//!    of `pending_approved`/`pending_rejected`/`pending_snoozed`.
//! 2. On approve: queue a `force_update` HostAction so the agent picks up the
//!    update on its next sync.
//! 3. On snooze: upsert a service-scope policy with `paused_until = now + N`
//!    so the next updater scan skips this service.
//! 4. Emit `update.approved` / `update.rejected` / `update.snoozed` on the
//!    controller's EventBus so the journal + notifier can react.
//!
//! Telegram callback flow:
//! 1. Verify `X-Telegram-Bot-Api-Secret-Token` matches env
//!    `ISENGARD_TELEGRAM_WEBHOOK_SECRET` via constant-time compare. 401 on
//!    mismatch or unset env.
//! 2. Parse `callback_query.data` as `apv:<action_id>:<decision>[:hours]`.
//! 3. Dispatch to the same `decide_pending_approval` path.
//! 4. Edit the original Telegram message via the notifier helper to flip the
//!    text and drop the inline keyboard.
//! 5. Return Telegram's expected `answerCallbackQuery` shape so the user's
//!    button stops spinning.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Duration, Utc};
use isengard_controller::ControllerHandles;
use isengard_core::Event;
use isengard_core::event::kinds::{UPDATE_APPROVED, UPDATE_REJECTED, UPDATE_SNOOZED};
use isengard_core::policy::{Policy, PolicyScopeType};
use isengard_plugin_notifier::telegram::edit_telegram_message_text;
use isengard_storage::HostActionKind;
use isengard_storage::host_action::{
    ApprovalDecision, ApprovalFilter, ApprovalState, ApprovalStateFilter, PendingApprovalRow,
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

const TELEGRAM_WEBHOOK_SECRET_ENV: &str = "ISENGARD_TELEGRAM_WEBHOOK_SECRET";
const TELEGRAM_BOT_TOKEN_ENV: &str = "ISENGARD_TELEGRAM_BOT_TOKEN";
const TELEGRAM_API_BASE_ENV: &str = "ISENGARD_TELEGRAM_API_BASE";
const TELEGRAM_SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/approvals", get(list_approvals))
        .route("/approvals/{id}", get(get_approval).post(decide_approval))
        .route("/notifier/callback/telegram", post(telegram_callback))
        .with_state(handles)
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// JSON projection of a `PendingApprovalRow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDto {
    pub action_id: String,
    pub state: ApprovalState,
    pub host_id: String,
    pub stack: String,
    pub service: String,
    pub container_name: String,
    pub image: String,
    pub current_digest: String,
    pub proposed_digest: String,
    pub diff_url: Option<String>,
    pub approver_channel: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<PendingApprovalRow> for ApprovalDto {
    fn from(r: PendingApprovalRow) -> Self {
        Self {
            action_id: r.action_id,
            state: r.state,
            host_id: r.body.host_id.to_string(),
            stack: r.body.stack,
            service: r.body.service,
            container_name: r.body.container_name,
            image: r.body.image,
            current_digest: r.body.current_digest,
            proposed_digest: r.body.proposed_digest,
            diff_url: r.body.diff_url,
            approver_channel: r.body.approver_channel,
            expires_at: r.expires_at,
            decided_at: r.decided_at,
            decided_by: r.decided_by,
            metadata: r.metadata_json,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalListQuery {
    /// One of `open` (default), `decided`, `all`.
    pub state: Option<String>,
    pub host_id: Option<String>,
    pub stack: Option<String>,
    pub service: Option<String>,
    /// RFC3339 timestamp; rows older than this are excluded.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionDto {
    /// One of `approve`, `reject`, `snooze`.
    pub decision: String,
    /// Required when `decision == "snooze"`; ignored otherwise.
    pub snooze_hours: Option<u32>,
    /// Optional operator identity; defaults to `"dashboard"` when absent.
    pub decided_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionResponseDto {
    pub approval: ApprovalDto,
    /// `true` iff a `force_update` HostAction was queued for the agent.
    pub dispatched_apply_update: bool,
    /// When `decision == "snooze"`, the `paused_until` timestamp written to
    /// the service-scope policy.
    pub paused_until_set: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

// ---------------------------------------------------------------------------
// Decision parsing
// ---------------------------------------------------------------------------

/// Parsed, validated form of the `DecisionDto.decision` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedDecision {
    Approve,
    Reject,
    Snooze(u32),
}

impl ParsedDecision {
    fn to_storage(self) -> ApprovalDecision {
        match self {
            Self::Approve => ApprovalDecision::Approve,
            Self::Reject => ApprovalDecision::Reject,
            Self::Snooze(h) => ApprovalDecision::SnoozeHours(h),
        }
    }

    fn event_kind(self) -> &'static str {
        match self {
            Self::Approve => UPDATE_APPROVED,
            Self::Reject => UPDATE_REJECTED,
            Self::Snooze(_) => UPDATE_SNOOZED,
        }
    }
}

/// Validate a `DecisionDto`. Returns:
/// - 422 if decision is not one of approve/reject/snooze
/// - 400 if decision=snooze but snooze_hours is absent or zero
fn parse_dashboard_decision(body: &DecisionDto) -> Result<ParsedDecision, Response> {
    match body.decision.as_str() {
        "approve" => Ok(ParsedDecision::Approve),
        "reject" => Ok(ParsedDecision::Reject),
        "snooze" => match body.snooze_hours {
            Some(h) if h > 0 => Ok(ParsedDecision::Snooze(h)),
            Some(_) => Err(err(
                StatusCode::BAD_REQUEST,
                "snooze_hours must be greater than zero",
            )),
            None => Err(err(
                StatusCode::BAD_REQUEST,
                "snooze_hours is required when decision=snooze",
            )),
        },
        other => Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid decision '{other}': allowed values are approve, reject, snooze"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_approvals(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ApprovalListQuery>,
) -> Response {
    let state_filter = match q.state.as_deref() {
        None | Some("open") => ApprovalStateFilter::Open,
        Some("decided") => ApprovalStateFilter::Decided,
        Some("all") => ApprovalStateFilter::All,
        Some(other) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("invalid state filter '{other}': allowed values are open, decided, all"),
            );
        }
    };

    let host_id = match q.host_id.as_deref() {
        Some(s) => match parse_host_id(s) {
            Ok(h) => Some(h),
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid host_id: {e}")),
        },
        None => None,
    };

    let since = match q.since.as_deref() {
        Some(s) => match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(dt.with_timezone(&Utc)),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("invalid since (RFC3339): {e}"),
                );
            }
        },
        None => None,
    };

    let filter = ApprovalFilter {
        state: Some(state_filter),
        host_id,
        stack: q.stack,
        service: q.service,
        since,
        limit: None,
    };

    match handles.inventory.list_pending_approvals(filter).await {
        Ok(rows) => {
            let dtos: Vec<ApprovalDto> = rows.into_iter().map(ApprovalDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list approvals: {e}"),
        ),
    }
}

async fn get_approval(
    State(handles): State<Arc<ControllerHandles>>,
    Path(action_id): Path<String>,
) -> Response {
    match handles.inventory.get_pending_approval(&action_id).await {
        Ok(Some(row)) => Json(ApprovalDto::from(row)).into_response(),
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            format!("approval {action_id} not found"),
        ),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get approval: {e}"),
        ),
    }
}

async fn decide_approval(
    State(handles): State<Arc<ControllerHandles>>,
    Path(action_id): Path<String>,
    Json(body): Json<DecisionDto>,
) -> Response {
    let parsed = match parse_dashboard_decision(&body) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let decided_by = body.decided_by.unwrap_or_else(|| "dashboard".to_string());
    apply_decision(&handles, &action_id, parsed, &decided_by).await
}

/// Shared decision path used by both the dashboard POST and the Telegram
/// callback. Returns a response carrying a `DecisionResponseDto` on success.
async fn apply_decision(
    handles: &Arc<ControllerHandles>,
    action_id: &str,
    parsed: ParsedDecision,
    decided_by: &str,
) -> Response {
    let storage_decision = parsed.to_storage();

    let decided = match handles
        .inventory
        .decide_pending_approval(action_id, storage_decision, decided_by)
        .await
    {
        Ok(d) => d,
        Err(isengard_storage::Error::Conflict(msg)) => {
            return err(StatusCode::CONFLICT, msg);
        }
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decide approval: {e}"),
            );
        }
    };

    // On Approve: queue the apply-update HostAction so the agent picks it up
    // on its next sync. We currently use ForceUpdate with the stack name from
    // the approval body; the agent's updater treats this as "re-pull and
    // recreate the affected stack". A future iteration may add a typed
    // `ApplyUpdate { service, proposed_digest }` kind.
    let mut dispatched = false;
    let mut paused_until_set: Option<DateTime<Utc>> = None;

    if decided.should_dispatch_apply_update {
        let host_id = decided.row.body.host_id;
        let stack_name = decided.row.body.stack.clone();
        match handles
            .inventory
            .queue_action(
                host_id,
                HostActionKind::ForceUpdate {
                    stack_name: Some(stack_name),
                },
            )
            .await
        {
            Ok(_) => {
                dispatched = true;
            }
            Err(e) => {
                warn!(
                    action_id = %action_id,
                    error = %e,
                    "decide_approval: failed to queue apply_update HostAction; row already \
                     transitioned. Operator may need to retry via force-update endpoint."
                );
            }
        }
    }

    if let ParsedDecision::Snooze(hours) = parsed {
        let until = Utc::now() + Duration::hours(hours as i64);
        paused_until_set = Some(until);
        // Merge with existing service-scope policy (if any) so we don't blow
        // away strategy/gate/on_failure overrides. read-modify-write.
        let scope_key = decided.row.body.service.clone();
        let merged = match handles
            .inventory
            .get_policy(PolicyScopeType::Service, &scope_key)
            .await
        {
            Ok(Some(existing)) => Policy {
                paused_until: Some(until),
                ..existing.body
            },
            Ok(None) => Policy {
                paused_until: Some(until),
                ..Default::default()
            },
            Err(e) => {
                warn!(
                    action_id = %action_id,
                    error = %e,
                    "decide_approval: failed to read existing service policy; writing fresh row"
                );
                Policy {
                    paused_until: Some(until),
                    ..Default::default()
                }
            }
        };
        if let Err(e) = handles
            .inventory
            .upsert_policy(PolicyScopeType::Service, &scope_key, &merged)
            .await
        {
            warn!(
                action_id = %action_id,
                service = %scope_key,
                error = %e,
                "decide_approval: failed to upsert service-scope paused_until policy; \
                 row state still transitioned to pending_snoozed"
            );
            paused_until_set = None;
        }
    }

    // Emit the lifecycle event on the bus so journal + notifier can react.
    let row = &decided.row;
    let mut event = Event {
        kind: parsed.event_kind().to_string(),
        occurred_at: Utc::now(),
        host_id: Some(row.body.host_id.into()),
        summary: format!(
            "{} for {}/{}: {}",
            parsed.event_kind(),
            row.body.stack,
            row.body.service,
            decided_by
        ),
        container_name: Some(row.body.container_name.clone()),
        image: Some(row.body.image.clone()),
        old_digest: Some(row.body.current_digest.clone()),
        new_digest: Some(row.body.proposed_digest.clone()),
        ..Default::default()
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "action_id".to_string(),
        serde_json::Value::String(row.action_id.clone()),
    );
    metadata.insert(
        "decided_by".to_string(),
        serde_json::Value::String(decided_by.to_string()),
    );
    if let ParsedDecision::Snooze(hours) = parsed {
        metadata.insert("snooze_hours".to_string(), serde_json::Value::from(hours));
        if let Some(pu) = paused_until_set {
            metadata.insert(
                "paused_until".to_string(),
                serde_json::Value::String(pu.to_rfc3339()),
            );
        }
    }
    event.metadata = serde_json::Value::Object(metadata);
    handles.bus.publish(event);

    let resp = DecisionResponseDto {
        approval: ApprovalDto::from(decided.row),
        dispatched_apply_update: dispatched,
        paused_until_set,
    };
    Json(resp).into_response()
}

fn parse_host_id(s: &str) -> Result<isengard_storage::HostId, String> {
    let ulid = s.parse::<ulid::Ulid>().map_err(|e| format!("{e}"))?;
    Ok(isengard_storage::HostId::from(ulid))
}

// ---------------------------------------------------------------------------
// Telegram callback
// ---------------------------------------------------------------------------

/// Telegram update body shape for inline-keyboard callbacks. Only the fields
/// we care about are deserialized; everything else is ignored.
#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<TelegramMessageRef>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessageRef {
    message_id: i64,
    chat: TelegramChatRef,
}

#[derive(Debug, Deserialize)]
struct TelegramChatRef {
    id: i64,
}

/// Telegram-shaped response. Returning this from the webhook tells Telegram
/// to invoke `answerCallbackQuery` with the specified text so the user's
/// button stops spinning. Returning the body is the documented "trick"
/// alternative to making a separate POST to /answerCallbackQuery.
#[derive(Debug, Serialize)]
struct AnswerCallbackQueryReply<'a> {
    method: &'a str,
    callback_query_id: &'a str,
    text: &'a str,
}

async fn telegram_callback(
    State(handles): State<Arc<ControllerHandles>>,
    headers: HeaderMap,
    Json(update): Json<TelegramUpdate>,
) -> Response {
    // 1. Verify webhook secret (constant-time compare; both must be set).
    let configured = match std::env::var(TELEGRAM_WEBHOOK_SECRET_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            warn!(
                env = TELEGRAM_WEBHOOK_SECRET_ENV,
                "telegram callback rejected: webhook secret env not set"
            );
            return err(
                StatusCode::UNAUTHORIZED,
                "telegram webhook secret not configured",
            );
        }
    };
    let provided = headers
        .get(TELEGRAM_SECRET_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), configured.as_bytes()) {
        return err(StatusCode::UNAUTHORIZED, "invalid telegram webhook secret");
    }

    // 2. Pull the callback_query out of the update.
    let cq = match update.callback_query {
        Some(c) => c,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "telegram update has no callback_query",
            );
        }
    };
    let data = match cq.data.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "telegram callback_query has no data",
            );
        }
    };

    // 3. Parse `apv:<action_id>:<decision>[:hours]`.
    let parsed = match parse_callback_data(data) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };

    // 4. Resolve decided_by from the user (username preferred, then first_name).
    let decided_by = cq
        .from
        .as_ref()
        .and_then(|u| {
            u.username
                .as_ref()
                .map(|n| format!("telegram:@{n}"))
                .or_else(|| u.first_name.as_ref().map(|n| format!("telegram:{n}")))
        })
        .unwrap_or_else(|| "telegram".to_string());

    // 5. Apply the decision via the shared path. We don't return the
    // `DecisionResponseDto` directly because Telegram expects a specific
    // response shape; we just need to confirm it succeeded.
    let storage_decision = parsed.parsed.to_storage();
    let decide_res = handles
        .inventory
        .decide_pending_approval(&parsed.action_id, storage_decision, &decided_by)
        .await;

    let decided = match decide_res {
        Ok(d) => d,
        Err(isengard_storage::Error::Conflict(msg)) => {
            // Already decided (race with dashboard). Inform Telegram with a
            // 200 + answerCallbackQuery so the spinner stops, but include the
            // conflict reason in the popup text.
            return Json(AnswerCallbackQueryReply {
                method: "answerCallbackQuery",
                callback_query_id: &cq.id,
                text: &format!("Already decided: {msg}"),
            })
            .into_response();
        }
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decide approval: {e}"),
            );
        }
    };

    // 6. Run the side-effects (queue HostAction / upsert paused_until / emit
    // event). Reuse the shared logic by calling apply_decision_side_effects.
    let mut dispatched = false;
    let mut paused_until_set: Option<DateTime<Utc>> = None;

    if decided.should_dispatch_apply_update {
        let host_id = decided.row.body.host_id;
        let stack_name = decided.row.body.stack.clone();
        if let Err(e) = handles
            .inventory
            .queue_action(
                host_id,
                HostActionKind::ForceUpdate {
                    stack_name: Some(stack_name),
                },
            )
            .await
        {
            warn!(
                action_id = %parsed.action_id,
                error = %e,
                "telegram callback: failed to queue apply_update HostAction"
            );
        } else {
            dispatched = true;
        }
    }

    if let ParsedDecision::Snooze(hours) = parsed.parsed {
        let until = Utc::now() + Duration::hours(hours as i64);
        let scope_key = decided.row.body.service.clone();
        let merged = match handles
            .inventory
            .get_policy(PolicyScopeType::Service, &scope_key)
            .await
        {
            Ok(Some(existing)) => Policy {
                paused_until: Some(until),
                ..existing.body
            },
            _ => Policy {
                paused_until: Some(until),
                ..Default::default()
            },
        };
        if handles
            .inventory
            .upsert_policy(PolicyScopeType::Service, &scope_key, &merged)
            .await
            .is_ok()
        {
            paused_until_set = Some(until);
        }
    }

    // Emit lifecycle event on the bus.
    let row = &decided.row;
    let mut event = Event {
        kind: parsed.parsed.event_kind().to_string(),
        occurred_at: Utc::now(),
        host_id: Some(row.body.host_id.into()),
        summary: format!(
            "{} for {}/{}: {}",
            parsed.parsed.event_kind(),
            row.body.stack,
            row.body.service,
            decided_by
        ),
        container_name: Some(row.body.container_name.clone()),
        image: Some(row.body.image.clone()),
        old_digest: Some(row.body.current_digest.clone()),
        new_digest: Some(row.body.proposed_digest.clone()),
        ..Default::default()
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "action_id".to_string(),
        serde_json::Value::String(row.action_id.clone()),
    );
    metadata.insert(
        "decided_by".to_string(),
        serde_json::Value::String(decided_by.clone()),
    );
    metadata.insert(
        "source".to_string(),
        serde_json::Value::String("telegram_callback".to_string()),
    );
    if let ParsedDecision::Snooze(hours) = parsed.parsed {
        metadata.insert("snooze_hours".to_string(), serde_json::Value::from(hours));
        if let Some(pu) = paused_until_set {
            metadata.insert(
                "paused_until".to_string(),
                serde_json::Value::String(pu.to_rfc3339()),
            );
        }
    }
    event.metadata = serde_json::Value::Object(metadata);
    handles.bus.publish(event);

    // 7. Edit the original Telegram message (best-effort). We pull
    // (chat_id, message_id) from the inline `message` field on the callback
    // query if present (cheapest path), falling back to the persisted
    // metadata on the approval row otherwise.
    let (chat_id_for_edit, message_id_for_edit) =
        match (cq.message.as_ref(), &decided.row.metadata_json) {
            (Some(m), _) => (Some(m.chat.id), Some(m.message_id)),
            (None, Some(meta)) => (
                meta.get("notifier_chat_id").and_then(|v| v.as_i64()),
                meta.get("notifier_message_id").and_then(|v| v.as_i64()),
            ),
            _ => (None, None),
        };

    if let (Some(chat_id), Some(message_id)) = (chat_id_for_edit, message_id_for_edit) {
        let edited_text = render_decided_message_text(parsed.parsed, &decided_by, dispatched);
        match std::env::var(TELEGRAM_BOT_TOKEN_ENV) {
            Ok(token) if !token.is_empty() => {
                let api_base = std::env::var(TELEGRAM_API_BASE_ENV).ok();
                if let Err(e) = edit_telegram_message_text(
                    api_base.as_deref(),
                    &token,
                    &chat_id.to_string(),
                    message_id,
                    &edited_text,
                )
                .await
                {
                    warn!(
                        action_id = %parsed.action_id,
                        error = %e,
                        "telegram callback: editMessageText failed; decision still applied"
                    );
                }
            }
            _ => {
                debug!(
                    "telegram callback: ISENGARD_TELEGRAM_BOT_TOKEN unset; skipping editMessageText"
                );
            }
        }
    }

    // 8. Reply with the answerCallbackQuery shape so Telegram can stop the
    // spinner on the user's button. Embedding it in the webhook reply is
    // documented behaviour (saves an extra HTTP round-trip).
    let popup = match parsed.parsed {
        ParsedDecision::Approve => "Approved",
        ParsedDecision::Reject => "Rejected",
        ParsedDecision::Snooze(_) => "Snoozed",
    };
    Json(AnswerCallbackQueryReply {
        method: "answerCallbackQuery",
        callback_query_id: &cq.id,
        text: popup,
    })
    .into_response()
}

#[derive(Debug)]
struct ParsedCallbackData {
    action_id: String,
    parsed: ParsedDecision,
}

/// Parse `apv:<action_id>:<decision>[:hours]`. The action_id may not contain
/// `:` (ULIDs are crockford alphanumeric, so this holds).
fn parse_callback_data(data: &str) -> Result<ParsedCallbackData, String> {
    let mut parts = data.split(':');
    match parts.next() {
        Some("apv") => {}
        _ => return Err(format!("callback_data missing 'apv:' prefix: {data}")),
    }
    let action_id = parts
        .next()
        .ok_or_else(|| format!("callback_data missing action_id: {data}"))?;
    if action_id.is_empty() {
        return Err(format!("callback_data has empty action_id: {data}"));
    }
    let decision_s = parts
        .next()
        .ok_or_else(|| format!("callback_data missing decision: {data}"))?;
    let parsed = match decision_s {
        "approve" => ParsedDecision::Approve,
        "reject" => ParsedDecision::Reject,
        "snooze" => {
            let hours_s = parts
                .next()
                .ok_or_else(|| format!("callback_data missing snooze hours: {data}"))?;
            let hours: u32 = hours_s
                .parse()
                .map_err(|e| format!("callback_data snooze hours not u32: {e}"))?;
            if hours == 0 {
                return Err(format!("callback_data snooze hours must be > 0: {data}"));
            }
            ParsedDecision::Snooze(hours)
        }
        other => {
            return Err(format!(
                "unknown decision '{other}' in callback_data: {data}"
            ));
        }
    };
    if parts.next().is_some() {
        return Err(format!("callback_data has trailing components: {data}"));
    }
    Ok(ParsedCallbackData {
        action_id: action_id.to_string(),
        parsed,
    })
}

/// Constant-time byte compare so a bad webhook secret can't be timed out.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

fn render_decided_message_text(
    decision: ParsedDecision,
    decided_by: &str,
    dispatched: bool,
) -> String {
    let now = Utc::now();
    let stamp = now.format("%H:%M UTC");
    match decision {
        ParsedDecision::Approve => {
            let suffix = if dispatched {
                " (apply_update queued)"
            } else {
                ""
            };
            format!("Approved by {decided_by} at {stamp}{suffix}")
        }
        ParsedDecision::Reject => format!("Rejected by {decided_by} at {stamp}"),
        ParsedDecision::Snooze(hours) => {
            format!("Snoozed by {decided_by} at {stamp} for {hours}h")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_data_approve() {
        let p = parse_callback_data("apv:01ABC:approve").unwrap();
        assert_eq!(p.action_id, "01ABC");
        assert_eq!(p.parsed, ParsedDecision::Approve);
    }

    #[test]
    fn parse_callback_data_reject() {
        let p = parse_callback_data("apv:01ABC:reject").unwrap();
        assert_eq!(p.parsed, ParsedDecision::Reject);
    }

    #[test]
    fn parse_callback_data_snooze_with_hours() {
        let p = parse_callback_data("apv:01ABC:snooze:24").unwrap();
        assert_eq!(p.parsed, ParsedDecision::Snooze(24));
    }

    #[test]
    fn parse_callback_data_snooze_without_hours_errors() {
        assert!(parse_callback_data("apv:01ABC:snooze").is_err());
    }

    #[test]
    fn parse_callback_data_bad_prefix_errors() {
        assert!(parse_callback_data("xxx:01ABC:approve").is_err());
    }

    #[test]
    fn parse_callback_data_unknown_decision_errors() {
        assert!(parse_callback_data("apv:01ABC:explode").is_err());
    }

    #[test]
    fn parse_callback_data_empty_action_id_errors() {
        assert!(parse_callback_data("apv::approve").is_err());
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn parse_dashboard_decision_approve_ok() {
        let dto = DecisionDto {
            decision: "approve".into(),
            snooze_hours: None,
            decided_by: None,
        };
        assert_eq!(
            parse_dashboard_decision(&dto).unwrap(),
            ParsedDecision::Approve
        );
    }

    #[test]
    fn parse_dashboard_decision_snooze_without_hours_400() {
        let dto = DecisionDto {
            decision: "snooze".into(),
            snooze_hours: None,
            decided_by: None,
        };
        let err = parse_dashboard_decision(&dto).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_dashboard_decision_unknown_422() {
        let dto = DecisionDto {
            decision: "explode".into(),
            snooze_hours: None,
            decided_by: None,
        };
        let err = parse_dashboard_decision(&dto).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
