# Agent-Owned Ingress Network Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every route reconcile the runtime networking needed for the local Pingora proxy to reach its upstream container.

**Architecture:** Keep route intent in the controller and move host-local network reconciliation into the agent runtime boundary. `apply_config_with_backend` asks the runtime for an `IngressEndpoint`, Docker ensures `isengard-proxy` exists and attaches bridge-networked containers, and the proxy registry records either a routable upstream or an explicit unresolved reason. Remove the current implicit `127.0.0.1` fallback for unresolved Docker routes.

**Tech Stack:** Rust, Tokio, Bollard, Pingora, existing `RuntimeBackend`, `ProxyConfig`, `UpstreamRegistry`, and ignored real-Docker integration tests.

---

## File Structure

- `crates/isengard-agent/src/runtime/spec.rs`: define `IngressEndpoint`, `UnresolvedIngressReason`, and optional network-mode metadata on `NetworkSettings`.
- `crates/isengard-agent/src/runtime/mod.rs`: add the `RuntimeBackend::ensure_ingress_attachment` trait method with a default inspect-only implementation.
- `crates/isengard-agent/src/runtime/bollard_backend.rs`: implement Docker network create/connect/re-inspect behavior for `isengard-proxy`, host-network gateway handling, and unresolved reasons.
- `crates/isengard-agent/src/proxy/discovery.rs`: keep IP picking focused on existing snapshot IP maps; remove responsibility for unresolved route classification after the runtime endpoint method lands.
- `crates/isengard-agent/src/proxy/upstreams.rs`: extend registry entries to store unresolved routes alongside active upstreams.
- `crates/isengard-agent/src/proxy/router.rs`: return a 503 with a precise Pingora error context for unresolved routes.
- `crates/isengard-agent/src/proxy/mod.rs`: call `ensure_ingress_attachment`, install only valid endpoints, and preserve generation/healthcheck semantics.
- `crates/isengard-agent/tests/proxy_apply_config.rs`: mock backend coverage for active, host-network, and unresolved route behavior.
- `crates/isengard-agent/tests/proxy_label_discovery_e2e.rs`: add an ignored real-Docker route test that starts a bridge-networked container without `isengard-proxy` and verifies auto-attach.
- `docker/README.md`, `docker/compose.yaml`, `install/compose.yaml`: update operator-facing docs from manual opt-in to agent-owned ingress network.

## Task 1: Runtime Endpoint Types and Default Trait Method

**Files:**
- Modify: `crates/isengard-agent/src/runtime/spec.rs`
- Modify: `crates/isengard-agent/src/runtime/mod.rs`

- [ ] **Step 1: Add endpoint and unresolved reason types**

Add this near `NetworkSettings` in `crates/isengard-agent/src/runtime/spec.rs`:

```rust
/// Result of reconciling a route target onto the runtime's ingress fabric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressEndpoint {
    /// A reachable upstream IP and the network path used to reach it.
    Ready {
        ip: std::net::IpAddr,
        mode: IngressEndpointMode,
    },
    /// The route exists, but the runtime cannot currently provide a reachable endpoint.
    Unresolved(UnresolvedIngressReason),
}

/// How the proxy reaches an ingress endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressEndpointMode {
    /// Container is attached to the Isengard ingress network.
    IsengardNetwork,
    /// Container uses the host network namespace; proxy targets the Docker host gateway.
    HostNetwork,
    /// Caller supplied a literal container IP in ProxyConfig.
    ProvidedIp,
}

/// Stable unresolved route reason surfaced in logs, proxy 503s, and later UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedIngressReason {
    ContainerMissing,
    ContainerStopped,
    IngressNetworkCreateFailed,
    IngressNetworkAttachFailed,
    NoUsableContainerIp,
    UnsupportedNetworkModeNone,
    InvalidContainerPort,
}

impl UnresolvedIngressReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContainerMissing => "container_missing",
            Self::ContainerStopped => "container_stopped",
            Self::IngressNetworkCreateFailed => "ingress_network_create_failed",
            Self::IngressNetworkAttachFailed => "ingress_network_attach_failed",
            Self::NoUsableContainerIp => "no_usable_container_ip",
            Self::UnsupportedNetworkModeNone => "unsupported_network_mode_none",
            Self::InvalidContainerPort => "invalid_container_port",
        }
    }
}

/// Docker/Wisp network mode classification used by ingress reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerNetworkMode {
    Bridge,
    Host,
    None,
    Unknown,
}
```

