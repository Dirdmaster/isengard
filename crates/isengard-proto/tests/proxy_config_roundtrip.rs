use isengard_proto::pb::{
    Healthcheck, ProxyConfig, ProxySettings, RoutingRule, TlsMode, Upstream, WildcardCert,
};
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
        wildcard_certs: vec![],
    };

    let bytes = cfg.encode_to_vec();
    let back = ProxyConfig::decode(&*bytes).unwrap();
    assert_eq!(back.generation, 7);
    assert_eq!(back.rules.len(), 1);
    assert_eq!(back.rules[0].public_hostname, "blog.example.com");
    assert!(back.wildcard_certs.is_empty());
}

#[test]
fn proxy_config_carries_wildcard_certs() {
    let cfg = ProxyConfig {
        host_id: "h".into(),
        generation: 1,
        rules: vec![],
        settings: None,
        wildcard_certs: vec![WildcardCert {
            identifiers: vec!["*.vallee.casa".into(), "vallee.casa".into()],
            cert_pem: "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n".into(),
            key_pem: "-----BEGIN PRIVATE KEY-----\nFAKE\n-----END PRIVATE KEY-----\n".into(),
        }],
    };

    let bytes = cfg.encode_to_vec();
    let back = ProxyConfig::decode(&*bytes).unwrap();
    assert_eq!(back.wildcard_certs.len(), 1);
    assert_eq!(
        back.wildcard_certs[0].identifiers,
        vec!["*.vallee.casa".to_string(), "vallee.casa".to_string(),]
    );
    assert!(
        back.wildcard_certs[0]
            .cert_pem
            .contains("BEGIN CERTIFICATE")
    );
}
