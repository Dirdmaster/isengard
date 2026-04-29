//! Plugin trait surface.
//!
//! Every plugin implements [`Plugin`] (lifecycle). It then opts into one or more
//! capability sub-traits to declare what it does and where it runs:
//!
//! - [`AgentPlugin`] — runs on agents, runs work cycles
//! - [`ControllerPlugin`] — marker; runs on the controller (no extra methods yet)
//! - [`EventSubscriber`] — reacts to journal events on the controller
//! - [`HttpHandler`] — mounts HTTP routes on the controller's axum router (Phase 5)
//!
//! Inputs and outputs that cross the plugin boundary use serde-clean types so
//! the same trait works when plugins are loaded out-of-process (Phase 2+) or
//! sandboxed via WASM (later).

use crate::context::PluginContext;
use crate::error::Result;
use crate::event::{Event, EventKind};
use async_trait::async_trait;

/// Lifecycle. Every plugin implements this.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Stable plugin identifier (e.g. "updater", "notifier"). Used for config
    /// namespacing and journal events. Must match the directory name under
    /// `crates/isengard-plugins/`.
    fn name(&self) -> &'static str;

    /// Semantic version of this plugin.
    fn version(&self) -> &'static str;

    /// Called once at host startup. The plugin reads its slice of the config,
    /// returns `Err` if it's invalid. The host aborts startup on `Err`.
    async fn init(&mut self, ctx: &PluginContext) -> Result<()>;

    /// Called after `init` succeeds. Plugin spawns its tasks here.
    async fn start(&mut self, ctx: &PluginContext) -> Result<()>;

    /// Called on graceful shutdown.
    async fn stop(&mut self) -> Result<()>;
}

/// Capability: this plugin runs on agents and exposes a work cycle the agent
/// triggers (e.g. the updater plugin's check-and-recreate loop).
#[async_trait]
pub trait AgentPlugin: Plugin {
    async fn run_cycle(&self, ctx: &PluginContext) -> Result<()>;
}

/// Capability: this plugin runs on the controller. Marker trait — concrete
/// behavior is provided by [`EventSubscriber`] and [`HttpHandler`].
pub trait ControllerPlugin: Plugin {}

/// Capability: this plugin reacts to journal events on the controller.
#[async_trait]
pub trait EventSubscriber: Plugin {
    /// Filter — which event kinds this subscriber wants. Empty = all.
    fn subscribed_events(&self) -> &[EventKind];

    /// Called on every matching event. Errors are logged; they do not stop
    /// the journal.
    async fn handle(&self, event: &Event, ctx: &PluginContext) -> Result<()>;
}

/// Capability: this plugin mounts HTTP routes onto the controller's axum
/// router. Real signature lands in Phase 5 once axum is added; for Phase 1 it
/// is a marker only.
pub trait HttpHandler: Plugin {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::context::HostMode;

    /// Reusable no-op plugin for testing the trait surface and registration.
    pub struct NoopPlugin;

    #[async_trait]
    impl Plugin for NoopPlugin {
        fn name(&self) -> &'static str { "noop" }
        fn version(&self) -> &'static str { "0.0.0" }
        async fn init(&mut self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
        async fn start(&mut self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
        async fn stop(&mut self) -> Result<()> { Ok(()) }
    }

    #[async_trait]
    impl AgentPlugin for NoopPlugin {
        async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
    }

    impl ControllerPlugin for NoopPlugin {}

    #[tokio::test]
    async fn noop_lifecycle_runs_clean() {
        let mut p = NoopPlugin;
        let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);
        assert_eq!(p.name(), "noop");
        assert_eq!(p.version(), "0.0.0");
        p.init(&ctx).await.unwrap();
        p.start(&ctx).await.unwrap();
        p.run_cycle(&ctx).await.unwrap();
        p.stop().await.unwrap();
    }

    #[tokio::test]
    async fn noop_carries_correct_mode_in_context() {
        let p = NoopPlugin;
        let ctx = PluginContext::new(HostMode::Controller, serde_json::json!({"k": "v"}));
        p.run_cycle(&ctx).await.unwrap();
        assert_eq!(ctx.mode, HostMode::Controller);
    }
}
