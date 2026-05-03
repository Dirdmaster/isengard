use isengard_core::networking::{NetworkingAdapter, TlsStrategy};
use isengard_plugin_networking_tailscale::TailscaleAdapter;

#[test]
fn id_is_tailscale() {
    let a = TailscaleAdapter;
    assert_eq!(a.id(), "tailscale");
}

#[test]
fn tls_strategy_is_adapter_provided() {
    let a = TailscaleAdapter;
    assert!(matches!(a.tls_strategy(), TlsStrategy::AdapterProvided));
}

#[tokio::test]
async fn expose_unimplemented_yet_returns_clear_error() {
    use isengard_core::context::{HostMode, PluginContext};
    use isengard_core::networking::{AdapterContext, ExposeSpec, Protocol};

    let a = TailscaleAdapter;
    let ctx = AdapterContext {
        host_id: "h".into(),
        settings: serde_json::Value::Null,
        plugin_ctx: PluginContext::new(HostMode::Agent, serde_json::Value::Null),
    };
    let spec = ExposeSpec {
        public_hostname: "x".into(),
        local_listener_port: 8080,
        protocol: Protocol::Http,
        adapter_specific: serde_json::Value::Null,
    };
    let err = a.expose(&ctx, &spec).await.unwrap_err();
    assert!(format!("{err}").contains("8f-2"));
}
