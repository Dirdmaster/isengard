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
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

pub mod channel;
pub mod discord;
pub mod http;
pub mod telegram;

use crate::channel::{NotifyChannel, RateLimited};
use crate::discord::{DiscordChannel, DiscordConfig};
use crate::http::{HttpChannel, HttpConfig};
use crate::telegram::{TelegramChannel, TelegramConfig};

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
    /// Stored as String here, leaked to &'static str in event_kinds() — see note.
    kinds: Vec<String>,
    task: Option<JoinHandle<()>>,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            kinds: Vec::new(),
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
            self.channels.push(Box::new(RateLimited::new(tg, tpm)));
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
        ];

        info!(channels = self.channels.len(), "notifier initialised");
        Ok(())
    }

    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
        // Downcast the bus handle to the controller handles bundle.
        let handles = ctx
            .bus
            .clone()
            .ok_or_else(|| start_err("notifier started without ControllerHandles on PluginContext"))?
            .downcast::<ControllerHandles>()
            .map_err(|_| start_err("bus on PluginContext was not ControllerHandles"))?;
        let mut rx = handles.bus.subscribe();

        // Move channels out so the spawned task owns them (Plugin::stop
        // doesn't need them anymore).
        let channels = std::mem::take(&mut self.channels);
        let channels: Arc<[Box<dyn NotifyChannel>]> = channels.into();

        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => dispatch(&channels, event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "notifier broadcast lag — events dropped");
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