Extend `NetworkSettings`:

```rust
#[derive(Debug, Clone, Default)]
pub struct NetworkSettings {
    /// `network name -> attached IP`.
    pub ip_addresses: BTreeMap<String, std::net::IpAddr>,
    /// `"80/tcp" -> [host bindings]`.
    pub ports: BTreeMap<String, Vec<HostPort>>,
    /// Runtime network mode. Docker fills this from HostConfig.network_mode or
    /// NetworkSettings.Networks; Wisp can set it from its persisted spec.
    pub mode: ContainerNetworkMode,
}
```

Add this default impl below the enum:

```rust
impl Default for ContainerNetworkMode {
    fn default() -> Self {
        Self::Unknown
    }
}
```

- [ ] **Step 2: Re-export new types**

In `crates/isengard-agent/src/runtime/mod.rs`, update the `pub use spec::{...}` list:

```rust
pub use spec::{
    ContainerCreateSpec, ContainerNetworkMode, ContainerSnapshot, ContainerState, HealthState,
    HealthcheckSpec, HostPort, IngressEndpoint, IngressEndpointMode, LinuxResources, ListFilter,
    LogChunk, LogOptions, LogSource, MountKind, MountSpec, NetworkSettings, PortProtocol,
    PortSpec, RestartPolicy, RuntimeEvent, RuntimeEventType, SecretMount, UnresolvedIngressReason,
};
```

- [ ] **Step 3: Add default trait method**

Add this method to `RuntimeBackend` after `ensure_network`:

```rust
/// Ensure `container_ref` has a proxy-reachable ingress endpoint.
///
/// The default implementation is conservative: it only inspects the container
/// and uses an existing `isengard-proxy` IP when present. Backends that can
/// mutate networking should override this method.
async fn ensure_ingress_attachment(
    &self,
    container_ref: &str,
) -> Result<IngressEndpoint, RuntimeError> {
    let Some(snapshot) = self.inspect_container(container_ref).await? else {
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::ContainerMissing,
        ));
    };
    if snapshot.state != ContainerState::Running {
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::ContainerStopped,
        ));
    }
    if let Some(ip) = snapshot.network_settings.ip_addresses.get("isengard-proxy") {
        return Ok(IngressEndpoint::Ready {
            ip: *ip,
            mode: IngressEndpointMode::IsengardNetwork,
        });
    }
    Ok(IngressEndpoint::Unresolved(
        UnresolvedIngressReason::NoUsableContainerIp,
    ))
}
```

- [ ] **Step 4: Run the compile check and fix required mock impl fallout**

Run:

```bash
cargo test -p isengard-agent runtime::tests::mock_backend_satisfies_dyn_trait
```

Expected before mock fallout is fixed: compile failures only where struct literals need the new `mode` field. Replace those struct literals with `NetworkSettings::default()` or add `mode: ContainerNetworkMode::Unknown`. Re-run the same command until it passes.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-agent/src/runtime/spec.rs crates/isengard-agent/src/runtime/mod.rs
git commit -m "feat(agent): add ingress endpoint runtime types"
```

## Task 2: Docker Runtime Ingress Reconciliation

**Files:**
- Modify: `crates/isengard-agent/src/runtime/bollard_backend.rs`
- Test: `crates/isengard-agent/src/runtime/bollard_backend.rs` unit tests

- [ ] **Step 1: Write failing unit tests for network mode mapping**

Add tests in the existing `#[cfg(test)]` module in `bollard_backend.rs`:

