//! Isengard `notifier` plugin (controller-side).
//!
//! Subscribes to the controller's EventBus, filters events by kind, and
//! dispatches matches to configured channels (v1: Telegram only).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use async_trait::async_trait;
use isengard_controller::ControllerHandles;
use isengard_core::{
    Capability, CoreError, Event, EventSubscriber, Plugin, PluginContext, PluginRegistration,
    Result,
};
use isengard_storage::Inventory;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

pub mod channel;
pub mod discord;
pub mod http;
pub mod telegram;

use crate::channel::{InlineButton, NotifyChannel, RateLimited};
use crate::discord::{DiscordChannel, DiscordConfig};
use crate::http::{HttpChannel, HttpConfig};
use crate::telegram::{TelegramChannel, TelegramConfig};

/// Event kind the interactive subscriber listens for.
///
/// TODO(phase 9b T2): replace with `isengard_core::event::kinds::UPDATE_PENDING_APPROVAL`
/// once T2 lands the constant. Defined locally for now so T6 can land in
/// parallel without blocking on the event-kinds add.
const UPDATE_PENDING_APPROVAL_KIND: &str = "update.pending_approval";

const TELEGRAM_WEBHOOK_SECRET_ENV: &str = "ISENGARD_TELEGRAM_WEBHOOK_SECRET";

const PLUGIN_NAME: &str = "notifier";

#[derive(Debug, Clone, Deserialize, Default)]
struct NotifierConfig {
    #[serde(default)]
    telegram: Option<TelegramConfig>,
    #[serde(default)]
    discord: Option<DiscordConfig>,
    #[serde(default)]
    http: Option<HttpConfig>,
}

pub struct Notifier {
    channels: Vec<Box<dyn NotifyChannel>>,
    /// Union of all kinds across channels (for the EventSubscriber trait).
    /// Stored as String here, leaked to &'static str in event_kinds() see note.
    kinds: Vec<String>,
    /// Direct handle to the Telegram channel (if configured) for the
    /// interactive `update.pending_approval` subscriber, which calls typed
    /// methods (`send_inline_keyboard`, `edit_message_text`) that the generic
    /// `NotifyChannel` trait does not expose.
    telegram: Option<Arc<TelegramChannel>>,
    task: Option<JoinHandle<()>>,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            kinds: Vec::new(),
            telegram: None,
            task: None,
        }
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

#[async_trait]
impl Plugin for Notifier {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: NotifierConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing notifier config: {e}")))?;

        if let Some(tg_cfg) = cfg.telegram {
            let tpm = tg_cfg.tokens_per_minute.unwrap_or(10.0);
            let tg = TelegramChannel::from_config(tg_cfg)
                .map_err(|e| init_err(format!("telegram channel: {e}")))?;
            let tg_arc: Arc<TelegramChannel> = Arc::new(tg);
            self.channels
                .push(Box::new(RateLimited::new(tg_arc.clone(), tpm)));
            self.telegram = Some(tg_arc);

            if std::env::var(TELEGRAM_WEBHOOK_SECRET_ENV).is_err() {
                warn!(
                    env = TELEGRAM_WEBHOOK_SECRET_ENV,
                    "set ISENGARD_TELEGRAM_WEBHOOK_SECRET to enable approval callbacks; \
                     current Telegram messages will be one-way only"
                );
            }
        }

        if let Some(dc_cfg) = cfg.discord {
            let tpm = dc_cfg.tokens_per_minute.unwrap_or(10.0);
            let dc = DiscordChannel::from_config(dc_cfg)
                .map_err(|e| init_err(format!("discord channel: {e}")))?;
            self.channels.push(Box::new(RateLimited::new(dc, tpm)));
        }

        if let Some(http_cfg) = cfg.http {
            let tpm = http_cfg.tokens_per_minute.unwrap_or(10.0);
            let hc = HttpChannel::from_config(http_cfg)
                .map_err(|e| init_err(format!("http channel: {e}")))?;
            self.channels.push(Box::new(RateLimited::new(hc, tpm)));
        }

        self.kinds = vec![
            "update.success".into(),
            "update.failed".into(),
            "update.checked".into(),
            "agent.disconnect_long".into(),
            UPDATE_PENDING_APPROVAL_KIND.into(),
        ];

