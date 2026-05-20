//! Discord channel.
//!
//! v1 had one path: post to an incoming webhook URL. v2 adds an
//! interactive path that uses the Discord Bot API to send messages
//! with action-row buttons and edit them after a decision.
//!
//! # Two auth models
//!
//! - **One-way fan-out** (`update.success`, `update.failed`, ...) reuses
//!   the existing webhook URL. Webhooks are simple and need no bot
//!   account.
//! - **Interactive callbacks** (`update.pending_approval`) require a
//!   registered Discord application with a bot token
//!   (`ISENGARD_DISCORD_BOT_TOKEN`) and a channel id. Webhook URLs
//!   can't edit messages by id without reusing the webhook URL, and
//!   inbound interactions need the application's public key
//!   (`ISENGARD_DISCORD_PUBLIC_KEY`) for signature verification.

use async_trait::async_trait;
use ed25519_dalek::{Signature, VerifyingKey};
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::{InlineButton, NotifyChannel, format_event};

/// Discord's 2000-character `content` cap.
const DISCORD_CONTENT_MAX: usize = 2000;

/// Suffix appended when a message body has to be truncated.
const TRUNCATED_SUFFIX: &str = "\n... [truncated]";

/// Public Discord API base URL.
const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";

/// Parsed `[notifier.discord]` config block.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DiscordConfig {
    /// Incoming webhook URL for the one-way fan-out path. Required.
    pub webhook_url: String,
    /// Exact-match event kinds this channel wants.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Channel id (snowflake) for the interactive path.
    ///
    /// Required for approval messages; ignored by the webhook path.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Override base URL. Used by tests to point at `wiremock`.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Rate limit refill rate in tokens per minute.
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

/// One-way Discord channel that POSTs to an incoming webhook URL.
pub struct DiscordChannel {
    /// Webhook URL for outbound messages.
    webhook_url: String,
    /// Event kinds this channel matches.
    kinds: Vec<String>,
    /// HTTP client used for the POST.
    http: Client,
}

/// Body for a webhook POST.
#[derive(Debug, Serialize)]
struct DiscordBody<'a> {
    /// Message content. Capped at [`DISCORD_CONTENT_MAX`].
    content: &'a str,
}

impl DiscordChannel {
    /// Builds a webhook channel from config.
    ///
    /// # Errors
    ///
    /// Returns an error when `webhook_url` is empty or when the HTTP
    /// client builder fails.
    pub fn from_config(cfg: DiscordConfig) -> anyhow::Result<Self> {
        if cfg.webhook_url.is_empty() {
            return Err(anyhow::anyhow!("discord channel: webhook_url required"));
        }
        Ok(Self {
            webhook_url: cfg.webhook_url,
            kinds: cfg.kinds,
            http: Client::builder()
                .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| anyhow::anyhow!("building http client: {e}"))?,
        })
    }
}

/// Truncates a message body to fit Discord's 2000-character cap.
///
/// Pass-through when the body already fits. Otherwise keeps
/// `DISCORD_CONTENT_MAX - TRUNCATED_SUFFIX.len()` chars and appends
/// `\n... [truncated]`.
fn truncate_for_discord(s: String) -> String {
    if s.len() <= DISCORD_CONTENT_MAX {
        return s;
    }
    let cap = DISCORD_CONTENT_MAX - TRUNCATED_SUFFIX.len();
    let mut t: String = s.chars().take(cap).collect();
    t.push_str(TRUNCATED_SUFFIX);
    t
}

#[async_trait]
impl NotifyChannel for DiscordChannel {
    fn name(&self) -> &'static str {
        "discord"
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        let content = truncate_for_discord(format_event(event));
        let body = DiscordBody { content: &content };
        match self.http.post(&self.webhook_url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                warn!(status = %resp.status(), "discord webhook non-success");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "discord webhook failed");
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Interactive
// ---------------------------------------------------------------------------

/// Result of a successful interactive Discord send.
///
/// Carries the Discord-assigned `message_id` (snowflake, decoded to
/// `i64`) plus the `channel_id` so the dashboard callback can edit
/// the message after a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordSentMessage {
    /// Channel the message landed in.
    pub channel_id: i64,
    /// Discord-assigned message id.
    pub message_id: i64,
}

/// Discord interactive client.
///
/// Uses the Bot API (`Authorization: Bot <token>`) rather than the
/// webhook URL. Required for sending and editing messages by id.
pub struct DiscordInteractive {
    /// Bot account token.
    bot_token: String,
    /// Channel snowflake to post into.
    channel_id: i64,
    /// Resolved API base URL.
    api_base: String,
    /// HTTP client used for every Discord call.
    http: Client,
}