```rust
#[test]
fn map_inspect_marks_host_network_mode() {
    use bollard::secret::{ContainerConfig, ContainerInspectResponse, HostConfig, NetworkSettings as BollardNetworkSettings};
    let inspect = ContainerInspectResponse {
        id: Some("c1".into()),
        name: Some("/plex".into()),
        config: Some(ContainerConfig {
            image: Some("plex:latest".into()),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            network_mode: Some("host".into()),
            ..Default::default()
        }),
        network_settings: Some(BollardNetworkSettings {
            networks: Some(std::collections::HashMap::from([(
                "host".into(),
                bollard::secret::EndpointSettings::default(),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    };

    let snap = map_inspect(inspect);
    assert_eq!(snap.network_settings.mode, crate::runtime::ContainerNetworkMode::Host);
}

#[test]
fn map_inspect_marks_none_network_mode() {
    use bollard::secret::{ContainerConfig, ContainerInspectResponse, HostConfig, NetworkSettings as BollardNetworkSettings};
    let inspect = ContainerInspectResponse {
        id: Some("c1".into()),
        name: Some("/isolated".into()),
        config: Some(ContainerConfig {
            image: Some("alpine:latest".into()),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()),
            ..Default::default()
        }),
        network_settings: Some(BollardNetworkSettings {
            networks: Some(std::collections::HashMap::from([(
                "none".into(),
                bollard::secret::EndpointSettings::default(),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    };

    let snap = map_inspect(inspect);
    assert_eq!(snap.network_settings.mode, crate::runtime::ContainerNetworkMode::None);
}
```

- [ ] **Step 2: Verify the tests fail**

Run:

```bash
cargo test -p isengard-agent runtime::bollard_backend::tests::map_inspect_marks_ --lib
```

Expected: FAIL because `NetworkSettings.mode` is still `Unknown`.

- [ ] **Step 3: Implement mode mapping**

Add helper:

```rust
fn classify_network_mode(
    host_mode: Option<&str>,
    networks: Option<&std::collections::HashMap<String, bollard::secret::EndpointSettings>>,
) -> ContainerNetworkMode {
    match host_mode.unwrap_or_default() {
        "host" => return ContainerNetworkMode::Host,
        "none" => return ContainerNetworkMode::None,
        "bridge" => return ContainerNetworkMode::Bridge,
        _ => {}
    }
    let Some(networks) = networks else {
        return ContainerNetworkMode::Unknown;
    };
    if networks.contains_key("host") {
        ContainerNetworkMode::Host
    } else if networks.contains_key("none") {
        ContainerNetworkMode::None
    } else if !networks.is_empty() {
        ContainerNetworkMode::Bridge
    } else {
        ContainerNetworkMode::Unknown
    }
}
```

Update the import from `super::{...}` near the top of `bollard_backend.rs` to include:

```rust
ContainerNetworkMode, IngressEndpoint, IngressEndpointMode, UnresolvedIngressReason,
```

In both `map_summary` and `map_inspect`, set:

```rust
network_settings.mode = classify_network_mode(
    inspect.host_config.as_ref().and_then(|h| h.network_mode.as_deref()),
    inspect.network_settings.as_ref().and_then(|ns| ns.networks.as_ref()),
);
```

For `map_summary`, use `None` for `host_mode` and the summary network map.

- [ ] **Step 4: Implement Docker `ensure_network`**

Add this method to `impl RuntimeBackend for BollardBackend`:

```rust
async fn ensure_network(&self, network: &str) -> Result<(), RuntimeError> {
    match self.docker.inspect_network(network, None).await {
        Ok(_) => return Ok(()),
        Err(e) => {
            let s = e.to_string().to_lowercase();
            if !(s.contains("404") || s.contains("no such network")) {
                return Err(RuntimeError::Network(format!(
                    "inspect_network {network}: {e}"
                )));
            }
        }
    }

    match self
        .docker
        .create_network(bollard::network::CreateNetworkOptions {
            name: network.to_string(),
            driver: "bridge".to_string(),
            check_duplicate: true,
            ..Default::default()
        })
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            let s = e.to_string().to_lowercase();
            if s.contains("already exists") {
                Ok(())
            } else {
                Err(RuntimeError::Network(format!("create_network {network}: {e}")))
            }
        }
    }
}
```

