//! Telegram bot channel.
//!
//! v1 fan-out is one-way: POST to `/sendMessage`. The interactive
//! path adds inline-keyboard sends and message edits for the
//! approval flow.
//!
//! # Setup
//!
//! Operator creates a bot via `@BotFather`, gets a token, adds the bot
//! to a chat, sends `/start`, reads the chat id from
//! `https://api.telegram.org/bot<TOKEN>/getUpdates`, then configures:
//!
//! ```json
//! { "telegram": { "chat_ids": ["123456"], "kinds": ["update.success", "update.failed"] } }
//! ```
//!
//! The bot token comes from the env var `ISENGARD_TELEGRAM_BOT_TOKEN`
//! (preferred) or the config field `bot_token` (fallback). Env wins.

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::{InlineButton, NotifyChannel, format_event};

/// Public Telegram Bot API base URL.
const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Parsed `[notifier.telegram]` config block.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelegramConfig {
    /// Bot token. Optional: env `ISENGARD_TELEGRAM_BOT_TOKEN` wins.
    #[serde(default)]
    pub bot_token: Option<String>,
    /// One or more chat ids to post to. Required.
    #[serde(default)]
    pub chat_ids: Vec<String>,
    /// Exact-match event kinds this channel wants.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Override base URL. Used by tests to point at `wiremock`.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Rate limit refill rate in tokens per minute.
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

/// Live Telegram channel.
///
/// Holds the resolved bot token, the configured chat ids, and a
/// reqwest client tagged with the notifier user agent. Cheap to
/// clone via `Arc`.
pub struct TelegramChannel {
    /// Resolved bot token.
    bot_token: String,
    /// Destination chat ids.
    chat_ids: Vec<String>,
    /// Event kinds this channel matches.
    kinds: Vec<String>,
    /// Resolved API base URL.
    api_base: String,
    /// HTTP client used for every Telegram call.
    http: Client,
}

/// Body for the fan-out `sendMessage` POST.
#[derive(Debug, Serialize)]
struct SendMessageBody<'a> {
    /// Destination chat id.
    chat_id: &'a str,
    /// Plain text message body.
    text: &'a str,
}

/// Result of a successful interactive send.
///
/// Carries the Telegram-assigned `message_id` so callers can edit the
/// message (after a decision) or persist it next to the originating
/// row (e.g. a pending-approval action).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramSentMessage {
    /// Destination chat id, echoed from the request.
    pub chat_id: String,
    /// Telegram-assigned message id.
    pub message_id: i64,
}

/// Telegram inline-keyboard wire format.
///
/// `inline_keyboard` is a 2-D array: rows of buttons.
#[derive(Debug, Serialize)]
struct InlineKeyboardMarkup<'a> {
    /// Rows of buttons.
    inline_keyboard: Vec<Vec<TgInlineButton<'a>>>,
}

/// One button on a Telegram inline keyboard.
#[derive(Debug, Serialize)]
struct TgInlineButton<'a> {
    /// User-visible button label.
    text: &'a str,
    /// Opaque payload echoed back in the callback query.
    callback_data: &'a str,
}

/// Body for the interactive `sendMessage` POST.
#[derive(Debug, Serialize)]
struct SendInteractiveBody<'a> {
    /// Destination chat id.
    chat_id: &'a str,
    /// Message body. HTML-escaped at the call site.
    text: &'a str,
    /// Telegram parse mode. Always `HTML` in this crate.
    parse_mode: &'static str,
    /// Inline keyboard attached to the message.
    reply_markup: InlineKeyboardMarkup<'a>,
}

/// Body for `editMessageText`.
#[derive(Debug, Serialize)]
struct EditMessageBody<'a> {
    /// Chat the message lives in.
    chat_id: &'a str,
    /// Message id to edit.
    message_id: i64,
    /// New body text.
    text: &'a str,
    /// Telegram parse mode. Always `HTML`.
    parse_mode: &'static str,
    /// Optional new keyboard. `None` clears the keyboard.
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<InlineKeyboardMarkup<'a>>,
}

/// Shape of Telegram's `sendMessage` success body.
///
/// Telegram returns more than this (chat, date, etc.); the channel
/// only cares about `result.message_id`.
#[derive(Debug, Deserialize)]
struct TelegramSendResponse {
    /// Telegram's `ok` flag.
    #[serde(default)]
    ok: bool,
    /// Human-readable error description when `ok` is false.
    #[serde(default)]
    description: Option<String>,
    /// Decoded result payload when `ok` is true.
    result: Option<TelegramMessageResult>,
}

