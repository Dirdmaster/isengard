//! Discord webhook channel. Outbound-only via incoming webhook URL.
//!
//! Setup: server admin creates a webhook in channel settings → Discord gives
//! a URL like `https://discord.com/api/webhooks/<id>/<token>`. POST `{"content": "<text>"}`
//! to that URL; max 2000 chars in `content`.

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::channel::{NotifyChannel, format_event};

const DISCORD_CONTENT_MAX: usize = 2000;
const TRUNCATED_SUFFIX: &str = "\n... [truncated]";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DiscordConfig {
    pub webhook_url: String,
    #[serde(default)]
    pub kinds: Vec<String>,
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
}