- [ ] **Step 5: Implement `ensure_ingress_attachment` for `BollardBackend`**

Add to the `impl RuntimeBackend for BollardBackend` block:

```rust
async fn ensure_ingress_attachment(
    &self,
    container_ref: &str,
) -> Result<IngressEndpoint, RuntimeError> {
    let Some(initial) = self.inspect_container(container_ref).await? else {
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::ContainerMissing,
        ));
    };
    if initial.state != ContainerState::Running {
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::ContainerStopped,
        ));
    }

    match initial.network_settings.mode {
        ContainerNetworkMode::Host => {
            let Some(ip) = docker_host_gateway_ip() else {
                return Ok(IngressEndpoint::Unresolved(
                    UnresolvedIngressReason::NoUsableContainerIp,
                ));
            };
            return Ok(IngressEndpoint::Ready {
                ip: ip.into(),
                mode: IngressEndpointMode::HostNetwork,
            });
        }
        ContainerNetworkMode::None => {
            return Ok(IngressEndpoint::Unresolved(
                UnresolvedIngressReason::UnsupportedNetworkModeNone,
            ));
        }
        ContainerNetworkMode::Bridge | ContainerNetworkMode::Unknown => {}
    }

    if let Some(ip) = initial.network_settings.ip_addresses.get(crate::proxy::SHARED_PROXY_NETWORK)
    {
        return Ok(IngressEndpoint::Ready {
            ip: *ip,
            mode: IngressEndpointMode::IsengardNetwork,
        });
    }

    if let Err(e) = self.ensure_network(crate::proxy::SHARED_PROXY_NETWORK).await {
        tracing::warn!(
            container = %container_ref,
            error = %e,
            "ingress: failed to ensure isengard proxy network",
        );
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::IngressNetworkCreateFailed,
        ));
    }

    if let Err(e) = self
        .connect_network(container_ref, crate::proxy::SHARED_PROXY_NETWORK)
        .await
    {
        tracing::warn!(
            container = %container_ref,
            network = crate::proxy::SHARED_PROXY_NETWORK,
            error = %e,
            "ingress: failed to attach container to proxy network",
        );
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::IngressNetworkAttachFailed,
        ));
    }

    let Some(after) = self.inspect_container(container_ref).await? else {
        return Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::ContainerMissing,
        ));
    };
    if let Some(ip) = after.network_settings.ip_addresses.get(crate::proxy::SHARED_PROXY_NETWORK) {
        return Ok(IngressEndpoint::Ready {
            ip: *ip,
            mode: IngressEndpointMode::IsengardNetwork,
        });
    }
    Ok(IngressEndpoint::Unresolved(
        UnresolvedIngressReason::NoUsableContainerIp,
    ))
}
```

- [ ] **Step 6: Make `connect_network` idempotent**

Change `connect_network` error handling so Docker's "already exists" / "already connected" case returns `Ok(())`.

Use:

```rust
let s = e.to_string().to_lowercase();
if s.contains("already exists") || s.contains("already connected") {
    Ok(())
} else {
    Err(RuntimeError::Network(format!(
        "connect_network {network} -> {container_id}: {e}"
    )))
}
```

- [ ] **Step 7: Run Docker runtime unit tests**

Run:

```bash
cargo test -p isengard-agent runtime::bollard_backend::tests --lib
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/isengard-agent/src/runtime/bollard_backend.rs
git commit -m "feat(agent): reconcile docker ingress attachment"
```

## Task 3: Proxy Registry Unresolved Routes

