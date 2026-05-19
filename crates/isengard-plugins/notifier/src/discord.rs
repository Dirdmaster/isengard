//! Discord channel.
//!
//! v1 had a single one-way path: post to an incoming webhook URL. V2
//! adds an optional interactive path that uses the Discord Bot API to send
//! messages with action-row buttons and edit them after a decision.
//!
//! Two auth models on purpose:
//! - One-way fan-out (`update.success`, `update.failed`, ...): keep using the
//!   existing webhook URL. Webhooks are simple, no bot account required.
//! - Interactive callbacks (`update.pending_approval`): require a registered
//!   Discord application with a bot token (`ISENGARD_DISCORD_BOT_TOKEN`) and
//!   a channel id. Webhook URLs cannot edit messages by id without re-using
//!   the webhook URL itself, and the inbound interaction signature requires
//!   the application's public key (`ISENGARD_DISCORD_PUBLIC_KEY`).

use async_trait::async_trait;
use ed25519_dalek::{Signature, VerifyingKey};
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::{InlineButton, NotifyChannel, format_event};

const DISCORD_CONTENT_MAX: usize = 2000;
const TRUNCATED_SUFFIX: &str = "\n... [truncated]";
const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DiscordConfig {
    pub webhook_url: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Channel id (snowflake) used by the interactive path. Required for
    /// approval messages; ignored by the one-way webhook path.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Override base URL (used by tests; defaults to the public Discord API).
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

pub struct DiscordChannel {
    webhook_url: String,
    kinds: Vec<String>,
    http: Client,
}

#[derive(Debug, Serialize)]
struct DiscordBody<'a> {
    content: &'a str,
}

impl DiscordChannel {
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

/// Result of a successful interactive send. Carries the Discord-assigned
/// `message_id` (snowflake i64) plus the channel id so the dashboard callback
/// can edit the message after a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordSentMessage {
    pub channel_id: i64,
    pub message_id: i64,
}

pub struct DiscordInteractive {
    bot_token: String,
    channel_id: i64,
    api_base: String,
    http: Client,
}

#[derive(Debug, Serialize)]
struct ActionRowMessage<'a> {
    content: &'a str,
    components: Vec<ActionRow<'a>>,
}

#[derive(Debug, Serialize)]
struct ActionRow<'a> {
    /// Component type 1 = Action Row.
    #[serde(rename = "type")]
    kind: u8,
    components: Vec<ButtonComponent<'a>>,
}

#[derive(Debug, Serialize)]
struct ButtonComponent<'a> {
    /// Component type 2 = Button.
    #[serde(rename = "type")]
    kind: u8,
    /// Style: 1=Primary, 2=Secondary, 3=Success, 4=Danger, 5=Link.
    style: u8,
    label: &'a str,
    custom_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct DiscordCreatedMessage {
    /// Snowflake comes back as a JSON string; parse to i64.
    id: String,
    channel_id: String,
}

#[derive(Debug, Serialize)]
struct EditBody<'a> {
    content: &'a str,
    components: Vec<ActionRow<'a>>,
}

impl DiscordInteractive {
    /// Build from the configured channel id + an env-sourced bot token. Returns
    /// `None` if `ISENGARD_DISCORD_BOT_TOKEN` is unset, so callers can opt out
    /// silently without crashing the plugin.
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

    pub fn channel_id(&self) -> i64 {
        self.channel_id
    }

    /// Send a message with one action row of buttons. Returns the
    /// channel_id + message_id pair so callers can persist them and edit
    /// the message after a decision.
    ///
    /// Discord caps `custom_id` at 100 bytes (each); not enforced here.
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

/// Map a known callback_data prefix to a button style. Approve = success
/// (green), Reject = danger (red), everything else (snooze) = secondary.
fn button_style_for(callback_data: &str) -> u8 {
    if callback_data.ends_with(":approve") {
        3
    } else if callback_data.ends_with(":reject") {
        4
    } else {
        2
    }
}

/// Stateless helper used by the dashboard callback handler: edit a previously
/// sent Discord message in place and clear the action row so the buttons
/// disappear. Mirrors `edit_telegram_message_text` in shape.
///
/// `api_base` defaults to the public Discord API when `None`. Tests override
/// to point at a `wiremock::MockServer`.
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

/// Verify a Discord interaction request. Discord signs `timestamp || raw_body`
/// with the application's ed25519 private key. The public key is published in
/// the developer portal and supplied via `ISENGARD_DISCORD_PUBLIC_KEY`.
///
/// `public_key_hex` and `signature_hex` are hex-encoded; `timestamp` and
/// `body` are the raw bytes from the request. Errors if any decoding step
/// fails or the signature does not verify.
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