/// Message body for an interactive send.
#[derive(Debug, Serialize)]
struct ActionRowMessage<'a> {
    /// Plain-text content.
    content: &'a str,
    /// Action row components attached to the message.
    components: Vec<ActionRow<'a>>,
}

/// One Discord action-row component.
#[derive(Debug, Serialize)]
struct ActionRow<'a> {
    /// Component type. `1` = Action Row.
    #[serde(rename = "type")]
    kind: u8,
    /// Buttons inside the action row.
    components: Vec<ButtonComponent<'a>>,
}

/// One Discord button component.
#[derive(Debug, Serialize)]
struct ButtonComponent<'a> {
    /// Component type. `2` = Button.
    #[serde(rename = "type")]
    kind: u8,
    /// Visual style. `1`=Primary, `2`=Secondary, `3`=Success,
    /// `4`=Danger, `5`=Link.
    style: u8,
    /// User-visible label.
    label: &'a str,
    /// Opaque payload echoed back in the interaction.
    custom_id: &'a str,
}

/// Decoded subset of Discord's `Create Message` response.
#[derive(Debug, Deserialize)]
struct DiscordCreatedMessage {
    /// Snowflake message id as a JSON string (Discord's wire format).
    id: String,
    /// Snowflake channel id as a JSON string.
    channel_id: String,
}

/// Body for a `PATCH /channels/.../messages/...` edit.
#[derive(Debug, Serialize)]
struct EditBody<'a> {
    /// New content.
    content: &'a str,
    /// New component layout. Empty vector clears the action row.
    components: Vec<ActionRow<'a>>,
}

impl DiscordInteractive {
    /// Builds the interactive client from a channel id plus an
    /// env-sourced bot token.
    ///
    /// Returns `Ok(None)` when `ISENGARD_DISCORD_BOT_TOKEN` is unset
    /// or empty, so callers can opt out silently without crashing the
    /// plugin.
    ///
    /// # Errors
    ///
    /// Returns an error only when the HTTP client builder fails.
    pub fn from_env(channel_id: i64, api_base: Option<String>) -> anyhow::Result<Option<Self>> {
        let token = match std::env::var("ISENGARD_DISCORD_BOT_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => return Ok(None),
        };
        let http = Client::builder()
            .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| anyhow::anyhow!("building http client: {e}"))?;
        Ok(Some(Self {
            bot_token: token,
            channel_id,
            api_base: api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            http,
        }))
    }

    /// Configured channel id. Used by callers that need to record the
    /// destination alongside an outgoing approval row.
    pub fn channel_id(&self) -> i64 {
        self.channel_id
    }

    /// Sends a plain-text message with one action row of buttons.
    ///
    /// Returns `(channel_id, message_id)` so callers can persist them
    /// and edit the message after a decision. Discord caps each
    /// `custom_id` at 100 bytes; this method does not enforce that.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails, when Discord
    /// returns a non-2xx status, when the response fails to decode,
    /// or when either snowflake doesn't fit in `i64`.
    pub async fn send_action_row(
        &self,
        text: &str,
        buttons: &[InlineButton],
    ) -> anyhow::Result<DiscordSentMessage> {
        let row = ActionRow {
            kind: 1,
            components: buttons
                .iter()
                .map(|b| ButtonComponent {
                    kind: 2,
                    style: button_style_for(&b.callback_data),
                    label: &b.text,
                    custom_id: &b.callback_data,
                })
                .collect(),
        };
        let body = ActionRowMessage {
            content: text,
            components: vec![row],
        };
        let url = format!(
            "{}/channels/{}/messages",
            self.api_base.trim_end_matches('/'),
            self.channel_id
        );
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("discord create message transport: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "discord create message non-success: status={status} body={body}"
            ));
        }
        let parsed: DiscordCreatedMessage = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("discord create message decode: {e}"))?;
        let message_id = parsed
            .id
            .parse::<i64>()
            .map_err(|e| anyhow::anyhow!("discord message id not i64: {e}"))?;
        let channel_id = parsed
            .channel_id
            .parse::<i64>()
            .map_err(|e| anyhow::anyhow!("discord channel id not i64: {e}"))?;
        Ok(DiscordSentMessage {
            channel_id,
            message_id,
        })
    }
}