**Files:**
- Modify: `crates/isengard-agent/src/proxy/upstreams.rs`
- Modify: `crates/isengard-agent/src/proxy/router.rs`
- Test: `crates/isengard-agent/src/proxy/upstreams.rs`
- Test: `crates/isengard-agent/tests/proxy_basic_routing.rs`

- [ ] **Step 1: Add unresolved registry types**

In `upstreams.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedUpstream {
    pub container_id: String,
    pub reason: crate::runtime::UnresolvedIngressReason,
}

#[derive(Debug, Clone)]
pub enum RouteTarget {
    Ready(Upstream),
    Unresolved(UnresolvedUpstream),
}
```

Change the registry map:

```rust
map: HashMap<String, RouteTarget>,
```

Add methods:

```rust
pub fn set_unresolved(
    &mut self,
    host: impl Into<String>,
    unresolved: UnresolvedUpstream,
) {
    self.map.insert(host.into(), RouteTarget::Unresolved(unresolved));
}

pub fn get_target(&self, host: &str) -> Option<&RouteTarget> {
    self.map.get(host)
}
```

Keep `set`, `get`, `get_mut`, `remove`, `set_state`, `remove_if_draining`, and `iter` working with ready upstreams by matching `RouteTarget::Ready`.

- [ ] **Step 2: Add registry tests**

Add to `state_tests`:

```rust
#[test]
fn unresolved_route_is_stored_without_ready_upstream() {
    let mut reg = UpstreamRegistry::new();
    reg.set_unresolved(
        "plex.test",
        UnresolvedUpstream {
            container_id: "plex".into(),
            reason: crate::runtime::UnresolvedIngressReason::UnsupportedNetworkModeNone,
        },
    );

    assert!(reg.get("plex.test").is_none());
    match reg.get_target("plex.test").unwrap() {
        RouteTarget::Unresolved(u) => {
            assert_eq!(u.container_id, "plex");
            assert_eq!(
                u.reason,
                crate::runtime::UnresolvedIngressReason::UnsupportedNetworkModeNone
            );
        }
        RouteTarget::Ready(_) => panic!("expected unresolved route"),
    }
}
```

- [ ] **Step 3: Verify the registry test is red**

Run:

```bash
cargo test -p isengard-agent proxy::upstreams::state_tests::unresolved_route_is_stored_without_ready_upstream --lib
```

Expected: FAIL until registry is updated. A compile error for missing `RouteTarget` / `set_unresolved` also counts as the correct red signal.

- [ ] **Step 4: Update router to surface unresolved reasons**

In `router.rs`, replace the `get(&host)` lookup with:

```rust
let Some(target) = upstreams.get_target(&host) else {
    return Err(pingora_core::Error::because(
        pingora_core::ErrorType::HTTPStatus(404),
        "no_route",
        format!("no routing rule for {host}"),
    ));
};

let up = match target {
    crate::proxy::upstreams::RouteTarget::Ready(up) => up,
    crate::proxy::upstreams::RouteTarget::Unresolved(u) => {
        return Err(pingora_core::Error::because(
            pingora_core::ErrorType::HTTPStatus(503),
            u.reason.as_str(),
            format!("route for {host} unresolved: {}", u.reason.as_str()),
        ));
    }
};
```

- [ ] **Step 5: Run proxy routing tests**

Run:

```bash
cargo test -p isengard-agent proxy::upstreams::state_tests --lib
cargo test -p isengard-agent --test proxy_basic_routing route_by_host_header_returns_origin_response
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-agent/src/proxy/upstreams.rs crates/isengard-agent/src/proxy/router.rs crates/isengard-agent/tests/proxy_basic_routing.rs
git commit -m "feat(agent): track unresolved proxy routes"
```

## Task 4: Apply ProxyConfig Through Runtime Ingress Endpoint

**Files:**
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Test: `crates/isengard-agent/tests/proxy_apply_config.rs`

- [ ] **Step 1: Add mock backend tests**

In `proxy_apply_config.rs`, add a small `MockBackend` that implements `RuntimeBackend` and returns configured `IngressEndpoint` values.