        info!(channels = self.channels.len(), "notifier initialised");
        Ok(())
    }

    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
        // Downcast the bus handle to the controller handles bundle.
        let handles = ctx
            .bus
            .clone()
            .ok_or_else(|| {
                start_err("notifier started without ControllerHandles on PluginContext")
            })?
            .downcast::<ControllerHandles>()
            .map_err(|_| start_err("bus on PluginContext was not ControllerHandles"))?;
        let mut rx = handles.bus.subscribe();
        let inventory = handles.inventory.clone();

        // Move channels out so the spawned task owns them (Plugin::stop
        // doesn't need them anymore).
        let channels = std::mem::take(&mut self.channels);
        let channels: Arc<[Box<dyn NotifyChannel>]> = channels.into();
        let telegram = self.telegram.clone();

        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // Interactive path: pending_approval drives an inline
                        // keyboard via Telegram and persists message metadata
                        // so the callback handler can edit on decide.
                        if event.kind == UPDATE_PENDING_APPROVAL_KIND {
                            if let Some(tg) = telegram.as_ref() {
                                handle_pending_approval(tg, inventory.as_ref(), &event).await;
                            }
                        }
                        // One-way fan-out path: every channel that opted in
                        // by `kind` gets the event (skips pending_approval
                        // since no channel registers that kind in v1; the
                        // interactive Telegram message is the notification).
                        dispatch(&channels, event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "notifier broadcast lag, events dropped");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!("notifier broadcast closed; ending task");
                        break;
                    }
                }
            }
        });
        self.task = Some(task);
        info!("notifier started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        info!("notifier stopped");
        Ok(())
    }
}

async fn dispatch(channels: &[Box<dyn NotifyChannel>], event: Event) {
    for ch in channels.iter() {
        if !ch.matches_kind(&event.kind) {
            continue;
        }
        if let Err(e) = ch.send(&event).await {
            warn!(channel = ch.name(), kind = %event.kind, error = %e, "channel send failed");
        }
    }
}

/// Payload carried on `update.pending_approval` events. Pulled from
/// `event.metadata`. Matches the shape T2 emits.
#[derive(Debug, Clone, Deserialize)]
struct PendingApprovalPayload {
    action_id: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    current_digest: Option<String>,
    #[serde(default)]
    proposed_digest: Option<String>,
}

/// Render the inline-keyboard Telegram message for a pending approval and
/// persist (chat_id, message_id) on the action's metadata so the callback
/// handler in 9f can edit the same message after a decision.
///
/// v1 retry semantics: on send failure we log and bail. There is no
/// in-process retry. The next updater cycle's idempotence check
/// (`find_open_approval_for_proposed_digest`) prevents duplicate rows; if a
/// message_id never gets persisted, the action stays open and the operator
/// must decide via the dashboard. Documented gap; revisit in v1.x with a
/// dedicated outbox if it bites.
async fn handle_pending_approval(tg: &Arc<TelegramChannel>, inventory: &Inventory, event: &Event) {
    let payload: PendingApprovalPayload = match serde_json::from_value(event.metadata.clone()) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "pending_approval event has malformed metadata, dropping");
            return;
        }
    };

    let text = format_pending_approval_html(&payload, event);

    let buttons = vec![
        vec![
            InlineButton::new("Approve", format!("apv:{}:approve", payload.action_id)),
            InlineButton::new("Reject", format!("apv:{}:reject", payload.action_id)),
        ],
        vec![InlineButton::new(
            "Snooze 24h",
            format!("apv:{}:snooze:24", payload.action_id),
        )],
    ];

    // Send to the first configured chat_id. v1 design: one Telegram chat per
    // notifier instance for approvals. Fan-out across multiple chats with a
    // single decidable message would require richer message tracking.
    let Some(chat_id) = tg.chat_ids().first() else {
        warn!("telegram channel has no chat_ids; cannot send approval");
        return;
    };

    let sent = match tg.send_inline_keyboard(chat_id, &text, buttons).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                action_id = %payload.action_id,
                error = %e,
                "telegram send_inline_keyboard failed; not retrying in v1"
            );
            return;
        }
    };

    // Best-effort parse of chat_id to i64 for storage. Telegram chat ids are
    // numeric (negative for groups), but the config carries them as strings.
    let chat_id_i64 = match sent.chat_id.parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
            // Channel-style ids like "@somechannel" don't fit i64; storage
            // schema uses i64. Fall back to 0 with a warn so the action_id
            // still has SOME message metadata; callbacks won't work but
            // logs make the misconfiguration obvious.
            warn!(
                chat_id = %sent.chat_id,
                "non-numeric Telegram chat_id; storing 0; Telegram callbacks will not match"
            );
            0
        }
    };

    if let Err(e) = inventory
        .set_approval_message_metadata(&payload.action_id, chat_id_i64, sent.message_id)
        .await
    {
        warn!(
            action_id = %payload.action_id,
            error = %e,
            "failed to persist approval message metadata; callback edits will fail"
        );
    } else {
        debug!(
            action_id = %payload.action_id,
            chat_id = %sent.chat_id,
            message_id = sent.message_id,
            "persisted approval message metadata"
        );
    }
}