/// Maps a callback-data suffix to a Discord button style.
///
/// `:approve` -> success (green, style 3). `:reject` -> danger
/// (red, style 4). Everything else -> secondary (grey, style 2).
fn button_style_for(callback_data: &str) -> u8 {
    if callback_data.ends_with(":approve") {
        3
    } else if callback_data.ends_with(":reject") {
        4
    } else {
        2
    }
}

/// Stateless helper that edits a Discord message and clears the
/// action row.
///
/// Used by the dashboard callback handler, which has only
/// `(bot_token, channel_id, message_id)` and no full
/// [`DiscordInteractive`] instance. Mirrors
/// [`crate::telegram::edit_telegram_message_text`].
///
/// `api_base` defaults to [`DEFAULT_API_BASE`] when `None`. Tests
/// override it to a `wiremock::MockServer`.
///
/// # Errors
///
/// Returns an error when the HTTP client builder fails, when the
/// transport fails, or when Discord returns a non-2xx status.
pub async fn edit_discord_message_text(
    api_base: Option<&str>,
    bot_token: &str,
    channel_id: i64,
    message_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    let base = api_base.unwrap_or(DEFAULT_API_BASE);
    let url = format!(
        "{}/channels/{}/messages/{}",
        base.trim_end_matches('/'),
        channel_id,
        message_id
    );
    let body = EditBody {
        content: text,
        components: Vec::new(),
    };
    let http = Client::builder()
        .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| anyhow::anyhow!("building http client: {e}"))?;
    let resp = http
        .patch(&url)
        .header("Authorization", format!("Bot {bot_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("discord edit message transport: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "discord edit message non-success: status={status} body={body}"
        ));
    }
    Ok(())
}

/// Verifies a Discord interaction request signature.
///
/// Discord signs `timestamp || raw_body` with the application's
/// ed25519 private key. The public key is published in the developer
/// portal and supplied via `ISENGARD_DISCORD_PUBLIC_KEY`.
///
/// `public_key_hex` and `signature_hex` arrive hex-encoded;
/// `timestamp` and `body` are the raw bytes from the request.
///
/// # Errors
///
/// Returns an error when any hex decode fails, when the public key
/// isn't 32 bytes, when the signature isn't 64 bytes, when the public
/// key is malformed, or when the signature does not verify against
/// the body.
pub fn verify_discord_signature(
    public_key_hex: &str,
    timestamp: &[u8],
    body: &[u8],
    signature_hex: &str,
) -> anyhow::Result<()> {
    let key_bytes = hex::decode(public_key_hex)
        .map_err(|e| anyhow::anyhow!("discord public key not hex: {e}"))?;
    let key_arr: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("discord public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_arr)
        .map_err(|e| anyhow::anyhow!("discord public key invalid: {e}"))?;
    let sig_bytes = hex::decode(signature_hex)
        .map_err(|e| anyhow::anyhow!("discord signature not hex: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("discord signature must be 64 bytes"))?;
    let signature = Signature::from_bytes(&sig_arr);
    let mut signed = Vec::with_capacity(timestamp.len() + body.len());
    signed.extend_from_slice(timestamp);
    signed.extend_from_slice(body);
    verifying_key
        .verify_strict(&signed, &signature)
        .map_err(|e| anyhow::anyhow!("discord signature verify failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_errors_on_empty_url() {
        let r = DiscordChannel::from_config(DiscordConfig::default());
        assert!(r.is_err());
    }

    #[test]
    fn matches_kind_exact() {
        let cfg = DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/1/x".into(),
            kinds: vec!["update.success".into()],
            channel_id: None,
            api_base: None,
            tokens_per_minute: None,
        };
        let ch = DiscordChannel::from_config(cfg).unwrap();
        assert!(ch.matches_kind("update.success"));
        assert!(!ch.matches_kind("update.failed"));
    }

    #[test]
    fn truncate_keeps_short_strings() {
        let s = "hello".to_string();
        let t = truncate_for_discord(s.clone());
        assert_eq!(t, s);
    }

    #[test]
    fn truncate_caps_long_strings_and_appends_marker() {
        let s = "x".repeat(3000);
        let t = truncate_for_discord(s);
        assert!(t.len() <= DISCORD_CONTENT_MAX);
        assert!(t.ends_with(TRUNCATED_SUFFIX));
    }

    #[test]
    fn button_style_routing() {
        assert_eq!(button_style_for("apv:01ABC:approve"), 3);
        assert_eq!(button_style_for("apv:01ABC:reject"), 4);
        assert_eq!(button_style_for("apv:01ABC:snooze:24"), 2);
        assert_eq!(button_style_for("anything-else"), 2);
    }
}
