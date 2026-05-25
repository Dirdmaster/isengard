//! Task 13: agent-side `apply_config` for `ProxyConfig` pushed by the
//! controller. Covers the happy-path swap and the stale-generation drop.

use std::collections::HashMap;
use std::pin::Pin;

use futures_util::Stream;
use isengard_proto::pb::{Healthcheck, ProxyConfig, RoutingRule, TlsMode, Upstream};

#[derive(Debug, Default)]
struct MockBackend {
    endpoints: HashMap<String, isengard_agent::runtime::IngressEndpoint>,
}

#[async_trait::async_trait]
impl isengard_agent::runtime::RuntimeBackend for MockBackend {
    async fn ensure_image(
        &self,
        _reference: &str,
    ) -> Result<String, isengard_agent::runtime::RuntimeError> {
        Ok(String::new())
    }

    async fn create_container(
        &self,
        _spec: &isengard_agent::runtime::ContainerCreateSpec,
    ) -> Result<String, isengard_agent::runtime::RuntimeError> {
        Ok(String::new())
    }

    async fn start_container(
        &self,
        _id: &str,
    ) -> Result<(), isengard_agent::runtime::RuntimeError> {
        Ok(())
    }

    async fn stop_container(
        &self,
        _id: &str,
        _timeout_s: u32,
    ) -> Result<(), isengard_agent::runtime::RuntimeError> {
        Ok(())
    }

    async fn remove_container(
        &self,
        _id: &str,
        _force: bool,
    ) -> Result<(), isengard_agent::runtime::RuntimeError> {
        Ok(())
    }

    async fn list_containers(
        &self,
        _filter: isengard_agent::runtime::ListFilter,
    ) -> Result<
        Vec<isengard_agent::runtime::ContainerSnapshot>,
        isengard_agent::runtime::RuntimeError,
    > {
        Ok(Vec::new())
    }

    async fn inspect_container(
        &self,
        _id: &str,
    ) -> Result<
        Option<isengard_agent::runtime::ContainerSnapshot>,
        isengard_agent::runtime::RuntimeError,
    > {
        Ok(None)
    }

    async fn connect_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> Result<(), isengard_agent::runtime::RuntimeError> {
        Ok(())
    }

    async fn disconnect_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> Result<(), isengard_agent::runtime::RuntimeError> {
        Ok(())
    }

    async fn ensure_ingress_attachment(
        &self,
        container_ref: &str,
    ) -> Result<isengard_agent::runtime::IngressEndpoint, isengard_agent::runtime::RuntimeError>
    {
        Ok(self.endpoints.get(container_ref).cloned().unwrap_or(
            isengard_agent::runtime::IngressEndpoint::Unresolved(
                isengard_agent::runtime::UnresolvedIngressReason::ContainerMissing,
            ),
        ))
    }

    fn stream_logs(
        &self,
        _id: &str,
        _opts: isengard_agent::runtime::LogOptions,
    ) -> Pin<Box<dyn Stream<Item = isengard_agent::runtime::LogChunk> + Send>> {
        Box::pin(futures_util::stream::empty())
    }

    fn stream_events(
        &self,
    ) -> Pin<Box<dyn Stream<Item = isengard_agent::runtime::RuntimeEvent> + Send>> {
        Box::pin(futures_util::stream::empty())
    }