Use this shape:

```rust
#[derive(Debug, Default)]
struct MockBackend {
    endpoints: std::collections::HashMap<String, isengard_agent::runtime::IngressEndpoint>,
}

#[async_trait::async_trait]
impl isengard_agent::runtime::RuntimeBackend for MockBackend {
    async fn ensure_image(&self, _reference: &str) -> Result<String, isengard_agent::runtime::RuntimeError> { Ok(String::new()) }
    async fn create_container(&self, _spec: &isengard_agent::runtime::ContainerCreateSpec) -> Result<String, isengard_agent::runtime::RuntimeError> { Ok(String::new()) }
    async fn start_container(&self, _id: &str) -> Result<(), isengard_agent::runtime::RuntimeError> { Ok(()) }
    async fn stop_container(&self, _id: &str, _timeout_s: u32) -> Result<(), isengard_agent::runtime::RuntimeError> { Ok(()) }
    async fn remove_container(&self, _id: &str, _force: bool) -> Result<(), isengard_agent::runtime::RuntimeError> { Ok(()) }
    async fn list_containers(&self, _filter: isengard_agent::runtime::ListFilter) -> Result<Vec<isengard_agent::runtime::ContainerSnapshot>, isengard_agent::runtime::RuntimeError> { Ok(Vec::new()) }
    async fn inspect_container(&self, _id: &str) -> Result<Option<isengard_agent::runtime::ContainerSnapshot>, isengard_agent::runtime::RuntimeError> { Ok(None) }
    async fn connect_network(&self, _container_id: &str, _network: &str) -> Result<(), isengard_agent::runtime::RuntimeError> { Ok(()) }
    async fn disconnect_network(&self, _container_id: &str, _network: &str) -> Result<(), isengard_agent::runtime::RuntimeError> { Ok(()) }
    async fn ensure_ingress_attachment(&self, container_ref: &str) -> Result<isengard_agent::runtime::IngressEndpoint, isengard_agent::runtime::RuntimeError> {
        Ok(self.endpoints.get(container_ref).cloned().unwrap_or(
            isengard_agent::runtime::IngressEndpoint::Unresolved(
                isengard_agent::runtime::UnresolvedIngressReason::ContainerMissing,
            ),
        ))
    }
    fn stream_logs(&self, _id: &str, _opts: isengard_agent::runtime::LogOptions) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = isengard_agent::runtime::LogChunk> + Send>> {
        Box::pin(futures_util::stream::empty())
    }
    fn stream_events(&self) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = isengard_agent::runtime::RuntimeEvent> + Send>> {
        Box::pin(futures_util::stream::empty())
    }
    async fn run_healthcheck(&self, _id: &str, _hc: &isengard_agent::runtime::HealthcheckSpec) -> Result<isengard_agent::runtime::HealthState, isengard_agent::runtime::RuntimeError> {
        Ok(isengard_agent::runtime::HealthState::Healthy)
    }
    fn name(&self) -> &'static str { "mock" }
}
```

Then add tests:

