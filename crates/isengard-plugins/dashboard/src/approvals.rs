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
//! | POST   | `/notifier/callback/discord`    | Discord interactions endpoint (Phase 9c) |
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
//!
//! Discord callback flow (Phase 9c):
//! 1. Verify the ed25519 signature over `timestamp || raw_body` using
//!    `ISENGARD_DISCORD_PUBLIC_KEY`. 401 on any failure.
//! 2. Parse the interaction body. PING (type=1) returns PONG; MESSAGE_COMPONENT
//!    (type=3) parses `data.custom_id` and dispatches to `decide_pending_approval`.
//! 3. Respond with UPDATE_MESSAGE (type=7) so Discord clears the buttons in
//!    place. The optional out-of-band PATCH covers the rare case where the
//!    interaction omitted a message reference.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Duration, Utc};
use isengard_controller::ControllerHandles;
use isengard_core::Event;
use isengard_core::event::kinds::{UPDATE_APPROVED, UPDATE_REJECTED, UPDATE_SNOOZED};
use isengard_core::policy::{Policy, PolicyScopeType};
use isengard_plugin_notifier::discord::{edit_discord_message_text, verify_discord_signature};
use isengard_plugin_notifier::telegram::edit_telegram_message_text;
use isengard_storage::HostActionKind;
use isengard_storage::host_action::{
    ApprovalDecision, ApprovalFilter, ApprovalState, ApprovalStateFilter, DecidedApproval,
    PendingApprovalRow,
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

const TELEGRAM_WEBHOOK_SECRET_ENV: &str = "ISENGARD_TELEGRAM_WEBHOOK_SECRET";
const TELEGRAM_BOT_TOKEN_ENV: &str = "ISENGARD_TELEGRAM_BOT_TOKEN";
const TELEGRAM_API_BASE_ENV: &str = "ISENGARD_TELEGRAM_API_BASE";
const TELEGRAM_SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

const DISCORD_PUBLIC_KEY_ENV: &str = "ISENGARD_DISCORD_PUBLIC_KEY";
const DISCORD_BOT_TOKEN_ENV: &str = "ISENGARD_DISCORD_BOT_TOKEN";
const DISCORD_API_BASE_ENV: &str = "ISENGARD_DISCORD_API_BASE";
const DISCORD_SIGNATURE_HEADER: &str = "x-signature-ed25519";
const DISCORD_TIMESTAMP_HEADER: &str = "x-signature-timestamp";

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/approvals", get(list_approvals))
        .route("/approvals/{id}", get(get_approval).post(decide_approval))
        .route("/notifier/callback/telegram", post(telegram_callback))
        .route("/notifier/callback/discord", post(discord_callback))
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

// ---------------------------------------------------------------------------
// Discord callback
// ---------------------------------------------------------------------------

/// Discord interaction body. Discord uses snake_case AND camelCase across
/// fields in its API; this struct deserializes the subset we care about.
#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    /// 1=PING, 2=APPLICATION_COMMAND, 3=MESSAGE_COMPONENT, 4=APPLICATION_COMMAND_AUTOCOMPLETE,
    /// 5=MODAL_SUBMIT. We handle 1 and 3.
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    data: Option<DiscordInteractionData>,
    #[serde(default)]
    member: Option<DiscordMember>,
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    message: Option<DiscordMessageRef>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    #[serde(default)]
    custom_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordMember {
    #[serde(default)]
    user: Option<DiscordUser>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    global_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordMessageRef {
    /// Snowflake string. Parsed to i64 at use site.
    id: String,
    #[serde(default)]
    channel_id: Option<String>,
}

/// Discord interaction response shapes. Type 1 = PONG (response to PING),
/// type 7 = UPDATE_MESSAGE (edit the source component message). Other
/// response types are not used by this handler.
#[derive(Debug, Serialize)]
struct DiscordPong {
    #[serde(rename = "type")]
    kind: u8,
}

#[derive(Debug, Serialize)]
struct DiscordUpdateMessage<'a> {
    #[serde(rename = "type")]
    kind: u8,
    data: DiscordUpdateMessageData<'a>,
}

#[derive(Debug, Serialize)]
struct DiscordUpdateMessageData<'a> {
    content: &'a str,
    components: &'a [serde_json::Value],
}

