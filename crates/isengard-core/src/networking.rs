//! [`NetworkingAdapter`] trait + value types.
//!
//! Adapters sit ABOVE Pingora in the stack: they make a public hostname
//! reach this host's Pingora listener. Pingora handles host-header routing
//! and TLS termination. The adapter does NOT do hostname routing.
//!
//! See spec §3 in
//! `docs/superpowers/specs/2026-05-03-phase-8-networking-and-proxy-design.md`.

use crate::context::PluginContext;
use crate::error::Result;
use crate::plugin::Plugin;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Transport protocol the adapter exposes for a hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// HTTP / HTTPS.
    Http,
    /// Raw TCP passthrough.
    Tcp,
}

/// How the adapter handles TLS for a hostname it exposes.
///
/// Returned from [`NetworkingAdapter::tls_strategy`]. Pingora inspects this
/// to decide whether to terminate TLS itself or pass plain HTTP through.
///
/// Serialised as a bare discriminant string (e.g. `"edge_termination"`),
/// not as a tagged object. All variants are unit variants (no payload to
/// serialise). The runtime callback for [`Self::AdapterProvided`] is wired
/// via a separate API, not through serde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsStrategy {
    /// Adapter terminates TLS at its edge. Pingora listens on plain HTTP.
    EdgeTermination,
    /// Adapter passes raw TCP through. Pingora handles TLS via its own
    /// ACME.
    PassThrough,
    /// Adapter provides a cert via callback (e.g. tailscale's tsnet
    /// auto-cert).
    ///
    /// Serialised as the discriminant only; the callback is set up by the
    /// adapter at runtime via a separate API.
    AdapterProvided,
}

/// What the controller asks the adapter to expose for a single routing
/// rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposeSpec {
    /// Hostname operators want public traffic to reach.
    pub public_hostname: String,
    /// Local Pingora listener port the adapter should target.
    pub local_listener_port: u16,
    /// Transport protocol (HTTP or raw TCP).
    pub protocol: Protocol,
    /// Adapter-specific blob (e.g. CF zone_id, tailscale tags).
    ///
    /// Opaque to core; the adapter parses what it needs.
    pub adapter_specific: serde_json::Value,
}

/// What the adapter returns from `expose`.
///
/// `id` and `adapter_data` are the reverse-key for `unexpose`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposedEndpoint {
    /// Adapter-issued id the agent passes to `unexpose` later.
    pub id: String,
    /// Public URL operators can hit (e.g. `"https://blog.example.com"`).
    pub url: String,
    /// Free-form adapter state stored alongside the endpoint so the same
    /// adapter can find what it needs to tear down.
    pub adapter_data: serde_json::Value,
}

/// Per-host context an adapter receives on every call.
///
/// Built by the agent. `Clone` so adapters can hand it to spawned sub-tasks
/// (e.g. `cf-tunnel`'s `cloudflared` supervisor, `tailscale serve`
/// pollers) without juggling references across `'static` boundaries.
/// `Debug` is intentionally NOT derived because `PluginContext` carries
/// `Arc<dyn EventEmitter>` whose trait object isn't `Debug`. Implement a
/// custom `Debug` if you ever need one.
///
/// NOTE: `host_id` is typed as `String` rather than the canonical
/// [`crate::HostId`] because `HostId` currently lives in `isengard-storage`,
/// which `isengard-core` cannot depend on (storage depends on core types,
/// not the other way). When `HostId` (or a shared identifier crate) is
/// hoisted into core, this field should be tightened to `HostId`.
#[derive(Clone)]
pub struct AdapterContext {
    /// Stable host identifier, hex form.
    pub host_id: String,
    /// Host-scoped settings slice, opaque to core.
    ///
    /// Adapters parse what they need (e.g. CF API token from
    /// `cf-tunnel.api_token`). Will be narrowed to a typed `Settings`
    /// struct in a later phase.
    pub settings: serde_json::Value,
    /// Carries the event emitter (via `plugin_ctx.events`) and host mode.
    ///
    /// Bundles what the spec calls `event_emitter` as a top-level field.
    pub plugin_ctx: PluginContext,
}

/// Networking adapter capability trait.
///
/// Sub-trait of [`Plugin`]. Implementations live as crates under
/// `crates/isengard-plugins/networking-*`.
#[async_trait]
pub trait NetworkingAdapter: Plugin {
    /// Stable adapter id (e.g. `"cf-tunnel"`, `"tailscale"`).
    fn id(&self) -> &'static str;

    /// Run any per-host initialisation the adapter needs (e.g. log into
    /// Cloudflare, start tsnet).
    ///
    /// # Errors
    ///
    /// Returns [`crate::CoreError`] when the adapter cannot initialise on
    /// this host.
    async fn join(&self, ctx: &AdapterContext) -> Result<()>;

    /// Tear down what [`join`](Self::join) created.
    ///
    /// # Errors
    ///
    /// Returns [`crate::CoreError`] when the teardown fails. The agent
    /// logs and continues.
    async fn leave(&self, ctx: &AdapterContext) -> Result<()>;

    /// Make the hostname in `spec` route to the agent's local listener.
    ///
    /// # Errors
    ///
    /// Returns [`crate::CoreError`] when the adapter cannot create the
    /// public reachability (DNS failed, tunnel quota hit, ...).
    async fn expose(&self, ctx: &AdapterContext, spec: &ExposeSpec) -> Result<ExposedEndpoint>;

    /// Tear down what `expose` previously created for this `endpoint_id`.
    ///
    /// **Contract:** the adapter is responsible for persisting whatever it
    /// needs to perform a clean teardown (DNS record name, ingress rule
    /// id, tunnel uuid, etc.) keyed by `endpoint_id`. The agent does not
    /// call `unexpose` with the original [`ExposeSpec`]: only the id
    /// returned from [`expose`](Self::expose). If your adapter needs the
    /// spec to unexpose correctly, store it inside
    /// [`ExposedEndpoint::adapter_data`] and look it up on `unexpose`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::CoreError`] when the teardown fails.
    async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str) -> Result<()>;

    /// Tell the agent how Pingora should be configured for hostnames this
    /// adapter exposes.
    fn tls_strategy(&self) -> TlsStrategy;
}