```rust
#[tokio::test]
async fn apply_config_uses_runtime_ingress_endpoint_when_container_ip_empty() {
    let state = isengard_agent::proxy::ProxyState::new();
    let mut backend = MockBackend::default();
    backend.endpoints.insert(
        "web".into(),
        isengard_agent::runtime::IngressEndpoint::Ready {
            ip: "172.30.0.9".parse().unwrap(),
            mode: isengard_agent::runtime::IngressEndpointMode::IsengardNetwork,
        },
    );

    let cfg = cfg_with_empty_ip(1, "web", 8080, "web.test");
    isengard_agent::proxy::apply_config_with_backend(&state, cfg, Some(&backend))
        .await
        .unwrap();

    let up = state.upstreams.read().await;
    let got = up.get("web.test").expect("ready route applied");
    assert_eq!(got.addr.ip().to_string(), "172.30.0.9");
}

#[tokio::test]
async fn apply_config_records_unresolved_route_without_localhost_fallback() {
    let state = isengard_agent::proxy::ProxyState::new();
    let mut backend = MockBackend::default();
    backend.endpoints.insert(
        "isolated".into(),
        isengard_agent::runtime::IngressEndpoint::Unresolved(
            isengard_agent::runtime::UnresolvedIngressReason::UnsupportedNetworkModeNone,
        ),
    );

    let cfg = cfg_with_empty_ip(1, "isolated", 8080, "isolated.test");
    isengard_agent::proxy::apply_config_with_backend(&state, cfg, Some(&backend))
        .await
        .unwrap();

    let up = state.upstreams.read().await;
    assert!(up.get("isolated.test").is_none());
    match up.get_target("isolated.test").unwrap() {
        isengard_agent::proxy::upstreams::RouteTarget::Unresolved(u) => {
            assert_eq!(
                u.reason,
                isengard_agent::runtime::UnresolvedIngressReason::UnsupportedNetworkModeNone
            );
        }
        _ => panic!("expected unresolved route"),
    }
}
```

Define `cfg_with_empty_ip` locally by copying the existing `ProxyConfig` construction pattern and setting `container_ip: String::new()`.

- [ ] **Step 2: Verify tests fail**

Run:

```bash
cargo test -p isengard-agent --test proxy_apply_config apply_config_uses_runtime_ingress_endpoint_when_container_ip_empty
cargo test -p isengard-agent --test proxy_apply_config apply_config_records_unresolved_route_without_localhost_fallback
```

Expected: FAIL because `apply_config_with_backend` still calls `resolve_container_ip` and falls back to localhost.

- [ ] **Step 3: Update `apply_config_with_backend`**

Replace the `resolved_ip` block with:

```rust
let endpoint = if !up.container_ip.is_empty() {
    let ip = up
        .container_ip
        .parse()
        .map_err(|e| anyhow::anyhow!("bad container_ip {}: {e}", up.container_ip))?;
    crate::runtime::IngressEndpoint::Ready {
        ip,
        mode: crate::runtime::IngressEndpointMode::ProvidedIp,
    }
} else if let Some(b) = backend {
    b.ensure_ingress_attachment(&up.container_id).await?
} else {
    crate::runtime::IngressEndpoint::Unresolved(
        crate::runtime::UnresolvedIngressReason::NoUsableContainerIp,
    )
};

let ip = match endpoint {
    crate::runtime::IngressEndpoint::Ready { ip, mode } => {
        tracing::debug!(
            hostname = %rule.public_hostname,
            container_id = %up.container_id,
            ?mode,
            "proxy: resolved ingress endpoint",
        );
        ip
    }
    crate::runtime::IngressEndpoint::Unresolved(reason) => {
        tracing::warn!(
            hostname = %rule.public_hostname,
            container_id = %up.container_id,
            reason = reason.as_str(),
            "proxy: route unresolved; not installing localhost fallback",
        );
        new_reg.set_unresolved(
            rule.public_hostname.clone(),
            upstreams::UnresolvedUpstream {
                container_id: up.container_id.clone(),
                reason,
            },
        );
        continue;
    }
};
```

Leave healthcheck configuration as-is; `set_health_config` should naturally no-op for unresolved routes.

- [ ] **Step 4: Run apply config tests**

Run:

```bash
cargo test -p isengard-agent --test proxy_apply_config
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-agent/src/proxy/mod.rs crates/isengard-agent/tests/proxy_apply_config.rs
git commit -m "feat(agent): apply routes through ingress endpoints"
```

## Task 5: Real Docker Auto-Attach Regression

**Files:**
- Modify: `crates/isengard-agent/tests/proxy_label_discovery_e2e.rs`

- [ ] **Step 1: Add ignored Docker integration test**

Add a new ignored `#[tokio::test]` that:

