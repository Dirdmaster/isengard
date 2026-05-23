#![doc = include_str!("../docs/_crate.md")]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]
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
use crate::discord::{DiscordChannel, DiscordConfig, DiscordInteractive};
use crate::http::{HttpChannel, HttpConfig};
use crate::telegram::{TelegramChannel, TelegramConfig};

/// Event kind the interactive subscriber listens for.
///
/// Defined locally so this module can land independently of the
/// `isengard_core::event::kinds` constant. Migrate when the upstream
/// constant exists.
const UPDATE_PENDING_APPROVAL_KIND: &str = "update.pending_approval";

/// Env var carrying the shared secret Telegram returns in callback
/// query update bodies. Without it, button clicks have no way to
/// authenticate against the dashboard.
const TELEGRAM_WEBHOOK_SECRET_ENV: &str = "ISENGARD_TELEGRAM_WEBHOOK_SECRET";

/// Env var carrying the Discord bot account token used by the
/// interactive client.
const DISCORD_BOT_TOKEN_ENV: &str = "ISENGARD_DISCORD_BOT_TOKEN";

/// Env var carrying the Discord application public key used to verify
/// inbound interaction signatures.
const DISCORD_PUBLIC_KEY_ENV: &str = "ISENGARD_DISCORD_PUBLIC_KEY";

/// Stable plugin name surfaced to the controller and host registry.
const PLUGIN_NAME: &str = "notifier";

/// Parsed `[notifier]` config block.
///
/// Each channel sub-block is optional. Absent blocks leave the
/// corresponding channel unconfigured.
#[derive(Debug, Clone, Deserialize, Default)]
struct NotifierConfig {
    /// Optional Telegram channel config.
    #[serde(default)]
    telegram: Option<TelegramConfig>,
    /// Optional Discord channel config.
    #[serde(default)]
    discord: Option<DiscordConfig>,
    /// Optional generic HTTP channel config.
    #[serde(default)]
    http: Option<HttpConfig>,
}

/// Controller-side notifier plugin instance.
///
/// Holds the constructed channels, the spawned dispatch task handle,
/// and direct handles to the interactive Telegram and Discord clients
/// (when configured) so the `update.pending_approval` path can call
/// their typed methods.
pub struct Notifier {
    /// Configured channels, each wrapped in [`RateLimited`].
    channels: Vec<Box<dyn NotifyChannel>>,
    /// Union of every kind any channel cares about.
    ///
    /// The dispatch task always subscribes to every event on the bus
    /// and per-channel `matches_kind` does the actual filtering; this
    /// field is kept for diagnostic surfaces.
    kinds: Vec<String>,
    /// Direct handle to the Telegram channel (when configured).
    ///
    /// The interactive path calls
    /// [`TelegramChannel::send_inline_keyboard`] and
    /// [`TelegramChannel::edit_message_text`] which the generic
    /// `NotifyChannel` trait does not expose.
    telegram: Option<Arc<TelegramChannel>>,
    /// Direct handle to the Discord interactive client.
    ///
    /// Present when a Discord channel is configured AND
    /// `ISENGARD_DISCORD_BOT_TOKEN` is set. The dispatch task fans the
    /// approval event to this client alongside Telegram.
    discord_interactive: Option<Arc<DiscordInteractive>>,
    /// Join handle for the dispatch task spawned from [`Plugin::start`].
    task: Option<JoinHandle<()>>,
}

