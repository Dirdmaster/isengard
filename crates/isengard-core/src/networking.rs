//! NetworkingAdapter trait + value types.
//!
//! Adapters sit ABOVE Pingora in the stack: they make a public hostname
//! reach this host's Pingora listener. Pingora handles host-header routing
//! and TLS termination. The adapter does NOT do hostname routing.
//!
//! See spec §3 in docs/superpowers/specs/2026-05-03-phase-8-networking-and-proxy-design.md.

use crate::context::PluginContext;
use crate::error::Result;
use crate::plugin::Plugin;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Tcp,
}

/// How the adapter handles TLS for a hostname it exposes. Returned from
/// `NetworkingAdapter::tls_strategy`. Pingora inspects this to decide whether
/// to terminate TLS itself or pass plain HTTP through.
///
/// Serialised as a bare discriminant string (e.g. `"edge_termination"`),
/// not as a tagged object. All variants are unit variants — no payload to
/// serialise. The runtime callback for `AdapterProvided` is wired via a
/// separate API, not through serde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsStrategy {
    /// Adapter terminates TLS at its edge. Pingora listens on plain HTTP.
    EdgeTermination,
    /// Adapter passes raw TCP through. Pingora handles TLS via its own ACME.
    PassThrough,
    /// Adapter provides a cert via callback (e.g. tailscale's tsnet auto-cert).
    /// Serialised as just the discriminant; the callback is set up by the
    /// adapter at runtime via a separate API.
    AdapterProvided,
}

/// What the controller asks the adapter to expose for a single routing rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposeSpec {
    pub public_hostname: String,
    pub local_listener_port: u16,
    pub protocol: Protocol,
    /// Adapter-specific blob (e.g. CF zone_id, tailscale tags). Opaque to core.
    pub adapter_specific: serde_json::Value,
}

/// What the adapter returns from `expose`. `id` and `adapter_data` are the
/// reverse-key for `unexpose`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposedEndpoint {
    pub id: String,
    pub url: String,
    pub adapter_data: serde_json::Value,
}

/// Per-host context an adapter receives on every call. Built by the agent.
///
/// NOTE: `host_id` is typed as `String` rather than the canonical `HostId`
/// because `HostId` currently lives in `isengard-storage`, which `isengard-core`
/// cannot depend on (storage depends on core types, not the other way). When
/// `HostId` (or a shared identifier crate) is hoisted into core, this field
/// should be tightened to `HostId`.
pub struct AdapterContext {
    pub host_id: String,
    /// Host-scoped settings slice, opaque to core. Adapters parse what they
    /// need (e.g. CF API token from `cf-tunnel.api_token`). Will be narrowed
    /// to a typed `Settings` struct in a later phase.
    pub settings: serde_json::Value,
    /// Carries the event emitter (via `plugin_ctx.events`) and host mode.
    /// Bundles what the spec calls `event_emitter` as a top-level field.
    pub plugin_ctx: PluginContext,
}

/// NetworkingAdapter is a sub-trait of `Plugin`. Implementations live as
/// crates under `crates/isengard-plugins/networking-*`.
#[async_trait]
pub trait NetworkingAdapter: Plugin {
    fn id(&self) -> &'static str;

    async fn join(&self, ctx: &AdapterContext) -> Result<()>;
    async fn leave(&self, ctx: &AdapterContext) -> Result<()>;

    async fn expose(&self, ctx: &AdapterContext, spec: &ExposeSpec) -> Result<ExposedEndpoint>;
    async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str) -> Result<()>;

    fn tls_strategy(&self) -> TlsStrategy;
}
