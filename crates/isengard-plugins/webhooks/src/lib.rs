//! Isengard `webhooks` plugin (controller-side).
//!
//! Phase 12a: subscribes to the controller EventBus, persists a delivery row
//! per matching webhook, and runs a worker that POSTs queued deliveries with
//! an HMAC-SHA256 signature. Retries with exponential backoff
//! (30s, 1m, 5m, 30m, 2h) up to 5 attempts.
//!
//! See `docs/superpowers/specs/2026-05-06-phase-12a-outbound-webhooks-design.md`.

#![allow(clippy::result_large_err)]

use std::time::Duration;

use async_trait::async_trait;
use isengard_controller::ControllerHandles;
use isengard_core::{
    Capability, CoreError, Event, EventSubscriber, Plugin, PluginContext, PluginRegistration,
    Result,
};
use reqwest::Client;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::info;

pub mod backoff;
pub mod sign;
pub mod subscriber;
pub mod worker;

const PLUGIN_NAME: &str = "webhooks";
const HTTP_TIMEOUT_SECS: u64 = 10;
const WORKER_TICK: Duration = Duration::from_secs(5);
const WORKER_BATCH: i64 = 100;

/// Plugin config (from controller's `plugins.<name>` JSON). All fields
/// optional: tick + batch size are knobs operators rarely touch.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WebhooksConfig {
    /// Override the worker tick interval (seconds). Default 5.
    #[serde(default)]
    pub tick_secs: Option<u64>,
    /// Override the per-tick claim batch size. Default 100.
    #[serde(default)]
    pub batch: Option<i64>,
}

pub struct Webhooks {
    config: WebhooksConfig,
    subscriber_task: Option<JoinHandle<()>>,
    worker_task: Option<JoinHandle<()>>,
}

impl Webhooks {
    pub fn new() -> Self {
        Self {
            config: WebhooksConfig::default(),
            subscriber_task: None,
            worker_task: None,
        }
    }
}

impl Default for Webhooks {
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
impl Plugin for Webhooks {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: WebhooksConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing webhooks config: {e}")))?;
        self.config = cfg;
        info!("webhooks initialised");
        Ok(())
    }

    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
        let handles = ctx
            .bus
            .clone()
            .ok_or_else(|| start_err("webhooks started without ControllerHandles"))?
            .downcast::<ControllerHandles>()
            .map_err(|_| start_err("bus on PluginContext was not ControllerHandles"))?;

        let inventory = handles.inventory.clone();
        let bus_rx = handles.bus.subscribe();

        // Subscriber: persist a delivery row per matching webhook on each event.
        self.subscriber_task = Some(tokio::spawn(subscriber::run(inventory.clone(), bus_rx)));

        // Worker: drain pending deliveries with retry + signing.
        let http = Client::builder()
            .user_agent(concat!("isengard-webhooks/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| start_err(format!("building reqwest client: {e}")))?;

        let tick = self
            .config
            .tick_secs
            .map(Duration::from_secs)
            .unwrap_or(WORKER_TICK);
        let batch = self.config.batch.unwrap_or(WORKER_BATCH);
        self.worker_task = Some(tokio::spawn(worker::run(inventory, http, tick, batch)));

        info!("webhooks started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(t) = self.subscriber_task.take() {
            t.abort();
        }
        if let Some(t) = self.worker_task.take() {
            t.abort();
        }
        info!("webhooks stopped");
        Ok(())
    }
}

#[async_trait]
impl EventSubscriber for Webhooks {
    fn event_kinds(&self) -> &[&'static str] {
        // The actual subscriber task is spawned in `start()`; the
        // EventSubscriber trait is implemented for shape-compliance only.
        &[]
    }

    async fn handle(&self, _event: &Event, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration {
        name: PLUGIN_NAME,
        capabilities: &[Capability::Controller],
        constructor: || Box::new(Webhooks::new()) as Box<dyn Plugin>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhooks_new_has_no_tasks() {
        let w = Webhooks::new();
        assert!(w.subscriber_task.is_none());
        assert!(w.worker_task.is_none());
    }
}
