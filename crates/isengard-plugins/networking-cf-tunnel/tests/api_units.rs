use isengard_plugin_networking_cf_tunnel::api::{CfApi, IngressRule};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn create_tunnel_posts_body_and_returns_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/accounts/acct-1/cfd_tunnel"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "errors": [],
            "result": {
                "id": "tnl-123",
                "name": "isengard-test",
                "token": "tunnel-secret"
            }
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("test-token".into(), server.uri());
    let created = api.create_tunnel("acct-1", "isengard-test").await.unwrap();
    assert_eq!(created.id, "tnl-123");
    assert_eq!(created.token, "tunnel-secret");
}

#[tokio::test]
async fn cf_error_response_is_propagated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/accounts/acct-1/cfd_tunnel"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "success": false,
            "errors": [{"code": 1009, "message": "Tunnel name already exists"}],
            "result": null
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("t".into(), server.uri());
    let err = api
        .create_tunnel("acct-1", "isengard-test")
        .await
        .unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("1009"), "{s}");
    assert!(s.contains("Tunnel name already exists"), "{s}");
}

#[tokio::test]
async fn set_ingress_sends_correct_body() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/accounts/acct-1/cfd_tunnel/tnl-123/configurations"))
        .and(body_json(serde_json::json!({
            "config": {
                "ingress": [
                    {"hostname": "blog.example.com", "service": "http://localhost:8080"},
                    {"hostname": null, "service": "http_status:404"}
                ]
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "errors": [], "result": {}
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("t".into(), server.uri());
    api.set_ingress(
        "acct-1",
        "tnl-123",
        vec![
            IngressRule {
                hostname: Some("blog.example.com".into()),
                service: "http://localhost:8080".into(),
            },
            IngressRule {
                hostname: None,
                service: "http_status:404".into(),
            },
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn upsert_dns_cname_returns_record_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/zones/zone-1/dns_records"))
        .and(body_json(serde_json::json!({
            "type": "CNAME",
            "name": "blog.example.com",
            "content": "tnl-123.cfargotunnel.com",
            "proxied": true,
            "ttl": 1,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "errors": [],
            "result": { "id": "dns-record-1", "name": "blog.example.com" }
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("t".into(), server.uri());
    let r = api
        .upsert_dns_cname("zone-1", "blog.example.com", "tnl-123.cfargotunnel.com")
        .await
        .unwrap();
    assert_eq!(r.id, "dns-record-1");
}
