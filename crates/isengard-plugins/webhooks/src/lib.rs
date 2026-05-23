#![doc = include_str!("../docs/_crate.md")]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]
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
pub mod lifecycle;
pub mod sign;
pub mod subscriber;
pub mod worker;

/// Stable plugin name surfaced to the controller and host registry.
const PLUGIN_NAME: &str = "webhooks";

/// Per-request HTTP timeout for outbound POSTs.
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Default worker tick interval.
const WORKER_TICK: Duration = Duration::from_secs(5);

/// Default per-tick claim batch size.
const WORKER_BATCH: i64 = 100;

/// Parsed `[plugins.webhooks]` config block.
///
/// Both knobs are optional; operators rarely override them. The
/// defaults match `WORKER_TICK` and `WORKER_BATCH`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WebhooksConfig {
    /// Override the worker tick interval, in seconds.
    #[serde(default)]
    pub tick_secs: Option<u64>,
    /// Override the per-tick claim batch size.
    #[serde(default)]
    pub batch: Option<i64>,
}

/// Controller-side webhooks plugin instance.
///
/// Holds the resolved config and the join handles for the three
/// spawned tasks: the general subscriber, the lifecycle subscriber,
/// and the worker.
pub struct Webhooks {
    /// Resolved config from `init`.
    config: WebhooksConfig,
    /// Join handle for [`subscriber::run`].
    subscriber_task: Option<JoinHandle<()>>,
    /// Join handle for [`lifecycle::run`].
    lifecycle_task: Option<JoinHandle<()>>,
    /// Join handle for [`worker::run`].
    worker_task: Option<JoinHandle<()>>,
}

impl Webhooks {
    /// Builds an empty plugin. Tasks are spawned in [`Plugin::start`].
    pub fn new() -> Self {
        Self {
            config: WebhooksConfig::default(),
            subscriber_task: None,
            lifecycle_task: None,
            worker_task: None,
        }
    }
}

impl Default for Webhooks {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps any displayable error into [`CoreError::InitFailed`] for the
/// webhooks plugin.
fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Wraps any displayable error into [`CoreError::StartFailed`] for the
/// webhooks plugin.
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

    /// Decodes config and stores it on the plugin instance.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InitFailed`] when the config JSON is
    /// malformed.
    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: WebhooksConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing webhooks config: {e}")))?;
        self.config = cfg;
        info!("webhooks initialised");
        Ok(())
    }

    /// Spawns the subscriber, lifecycle, and worker tasks.
    ///
    /// Each subscriber gets its own `bus.subscribe()` receiver so a
    /// lag on one tap doesn't drop the other's events. The worker
    /// owns the `reqwest::Client`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StartFailed`] when the plugin context lacks
    /// a downcast-compatible [`ControllerHandles`] or when the HTTP
    /// client builder fails.
    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
        let handles = ctx
            .bus
            .clone()
            .ok_or_else(|| start_err("webhooks started without ControllerHandles"))?
            .downcast::<ControllerHandles>()
            .map_err(|_| start_err("bus on PluginContext was not ControllerHandles"))?;

        let inventory = handles.inventory.clone();
        let bus_rx = handles.bus.subscribe();

        self.subscriber_task = Some(tokio::spawn(subscriber::run(inventory.clone(), bus_rx)));

        let lifecycle_rx = handles.bus.subscribe();
        self.lifecycle_task = Some(tokio::spawn(lifecycle::run(
            inventory.clone(),
            lifecycle_rx,
        )));

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
        if let Some(t) = self.lifecycle_task.take() {
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
    /// Returns an empty slice. The plugin owns its own subscribers
    /// spawned in [`Plugin::start`]; this impl exists for trait
    /// conformance only.
    fn event_kinds(&self) -> &[&'static str] {
        &[]
    }

    /// Always returns `Ok(())`. The plugin's actual handling lives in
    /// the spawned tasks.
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
        assert!(w.lifecycle_task.is_none());
        assert!(w.worker_task.is_none());
    }
}