/// Trimmed view of the `result` object in a `sendMessage` response.
#[derive(Debug, Deserialize)]
struct TelegramMessageResult {
    /// Telegram-assigned message id.
    message_id: i64,
}

/// Converts the channel-agnostic [`InlineButton`] grid into Telegram's
/// wire shape.
fn to_keyboard<'a>(buttons: &'a [Vec<InlineButton>]) -> InlineKeyboardMarkup<'a> {
    InlineKeyboardMarkup {
        inline_keyboard: buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|b| TgInlineButton {
                        text: &b.text,
                        callback_data: &b.callback_data,
                    })
                    .collect()
            })
            .collect(),
    }
}

impl TelegramChannel {
    /// Builds a channel from config plus env.
    ///
    /// Env `ISENGARD_TELEGRAM_BOT_TOKEN` wins over the config field.
    ///
    /// # Errors
    ///
    /// Returns an error when no token is resolvable, when `chat_ids`
    /// is empty, or when the HTTP client builder fails.
    pub fn from_config(cfg: TelegramConfig) -> anyhow::Result<Self> {
        let bot_token = std::env::var("ISENGARD_TELEGRAM_BOT_TOKEN")
            .ok()
            .or(cfg.bot_token)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "telegram channel: no bot token (set ISENGARD_TELEGRAM_BOT_TOKEN or notifier.telegram.bot_token)"
                )
            })?;
        if cfg.chat_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "telegram channel: chat_ids must contain at least one entry"
            ));
        }
        Ok(Self {
            bot_token,
            chat_ids: cfg.chat_ids,
            kinds: cfg.kinds,
            api_base: cfg.api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            http: Client::builder()
                .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| anyhow::anyhow!("building http client: {e}"))?,
        })
    }

    /// Configured chat ids.
    ///
    /// Used by the interactive subscriber to pick a destination for
    /// `update.pending_approval` events.
    pub fn chat_ids(&self) -> &[String] {
        &self.chat_ids
    }

    /// Sends a message with an inline keyboard and returns the
    /// resulting `message_id`.
    ///
    /// `buttons` is a 2-D array (rows of buttons). Telegram caps each
    /// `callback_data` at 64 bytes; this method does not enforce that,
    /// so keep payloads short at the call site.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails, when Telegram
    /// returns a non-2xx status, when `ok` is false in the response,
    /// or when the `result.message_id` field is missing.
    pub async fn send_inline_keyboard(
        &self,
        chat_id: &str,
        text: &str,
        buttons: Vec<Vec<InlineButton>>,
    ) -> anyhow::Result<TelegramSentMessage> {
        let url = format!("{}/bot{}/sendMessage", self.api_base, self.bot_token);
        let body = SendInteractiveBody {
            chat_id,
            text,
            parse_mode: "HTML",
            reply_markup: to_keyboard(&buttons),
        };
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("telegram sendMessage transport: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "telegram sendMessage non-success: status={status} body={body}"
            ));
        }
        let parsed: TelegramSendResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("telegram sendMessage decode: {e}"))?;
        if !parsed.ok {
            return Err(anyhow::anyhow!(
                "telegram sendMessage ok=false: {}",
                parsed.description.unwrap_or_default()
            ));
        }
        let message_id = parsed
            .result
            .ok_or_else(|| anyhow::anyhow!("telegram sendMessage missing result"))?
            .message_id;
        Ok(TelegramSentMessage {
            chat_id: chat_id.to_string(),
            message_id,
        })
    }

    /// Edits a previously-sent message in place.
    ///
    /// Pass `reply_markup = None` to drop the keyboard. The dashboard
    /// callback handler uses this after recording a decision so the
    /// buttons disappear.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails or when Telegram
    /// returns a non-2xx status.
    pub async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: i64,
        text: &str,
        reply_markup: Option<Vec<Vec<InlineButton>>>,
    ) -> anyhow::Result<()> {
        let url = format!("{}/bot{}/editMessageText", self.api_base, self.bot_token);
        let keyboard = reply_markup.as_ref().map(|kb| to_keyboard(kb));
        let body = EditMessageBody {
            chat_id,
            message_id,
            text,
            parse_mode: "HTML",
            reply_markup: keyboard,
        };
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("telegram editMessageText transport: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "telegram editMessageText non-success: status={status} body={body}"
            ));
        }
        Ok(())
    }
}