1. Connects to Docker or returns early if unavailable.
2. Removes any stale test container named `isengard-e2e-autoattach-<pid>`.
3. Starts a `busybox` container on its default bridge with `isengard.expose=autoattach.test`.
4. Calls `BollardBackend::ensure_ingress_attachment(&container_name)`.
5. Re-inspects the container and asserts `NetworkSettings.Networks["isengard-proxy"].IPAddress` is non-empty.

Use the existing test's Docker setup/cleanup style. Keep the container command `sleep 30`.

- [ ] **Step 2: Run the ignored test locally**

Run:

```bash
cargo test -p isengard-agent --test proxy_label_discovery_e2e auto_attach_route_container_to_ingress_network -- --ignored --nocapture
```

Expected on a host with Docker: PASS. Expected without Docker: test returns early.

- [ ] **Step 3: Commit**

```bash
git add crates/isengard-agent/tests/proxy_label_discovery_e2e.rs
git commit -m "test(agent): cover docker ingress auto attach"
```

## Task 6: Documentation and Final Verification

**Files:**
- Modify: `docker/README.md`
- Modify: `docker/compose.yaml`
- Modify: `install/compose.yaml`

- [ ] **Step 1: Update docs wording**

Replace the “operator stacks opt in by attaching to `isengard-proxy`” sections with:

```markdown
The agent owns the `isengard-proxy` ingress network. When a route targets a
Docker bridge-networked container, the agent creates the network if needed,
attaches the target container, and routes to the IP on that network.

Operators do not need to edit Compose files just to make a routed service
reachable. Host-networked containers are routed through the Docker host
gateway. Containers using Docker `none` networking cannot be routed and show
an unresolved route reason.
```

Keep the verification command that inspects the network, but change it to say this is for diagnosis, not setup.

- [ ] **Step 2: Update compose comments**

In `docker/compose.yaml` and `install/compose.yaml`, change comments around `isengard-proxy` so they say the agent joins the ingress network and auto-attaches routed containers. Remove text implying every operator stack must declare it manually.

- [ ] **Step 3: Run verification**

Run:

```bash
cargo fmt --check
cargo test -p isengard-agent --lib
cargo test -p isengard-agent --test proxy_apply_config
```

Expected:

- `cargo fmt --check`: exit 0.
- `cargo test -p isengard-agent --lib`: all non-ignored tests pass.
- `cargo test -p isengard-agent --test proxy_apply_config`: all tests pass.

- [ ] **Step 4: Inspect debug leftovers**

Run:

```bash
rg -n "\\[DEBUG-|127\\.0\\.0\\.1.*fallback|falling back to 127" crates/isengard-agent/src crates/isengard-agent/tests docker install
```

Expected: no new debug markers and no remaining proxy localhost fallback message in the agent route apply path. Existing docs may mention `127.0.0.1` for published host ports; those are not failures.

- [ ] **Step 5: Commit docs and any cleanup**

```bash
git add docker/README.md docker/compose.yaml install/compose.yaml
git commit -m "docs: describe agent-owned ingress network"
```

## Self-Review

Spec coverage:

- Route networking reconciled on every route path: Task 4 moves reconciliation into `apply_config_with_backend`, which all pushed `ProxyConfig` rules use.
- Docker bridge/compose containers auto-attach: Task 2 implements, Task 5 verifies with real Docker.
- Host networking handled separately: Task 1 defines mode, Task 2 returns host gateway endpoints.
- Docker `none` unsupported: Task 1 reason enum, Task 2 unresolved behavior.
- No localhost fallback: Task 3 unresolved registry and Task 4 apply behavior.
- Observability: Task 4 logs resolved/unresolved state; Task 3 router returns reason-specific 503 context.
- Docs migration: Task 6.

Placeholder scan: no deferred implementation placeholders. The plan uses exact paths, commands, and code snippets for the major edits.

Type consistency: `IngressEndpoint`, `IngressEndpointMode`, `UnresolvedIngressReason`, `ContainerNetworkMode`, `RouteTarget`, and `UnresolvedUpstream` are defined before later tasks reference them.
