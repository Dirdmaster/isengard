use isengard_core::networking::{NetworkingAdapter, TlsStrategy};
use isengard_plugin_networking_none::NoneAdapter;

#[test]
fn id_is_none() {
    let a = NoneAdapter;
    assert_eq!(a.id(), "none");
}

#[test]
fn tls_strategy_is_pass_through() {
    let a = NoneAdapter;
    assert!(matches!(a.tls_strategy(), TlsStrategy::PassThrough));
}

#[tokio::test]
async fn expose_returns_err() {
    use isengard_core::context::{HostMode, PluginContext};
    use isengard_core::networking::{AdapterContext, ExposeSpec, Protocol};

    let a = NoneAdapter;
    let ctx = AdapterContext {
        host_id: "h-1".into(),
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
    assert!(format!("{err}").contains("no adapter"));
}