/// Stateless helper that edits a Telegram message and drops the
/// inline keyboard.
///
/// Used by the dashboard's Telegram callback handler, which has only
/// `(bot_token, chat_id, message_id)` and no full
/// [`TelegramChannel`] instance.
///
/// `api_base` defaults to `DEFAULT_API_BASE` when `None`. Tests
/// override it to a `wiremock::MockServer`.
///
/// # Errors
///
/// Returns an error when the HTTP client builder fails, when the
/// transport fails, or when Telegram returns a non-2xx status.
pub async fn edit_telegram_message_text(
    api_base: Option<&str>,
    bot_token: &str,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    let base = api_base.unwrap_or(DEFAULT_API_BASE);
    let url = format!("{base}/bot{bot_token}/editMessageText");
    let body = EditMessageBody {
        chat_id,
        message_id,
        text,
        parse_mode: "HTML",
        reply_markup: None,
    };
    let http = Client::builder()
        .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| anyhow::anyhow!("building http client: {e}"))?;
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("telegram editMessageText transport: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "telegram editMessageText non-success: status={status} body={body}"
        ));
    }
    Ok(())
}

#[async_trait]
impl NotifyChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        let text = format_event(event);
        let url = format!("{}/bot{}/sendMessage", self.api_base, self.bot_token);
        for chat_id in &self.chat_ids {
            let body = SendMessageBody {
                chat_id,
                text: &text,
            };
            match self.http.post(&url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    warn!(
                        chat_id = %chat_id,
                        status = %resp.status(),
                        "telegram sendMessage non-success"
                    );
                }
                Err(e) => {
                    warn!(chat_id = %chat_id, error = %e, "telegram sendMessage failed");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev() -> Event {
        Event {
            kind: "update.success".into(),
            occurred_at: Utc::now(),
            summary: "updated web".into(),
            container_name: Some("web".into()),
            image: Some("nginx:1.25".into()),
            old_digest: Some("sha256:aaa".into()),
            new_digest: Some("sha256:bbb".into()),
            ..Default::default()
        }
    }

    #[test]
    fn format_includes_kind_container_image_digests() {
        let s = format_event(&ev());
        assert!(s.contains("[isengard] update.success"));
        assert!(s.contains("container: web"));
        assert!(s.contains("image: nginx:1.25"));
        assert!(s.contains("digest: sha256:aaa → sha256:bbb"));
        assert!(s.contains("updated web"));
    }

    #[test]
    fn format_includes_error_when_present() {
        let mut e = ev();
        e.kind = "update.failed".into();
        e.error = Some("docker pull failed: timeout".into());
        let s = format_event(&e);
        assert!(s.contains("[isengard] update.failed"));
        assert!(s.contains("error: docker pull failed: timeout"));
    }

    #[test]
    fn format_skips_absent_optional_fields() {
        let e = Event {
            kind: "agent.disconnect_long".into(),
            occurred_at: Utc::now(),
            summary: "agent X unreachable for over 4h".into(),
            ..Default::default()
        };
        let s = format_event(&e);
        assert!(!s.contains("container:"));
        assert!(!s.contains("image:"));
        assert!(!s.contains("digest:"));
        assert!(s.contains("agent X unreachable"));
    }

    #[test]
    fn matches_kind_exact_string() {
        let cfg = TelegramConfig {
            bot_token: Some("test".into()),
            chat_ids: vec!["123".into()],
            kinds: vec!["update.success".into(), "update.failed".into()],
            api_base: None,
            tokens_per_minute: None,
        };
        let ch = TelegramChannel::from_config(cfg).unwrap();
        assert!(ch.matches_kind("update.success"));
        assert!(ch.matches_kind("update.failed"));
        assert!(!ch.matches_kind("update.checked"));
        assert!(!ch.matches_kind("agent.disconnect_long"));
    }

    #[test]
    fn from_config_errors_without_token() {
        let prev = std::env::var("ISENGARD_TELEGRAM_BOT_TOKEN").ok();
        // SAFETY: tests are not guaranteed single-threaded; the env
        // var is restored on the way out. Race-prone.
        unsafe {
            std::env::remove_var("ISENGARD_TELEGRAM_BOT_TOKEN");
        }
        let cfg = TelegramConfig {
            bot_token: None,
            chat_ids: vec!["123".into()],
            kinds: vec![],
            api_base: None,
            tokens_per_minute: None,
        };
        let r = TelegramChannel::from_config(cfg);
        if let Some(p) = prev {
            unsafe {
                std::env::set_var("ISENGARD_TELEGRAM_BOT_TOKEN", p);
            }
        }
        assert!(r.is_err());
    }

    #[test]
    fn from_config_errors_with_no_chat_ids() {
        let cfg = TelegramConfig {
            bot_token: Some("test".into()),
            chat_ids: vec![],
            kinds: vec![],
            api_base: None,
            tokens_per_minute: None,
        };
        let r = TelegramChannel::from_config(cfg);
        assert!(r.is_err());
    }
}