async fn discord_callback(
    State(handles): State<Arc<ControllerHandles>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Verify signature (public key + headers required).
    let public_key = match std::env::var(DISCORD_PUBLIC_KEY_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            warn!(
                env = DISCORD_PUBLIC_KEY_ENV,
                "discord callback rejected: public key env not set"
            );
            return err(
                StatusCode::UNAUTHORIZED,
                "discord public key not configured",
            );
        }
    };
    let signature = match headers
        .get(DISCORD_SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return err(
                StatusCode::UNAUTHORIZED,
                "missing X-Signature-Ed25519 header",
            );
        }
    };
    let timestamp = match headers
        .get(DISCORD_TIMESTAMP_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return err(
                StatusCode::UNAUTHORIZED,
                "missing X-Signature-Timestamp header",
            );
        }
    };
    if let Err(e) = verify_discord_signature(&public_key, timestamp.as_bytes(), &body, &signature) {
        debug!(error = %e, "discord signature verify failed");
        return err(StatusCode::UNAUTHORIZED, "invalid discord signature");
    }

    // 2. Parse interaction body.
    let interaction: DiscordInteraction = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("malformed discord interaction body: {e}"),
            );
        }
    };

    match interaction.kind {
        1 => {
            // PING: echo PONG. Discord uses this at endpoint registration time
            // to verify the URL implements the protocol.
            Json(DiscordPong { kind: 1 }).into_response()
        }
        3 => discord_message_component(&handles, interaction).await,
        other => err(
            StatusCode::BAD_REQUEST,
            format!("unsupported discord interaction type {other}"),
        ),
    }
}

async fn discord_message_component(
    handles: &Arc<ControllerHandles>,
    interaction: DiscordInteraction,
) -> Response {
    // Extract custom_id.
    let custom_id = match interaction
        .data
        .as_ref()
        .and_then(|d| d.custom_id.as_deref())
    {
        Some(s) if !s.is_empty() => s,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "discord component interaction missing custom_id",
            );
        }
    };

    let parsed = match parse_callback_data(custom_id) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };

    // Resolve decided_by from member.user.username (guild context) or
    // user.username (DM). Fall back to global_name then "discord".
    let decided_by = resolve_discord_decided_by(&interaction);

    // Apply the decision.
    let storage_decision = parsed.parsed.to_storage();
    let decide_res = handles
        .inventory
        .decide_pending_approval(&parsed.action_id, storage_decision, &decided_by)
        .await;
    let decided = match decide_res {
        Ok(d) => d,
        Err(isengard_storage::Error::Conflict(msg)) => {
            // Already decided. Return UPDATE_MESSAGE so Discord swaps the
            // buttons for an explanation rather than leaving them clickable.
            let text = format!("Already decided: {msg}");
            return discord_update_message_response(&text);
        }
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decide approval: {e}"),
            );
        }
    };

    // Run the same side-effects as the dashboard / Telegram paths.
    let outcome =
        apply_callback_side_effects(handles, &parsed, &decided, &decided_by, "discord_callback")
            .await;

    // Best-effort message edit. Discord's UPDATE_MESSAGE response handles the
    // common case (the user clicked the button on the very message we want to
    // edit), so this PATCH is only needed if the interaction omitted a
    // `message` reference (rare). Keep the helper around for completeness.
    let edited_text =
        render_decided_message_text(parsed.parsed, &decided_by, outcome.dispatched_apply_update);
    let interaction_message_id: Option<i64> =
        interaction.message.as_ref().and_then(|m| m.id.parse().ok());
    let interaction_channel_id: Option<i64> = interaction
        .message
        .as_ref()
        .and_then(|m| m.channel_id.as_ref())
        .and_then(|s| s.parse::<i64>().ok());

    if interaction_message_id.is_none() {
        // Fall back to persisted Discord metadata on the row.
        let stored = decided.row.metadata_json.as_ref().and_then(|meta| {
            let cid = meta
                .get("notifier_discord_channel_id")
                .and_then(|v| v.as_i64());
            let mid = meta
                .get("notifier_discord_message_id")
                .and_then(|v| v.as_i64());
            match (cid, mid) {
                (Some(c), Some(m)) => Some((c, m)),
                _ => None,
            }
        });
        if let Some((channel_id, message_id)) = stored {
            if let Ok(token) = std::env::var(DISCORD_BOT_TOKEN_ENV) {
                if !token.is_empty() {
                    let api_base = std::env::var(DISCORD_API_BASE_ENV).ok();
                    if let Err(e) = edit_discord_message_text(
                        api_base.as_deref(),
                        &token,
                        channel_id,
                        message_id,
                        &edited_text,
                    )
                    .await
                    {
                        warn!(
                            action_id = %parsed.action_id,
                            error = %e,
                            "discord callback: edit_discord_message_text failed; decision applied"
                        );
                    }
                }
            }
        }
    } else {
        // We have a message ref in the interaction itself; rely on the
        // UPDATE_MESSAGE response to do the edit. Logging the ids here keeps
        // troubleshooting straightforward when something goes sideways.
        debug!(
            action_id = %parsed.action_id,
            channel_id = ?interaction_channel_id,
            message_id = ?interaction_message_id,
            "discord callback: returning UPDATE_MESSAGE for interaction"
        );
    }

    discord_update_message_response(&edited_text)
}

