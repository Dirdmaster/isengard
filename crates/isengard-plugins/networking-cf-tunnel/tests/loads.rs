use isengard_core::networking::{NetworkingAdapter, TlsStrategy};
use isengard_plugin_networking_cf_tunnel::CfTunnelAdapter;

#[test]
fn id_is_cf_tunnel() {
    assert_eq!(CfTunnelAdapter.id(), "cf-tunnel");
}

#[test]
fn tls_strategy_is_edge_termination() {
    assert!(matches!(
        CfTunnelAdapter.tls_strategy(),
        TlsStrategy::EdgeTermination
    ));
}
