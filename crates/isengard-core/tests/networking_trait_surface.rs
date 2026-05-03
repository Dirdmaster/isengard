//! Compile-only test: exercises that NetworkingAdapter is object-safe and
//! that all the support types serde-roundtrip cleanly.

use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, Protocol, TlsStrategy,
};
use isengard_core::{HostMode, Plugin, PluginContext};

#[test]
fn expose_spec_roundtrips_json() {
    let spec = ExposeSpec {
        public_hostname: "blog.example.com".into(),
        local_listener_port: 8080,
        protocol: Protocol::Http,
        adapter_specific: serde_json::json!({"zone_id": "abc"}),
    };
    let json = serde_json::to_string(&spec).unwrap();
    let back: ExposeSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(back.public_hostname, "blog.example.com");
    assert_eq!(back.local_listener_port, 8080);
}

#[test]
fn networking_adapter_is_object_safe() {
    fn _accepts(_a: &dyn NetworkingAdapter) {}
}

#[test]
fn tls_strategy_edge_termination_serialises() {
    let s = TlsStrategy::EdgeTermination;
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(json, r#""edge_termination""#);
}
