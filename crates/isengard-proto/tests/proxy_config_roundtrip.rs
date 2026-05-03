use isengard_proto::pb::{Healthcheck, ProxyConfig, ProxySettings, RoutingRule, TlsMode, Upstream};
use prost::Message;

#[test]
fn proxy_config_encodes_and_decodes() {
    let cfg = ProxyConfig {
        host_id: "h-1".into(),
        generation: 7,
        rules: vec![RoutingRule {
            id: 42,
            public_hostname: "blog.example.com".into(),
            upstream: Some(Upstream {
                container_id: "cid".into(),
                container_ip: "10.0.0.5".into(),
                container_port: 8080,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: Some(Healthcheck {
                path: "/healthz".into(),
                interval_secs: 10,
            }),
            adapter: "none".into(),
        }],
        settings: Some(ProxySettings {
            http_port: 8080,
            https_port: 8443,
            acme_contact_email: "ops@example.com".into(),
            log_sample_rate: 0.05,
        }),
    };

    let bytes = cfg.encode_to_vec();
    let back = ProxyConfig::decode(&*bytes).unwrap();
    assert_eq!(back.generation, 7);
    assert_eq!(back.rules.len(), 1);
    assert_eq!(back.rules[0].public_hostname, "blog.example.com");
}