impl Notifier {
    /// Builds an empty notifier. Channels are wired up in
    /// [`Plugin::init`].
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            kinds: Vec::new(),
            telegram: None,
            discord_interactive: None,
            task: None,
        }
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps any displayable error into [`CoreError::InitFailed`] for the
/// notifier plugin.
fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Wraps any displayable error into [`CoreError::StartFailed`] for the
/// notifier plugin.
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

    /// Decodes config and constructs each configured channel.
    ///
    /// Telegram and Discord paths warn explicitly when only part of
    /// the interactive setup is wired: a missing webhook secret leaves
    /// Telegram one-way, a missing bot token leaves Discord one-way,
    /// a missing public key leaves Discord callbacks unauthenticated.
    /// Operators see the warnings in `isd ps` logs.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InitFailed`] when the config JSON is
    /// malformed or when any channel fails to construct (bad header
    /// name, missing required field, etc.).
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
            let interactive_channel_id = dc_cfg
                .channel_id
                .as_ref()
                .and_then(|s| s.parse::<i64>().ok());
            let interactive_api_base = dc_cfg.api_base.clone();
            let dc = DiscordChannel::from_config(dc_cfg)
                .map_err(|e| init_err(format!("discord channel: {e}")))?;
            self.channels.push(Box::new(RateLimited::new(dc, tpm)));

            match interactive_channel_id {
                Some(channel_id) => {
                    match DiscordInteractive::from_env(channel_id, interactive_api_base) {
                        Ok(Some(d)) => {
                            self.discord_interactive = Some(Arc::new(d));
                            if std::env::var(DISCORD_PUBLIC_KEY_ENV).is_err() {
                                warn!(
                                    env = DISCORD_PUBLIC_KEY_ENV,
                                    "set ISENGARD_DISCORD_PUBLIC_KEY to enable Discord approval \
                                     callbacks; outbound interactive messages will work but \
                                     button clicks will be rejected as unauthenticated"
                                );
                            }
                        }
                        Ok(None) => {
                            warn!(
                                env = DISCORD_BOT_TOKEN_ENV,
                                "set ISENGARD_DISCORD_BOT_TOKEN to enable Discord interactive \
                                 messages; current Discord channel will be one-way only"
                            );
                        }
                        Err(e) => {
                            return Err(init_err(format!("discord interactive: {e}")));
                        }
                    }
                }
                None => {
                    debug!(
                        "discord channel configured without channel_id; \
                         interactive approval messages disabled"
                    );
                }
            }
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

    /// Spawns the dispatch task that drains the controller event bus.
    ///
    /// The task owns the constructed channels (moved out of `self`)
    /// so [`Plugin::stop`] only needs to abort the join handle.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StartFailed`] when the plugin context lacks
    /// a downcast-compatible [`ControllerHandles`].
    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
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

        let channels = std::mem::take(&mut self.channels);
        let channels: Arc<[Box<dyn NotifyChannel>]> = channels.into();
        let telegram = self.telegram.clone();
        let discord_interactive = self.discord_interactive.clone();

        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if event.kind == UPDATE_PENDING_APPROVAL_KIND {
                            if let Some(tg) = telegram.as_ref() {
                                handle_pending_approval(tg, inventory.as_ref(), &event).await;
                            }
                            if let Some(dc) = discord_interactive.as_ref() {
                                handle_pending_approval_discord(dc, inventory.as_ref(), &event)
                                    .await;
                            }
                        }
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

/// Fans one event out to every channel that matches it.
///
/// Per-channel `send` errors log at warn and don't propagate: the
/// dispatch loop must keep draining the bus even if one channel
/// flaps.
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

/// Payload shape carried on `update.pending_approval` events in
/// `event.metadata`. Mirrors the JSON the updater plugin emits.
#[derive(Debug, Clone, Deserialize)]
struct PendingApprovalPayload {
    /// ULID identifying the pending approval row.
    action_id: String,
    /// Host the proposed change targets.
    #[serde(default)]
    host_id: Option<String>,
    /// Stack the proposed change targets.
    #[serde(default)]
    stack: Option<String>,
    /// Service the proposed change targets.
    #[serde(default)]
    service: Option<String>,
    /// Image reference (without digest).
    #[serde(default)]
    image: Option<String>,
    /// Currently running image digest.
    #[serde(default)]
    current_digest: Option<String>,
    /// Proposed image digest pulled from the registry.
    #[serde(default)]
    proposed_digest: Option<String>,
}

#[doc = include_str!("../docs/pending-approval.md")]
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

    let chat_id_i64 = match sent.chat_id.parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
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