    async fn run_healthcheck(
        &self,
        _id: &str,
        _hc: &isengard_agent::runtime::HealthcheckSpec,
    ) -> Result<isengard_agent::runtime::HealthState, isengard_agent::runtime::RuntimeError> {
        Ok(isengard_agent::runtime::HealthState::Healthy)
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

fn cfg_with_empty_ip(
    generation: u64,
    container_id: &str,
    container_port: u32,
    public_hostname: &str,
) -> ProxyConfig {
    ProxyConfig {
        host_id: "h".into(),
        generation,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: public_hostname.into(),
            upstream: Some(Upstream {
                container_id: container_id.into(),
                container_ip: String::new(),
                container_port,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: None,
            adapter: "none".into(),
        }],
        settings: None,
        wildcard_certs: vec![],
    }
}

#[tokio::test]
async fn apply_config_replaces_upstream_registry() {
    let state = isengard_agent::proxy::ProxyState::new();

    let cfg = ProxyConfig {
        host_id: "h-1".into(),
        generation: 1,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: "a.test".into(),
            upstream: Some(Upstream {
                container_id: "web".into(),
                container_ip: "127.0.0.1".into(),
                container_port: 8080,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: Some(Healthcheck {
                path: "/healthz".into(),
                interval_secs: 10,
            }),
            adapter: "none".into(),
        }],
        settings: None,
        wildcard_certs: vec![],
    };

    isengard_agent::proxy::apply_config(&state, cfg)
        .await
        .unwrap();

    let up = state.upstreams.read().await;
    let got = up.get("a.test").expect("rule applied");
    assert_eq!(got.container_id, "web");
    assert_eq!(got.addr.port(), 8080);
}

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

#[tokio::test]
async fn stale_generation_is_ignored() {
    let state = isengard_agent::proxy::ProxyState::new();

    let mk = |generation: u64, port: u16| ProxyConfig {
        host_id: "h".into(),
        generation,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: "x.test".into(),
            upstream: Some(Upstream {
                container_id: "web".into(),
                container_ip: "127.0.0.1".into(),
                container_port: port as u32,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: None,
            adapter: "none".into(),
        }],
        settings: None,
        wildcard_certs: vec![],
    };

    isengard_agent::proxy::apply_config(&state, mk(5, 8080))
        .await
        .unwrap();
    // older generation should be ignored
    isengard_agent::proxy::apply_config(&state, mk(3, 9999))
        .await
        .unwrap();

    let up = state.upstreams.read().await;
    assert_eq!(up.get("x.test").unwrap().addr.port(), 8080);
}

/// Regression test for the apply_config generation race fix.
///
/// Without the in-lock re-check (introduced after slice-4 review on PR #18),
/// two concurrent `apply_config(N)` and `apply_config(N+1)` calls could both
/// pass the lock-free pre-check, then race to acquire the registry write
/// lock: and whichever arrived SECOND would win, even with the older
/// generation. The current implementation re-checks `last_generation` under
/// the lock and drops the older config if it lost the race.
///
/// We verify the OUTCOME across many trials: regardless of arrival order,
/// the higher-generation config's rules end up installed.
#[tokio::test]
async fn concurrent_applies_higher_generation_wins() {
    let mk = |generation: u64, port: u16| ProxyConfig {
        host_id: "h".into(),
        generation,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: "x.test".into(),
            upstream: Some(Upstream {
                container_id: "web".into(),
                container_ip: "127.0.0.1".into(),
                container_port: port as u32,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: None,
            adapter: "none".into(),
        }],
        settings: None,
        wildcard_certs: vec![],
    };

    for trial in 0..50 {
        let state = isengard_agent::proxy::ProxyState::new();
        let (lo, hi) = (mk(1, 8080), mk(2, 9090));

        // Spawn both concurrently. Outcome: higher generation always wins.
        let (r_lo, r_hi) = tokio::join!(
            isengard_agent::proxy::apply_config(&state, lo),
            isengard_agent::proxy::apply_config(&state, hi),
        );
        r_lo.unwrap();
        r_hi.unwrap();

        let up = state.upstreams.read().await;
        let got = up.get("x.test").expect("rule applied");
        assert_eq!(
            got.addr.port(),
            9090,
            "trial {trial}: higher generation should win, got port {}",
            got.addr.port()
        );
    }
}

/// Regression test for the controller-restart bug.
///
/// The controller's per-host `generation` counter lives in memory
/// (`routing::by_host`) and resets to 0 on controller restart. The agent's
/// `last_generation` keeps its prior high value across the agent's sync
/// reconnect: so the very first push from the freshly-restarted controller
/// (generation=1) would be discarded as "stale" by the agent's monotonicity
/// check, leaving the agent serving the old config forever.
///
/// `run_sync_loop` resets `last_generation` to 0 on every fresh sync stream
/// open. After the reset, any positive generation (including 1, the first
/// push from a fresh controller) is accepted. This test mimics the reset
/// directly so it runs without spinning up a real controller.
#[tokio::test]
async fn last_generation_reset_on_stream_open_unblocks_low_gen_push() {
    use std::sync::atomic::Ordering;

    let state = isengard_agent::proxy::ProxyState::new();

    let mk = |generation: u64, port: u16, host: &str| ProxyConfig {
        host_id: "h".into(),
        generation,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: host.into(),
            upstream: Some(Upstream {
                container_id: "web".into(),
                container_ip: "127.0.0.1".into(),
                container_port: port as u32,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: None,
            adapter: "none".into(),
        }],
        settings: None,
        wildcard_certs: vec![],
    };

    // Simulate the agent having processed several pushes from the previous
    // controller incarnation: generation climbs to 5.
    isengard_agent::proxy::apply_config(&state, mk(5, 8080, "old.test"))
        .await
        .unwrap();
    assert_eq!(state.last_generation.load(Ordering::Acquire), 5);

    // Without the reset, the next line (a fresh post-restart push at
    // generation=1) would be dropped as stale: agent stays serving old.test.
    state.last_generation.store(0, Ordering::Release);

    // Fresh controller's first push: generation=1.
    isengard_agent::proxy::apply_config(&state, mk(1, 9090, "new.test"))
        .await
        .unwrap();

    let up = state.upstreams.read().await;
    let got = up.get("new.test").expect("post-reset push must apply");
    assert_eq!(got.addr.port(), 9090);
    assert!(
        up.get("old.test").is_none(),
        "registry should fully replace, not merge"
    );
    assert_eq!(state.last_generation.load(Ordering::Acquire), 1);
}