/// HTML-formatted summary for a pending-approval Telegram message. Mirrors
/// the prose in the spec (host . stack . service, image:current -> proposed).
fn format_pending_approval_html(p: &PendingApprovalPayload, event: &Event) -> String {
    let mut lines = vec!["<b>[isengard] update awaiting approval</b>".to_string()];

    let location = match (&p.host_id, &p.stack, &p.service) {
        (Some(h), Some(s), Some(svc)) => Some(format!("{h} . {s} . {svc}")),
        (None, Some(s), Some(svc)) => Some(format!("{s} . {svc}")),
        (Some(h), Some(s), None) => Some(format!("{h} . {s}")),
        (Some(h), None, None) => Some(h.clone()),
        _ => None,
    };
    if let Some(loc) = location {
        lines.push(format!("<b>where:</b> {}", html_escape(&loc)));
    }

    if let Some(img) = &p.image {
        let digest_line = match (&p.current_digest, &p.proposed_digest) {
            (Some(c), Some(n)) => format!(
                "<b>image:</b> {} <code>{}</code> -&gt; <code>{}</code>",
                html_escape(img),
                html_escape(&short_digest(c)),
                html_escape(&short_digest(n))
            ),
            (None, Some(n)) => format!(
                "<b>image:</b> {} <code>{}</code>",
                html_escape(img),
                html_escape(&short_digest(n))
            ),
            _ => format!("<b>image:</b> {}", html_escape(img)),
        };
        lines.push(digest_line);
    }

    if !event.summary.is_empty() {
        lines.push(html_escape(&event.summary));
    }

    lines.push("<i>Pick an action below.</i>".to_string());
    lines.join("\n")
}

/// Truncate a sha256 digest to its first 12 hex chars after the prefix, so
/// the message stays compact. Pass-through if the input doesn't look like a
/// digest.
fn short_digest(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("sha256:") {
        let take: String = rest.chars().take(12).collect();
        format!("sha256:{take}")
    } else {
        s.chars().take(20).collect()
    }
}

/// Minimal HTML escape for the four chars Telegram cares about in `parse_mode=HTML`.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

#[async_trait]
impl EventSubscriber for Notifier {
    fn event_kinds(&self) -> &[&'static str] {
        // The trait wants &'static str slice; we can't return runtime kinds
        // here without leaking. For v1, return an empty slice — the actual
        // filter happens per-channel in dispatch(). The host's role is just
        // to subscribe; we always-want-everything from the bus.
        &[]
    }

    async fn handle(&self, _event: &Event, _ctx: &PluginContext) -> Result<()> {
        // Unused — start() spawns its own dispatch task. EventSubscriber
        // is implemented for shape-compliance with the Phase 1 trait, but
        // the actual handling is the spawned task in start().
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration {
        name: PLUGIN_NAME,
        capabilities: &[Capability::Controller],
        constructor: || Box::new(Notifier::new()) as Box<dyn Plugin>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifier_new_has_no_channels() {
        let n = Notifier::new();
        assert!(n.channels.is_empty());
    }
}