/// Carrier for the post-decide side-effects so all callback paths share one
/// implementation. Currently only Discord uses this struct; the Telegram path
/// keeps its inline copy because it predates the helper. The data shape is
/// stable enough that a future refactor could collapse them.
struct CallbackOutcome {
    dispatched_apply_update: bool,
}

/// Run the post-decide side-effects: queue the apply_update HostAction on
/// approve, write paused_until on snooze, and emit the lifecycle event on the
/// bus. Returns whether the apply_update was queued so callers can render a
/// "queued" suffix in the edited message.
async fn apply_callback_side_effects(
    handles: &Arc<ControllerHandles>,
    parsed: &ParsedCallbackData,
    decided: &DecidedApproval,
    decided_by: &str,
    source: &str,
) -> CallbackOutcome {
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
                source = %source,
                action_id = %parsed.action_id,
                error = %e,
                "callback: failed to queue apply_update HostAction"
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
        serde_json::Value::String(decided_by.to_string()),
    );
    metadata.insert(
        "source".to_string(),
        serde_json::Value::String(source.to_string()),
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

    CallbackOutcome {
        dispatched_apply_update: dispatched,
    }
}

fn resolve_discord_decided_by(i: &DiscordInteraction) -> String {
    if let Some(member) = i.member.as_ref() {
        if let Some(u) = member.user.as_ref() {
            if let Some(name) = u.username.as_deref() {
                return format!("discord:@{name}");
            }
            if let Some(name) = u.global_name.as_deref() {
                return format!("discord:{name}");
            }
        }
    }
    if let Some(u) = i.user.as_ref() {
        if let Some(name) = u.username.as_deref() {
            return format!("discord:@{name}");
        }
        if let Some(name) = u.global_name.as_deref() {
            return format!("discord:{name}");
        }
    }
    "discord".to_string()
}

fn discord_update_message_response(text: &str) -> Response {
    let body = DiscordUpdateMessage {
        kind: 7,
        data: DiscordUpdateMessageData {
            content: text,
            components: &[],
        },
    };
    Json(body).into_response()
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
