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
