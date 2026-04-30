//! Telegram bot channel. Outbound-only in v1: POST to /sendMessage.
//!
//! Setup: user creates a bot via @BotFather, gets a token, adds the bot to
//! a chat (1:1 or group), sends `/start` once, reads the chat_id from
//! `https://api.telegram.org/bot<TOKEN>/getUpdates`. Configures via:
//!
//! ```json
//! { "telegram": { "chat_ids": ["123456"], "kinds": ["update.success", "update.failed"] } }
//! ```
//!
//! Bot token is sourced from env `ISENGARD_TELEGRAM_BOT_TOKEN` (preferred)
//! or config field `bot_token` (fallback).

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::{NotifyChannel, format_event};

const DEFAULT_API_BASE: &str = "https://api.telegram.org";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelegramConfig {
    /// Bot token. Optional in config — env `ISENGARD_TELEGRAM_BOT_TOKEN` takes precedence.
    #[serde(default)]
    pub bot_token: Option<String>,
    /// One or more chat IDs to post to.
    #[serde(default)]
    pub chat_ids: Vec<String>,
    /// Event kinds (exact string match) this channel cares about.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Override base URL (used by tests; defaults to https://api.telegram.org).
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

pub struct TelegramChannel {
    bot_token: String,
    chat_ids: Vec<String>,
    kinds: Vec<String>,
    api_base: String,
    http: Client,
}

#[derive(Debug, Serialize)]
struct SendMessageBody<'a> {
    chat_id: &'a str,
    text: &'a str,
}

impl TelegramChannel {
    /// Build a channel from config. Env var beats config. Returns Err if no
    /// token resolvable OR chat_ids is empty.
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
        // Save & clear env to ensure isolation.
        let prev = std::env::var("ISENGARD_TELEGRAM_BOT_TOKEN").ok();
        // SAFETY: tests run with `--test-threads=1` is not guaranteed; race is possible.
        // Document this. v1.x: figure(env) crate.
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