/// Discord variant of the pending-approval handler.
///
/// Sends a plain-text message with an action row of buttons, then
/// records `(channel_id, message_id)` on the action row so the
/// dashboard callback can edit the message after a decision. Discord
/// and Telegram coexist; each writes its own metadata key pair so a
/// single approval row can carry both.
async fn handle_pending_approval_discord(
    dc: &Arc<DiscordInteractive>,
    inventory: &Inventory,
    event: &Event,
) {
    let payload: PendingApprovalPayload = match serde_json::from_value(event.metadata.clone()) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "pending_approval event has malformed metadata, dropping (discord)");
            return;
        }
    };

    let text = format_pending_approval_plain(&payload, event);
    let buttons = vec![
        InlineButton::new("Approve", format!("apv:{}:approve", payload.action_id)),
        InlineButton::new("Reject", format!("apv:{}:reject", payload.action_id)),
        InlineButton::new("Snooze 24h", format!("apv:{}:snooze:24", payload.action_id)),
    ];

    let sent = match dc.send_action_row(&text, &buttons).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                action_id = %payload.action_id,
                error = %e,
                "discord send_action_row failed; not retrying in v1"
            );
            return;
        }
    };

    if let Err(e) = inventory
        .set_discord_approval_message_metadata(&payload.action_id, sent.channel_id, sent.message_id)
        .await
    {
        warn!(
            action_id = %payload.action_id,
            error = %e,
            "failed to persist discord approval message metadata; callback edits will fall back \
             to the in-interaction message reference"
        );
    } else {
        debug!(
            action_id = %payload.action_id,
            channel_id = sent.channel_id,
            message_id = sent.message_id,
            "persisted discord approval message metadata"
        );
    }
}

/// Plain-text body for a pending-approval Discord message.
///
/// Discord renders a subset of markdown but the formatting budget is
/// better spent on the action row buttons, so this stays plain.
fn format_pending_approval_plain(p: &PendingApprovalPayload, event: &Event) -> String {
    let mut lines = vec!["[isengard] update awaiting approval".to_string()];

    let location = match (&p.host_id, &p.stack, &p.service) {
        (Some(h), Some(s), Some(svc)) => Some(format!("{h} . {s} . {svc}")),
        (None, Some(s), Some(svc)) => Some(format!("{s} . {svc}")),
        (Some(h), Some(s), None) => Some(format!("{h} . {s}")),
        (Some(h), None, None) => Some(h.clone()),
        _ => None,
    };
    if let Some(loc) = location {
        lines.push(format!("where: {loc}"));
    }

    if let Some(img) = &p.image {
        let digest_line = match (&p.current_digest, &p.proposed_digest) {
            (Some(c), Some(n)) => {
                format!("image: {} {} -> {}", img, short_digest(c), short_digest(n))
            }
            (None, Some(n)) => format!("image: {} {}", img, short_digest(n)),
            _ => format!("image: {img}"),
        };
        lines.push(digest_line);
    }

    if !event.summary.is_empty() {
        lines.push(event.summary.clone());
    }

    lines.push("Pick an action below.".to_string());
    lines.join("\n")
}

/// HTML body for a pending-approval Telegram message.
///
/// Mirrors the prose in the spec: a `host . stack . service` location
/// line, a `image: current -> proposed` digest line, the event's own
/// summary, and a closing prompt.
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

/// Truncates a sha256 digest to the first 12 hex chars after the
/// prefix.
///
/// Keeps approval messages compact. Pass-through (with a 20-char cap)
/// when the input doesn't look like a digest.
fn short_digest(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("sha256:") {
        let take: String = rest.chars().take(12).collect();
        format!("sha256:{take}")
    } else {
        s.chars().take(20).collect()
    }
}

/// Minimal HTML escape for the four chars Telegram cares about in
/// `parse_mode=HTML`.
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
    /// Returns an empty slice. The dispatch task subscribes to every
    /// event on the bus and per-channel filtering happens inside
    /// `dispatch`; the `EventSubscriber` shape is implemented for
    /// trait conformance only.
    fn event_kinds(&self) -> &[&'static str] {
        &[]
    }

    /// Always returns `Ok(())`. The plugin's actual handling lives in
    /// the dispatch task spawned from [`Plugin::start`].
    async fn handle(&self, _event: &Event, _ctx: &PluginContext) -> Result<()> {
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
