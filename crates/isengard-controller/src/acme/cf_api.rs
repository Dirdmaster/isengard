//! Cloudflare v4 API client for DNS-01 ACME challenges.
//!
//! Distinct from the (similarly-shaped) plugin client at
//! `crates/isengard-plugins/networking-cf-tunnel/src/api.rs` because the two
//! live in different crates and have different scopes: the CF Tunnel plugin
//! manages CNAMEs for the tunnel ingress; this module manages TXT records
//! for `_acme-challenge.<host>` plus zone discovery. Sharing the type was
//! considered and rejected: the plugin requires `cf-tunnel` cargo feature,
//! the controller's ACME path is unconditional. Two ~200-line clients beats
//! one cross-crate dependency mess.
//!
//! Auth: Cloudflare API token with the `Zone:DNS:Edit` permission scoped to
//! the zone(s) you care about. Tokens with broader scopes work too.
//!
//! Timeouts: 30s per request, generous enough to ride out brief CF slowness
//! without letting a hung TCP connection stall the renewal scheduler.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";
const CF_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Thin client for the subset of CF v4 API the DNS-01 flow uses:
/// zone lookup, TXT record create, TXT record delete.
pub struct CloudflareApi {
    client: reqwest::Client,
    api_token: String,
    base_url: String,
}

impl CloudflareApi {
    pub fn new(api_token: String) -> Self {
        Self {
            client: build_client(),
            api_token,
            base_url: CF_API_BASE.to_string(),
        }
    }

    /// Test-only: override the API base URL (for wiremock).
    pub fn with_base_url(api_token: String, base_url: String) -> Self {
        Self {
            client: build_client(),
            api_token,
            base_url,
        }
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.api_token)
    }

    /// Find the zone whose name is the longest suffix of `domain`. CF
    /// requires the bare zone (e.g. `vallee.casa`), not a subdomain. Strips
    /// a leading `*.` if present (wildcards live under the apex zone).
    pub async fn find_zone_for_domain(&self, domain: &str) -> Result<Zone> {
        let bare = domain.trim_start_matches("*.");
        // CF's /zones?name=... requires an exact match. Walk the suffixes:
        // for `foo.bar.example.com` try `foo.bar.example.com`, then
        // `bar.example.com`, then `example.com`. First hit wins.
        let mut current = bare;
        loop {
            if let Some(zone) = self.zones_by_name(current).await? {
                return Ok(zone);
            }
            match current.split_once('.') {
                Some((_, rest)) if rest.contains('.') => current = rest,
                _ => return Err(anyhow!("no Cloudflare zone found for domain {domain}")),
            }
        }
    }

    async fn zones_by_name(&self, name: &str) -> Result<Option<Zone>> {
        let url = format!("{}/zones?name={}", self.base_url, name);
        let resp: CfResponse<Vec<Zone>> = self
            .auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| anyhow!("CF zones GET: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("CF zones JSON: {e}"))?;
        let mut zones = resp.into_result()?;
        Ok(zones.pop())
    }

    /// Create a TXT record at `name` (FQDN, e.g. `_acme-challenge.foo.com`)
    /// with body `content`. Returns the new record's id so the caller can
    /// `delete_dns_record` it after validation.
    pub async fn create_txt_record(
        &self,
        zone_id: &str,
        name: &str,
        content: &str,
    ) -> Result<String> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone_id);
        // TTL = 60s (CF minimum that doesn't mean "auto"). LE validates almost
        // immediately so the record's lifetime is dominated by the delete-
        // after-validation step, not by TTL expiry.
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "content": content,
            "ttl": 60,
        });
        let resp: CfResponse<DnsRecordCreated> = self
            .auth(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| anyhow!("CF create TXT: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("CF create TXT JSON: {e}"))?;
        Ok(resp.into_result()?.id)
    }

    /// Delete a DNS record by id. Idempotent on the CF side: deleting an
    /// already-deleted record returns success (we accept either; no special
    /// handling needed here).
    pub async fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone_id, record_id
        );
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| anyhow!("CF delete DNS: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("CF delete DNS JSON: {e}"))?;
        resp.into_result().map(|_| ())
    }

    /// Verify a token is valid by calling `/user/tokens/verify`. Used at
    /// boot to fail fast with a clear message rather than failing later
    /// inside the order flow.
    pub async fn verify_token(&self) -> Result<()> {
        let url = format!("{}/user/tokens/verify", self.base_url);
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| anyhow!("CF verify_token: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("CF verify_token JSON: {e}"))?;
        resp.into_result().map(|_| ())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct DnsRecordCreated {
    id: String,
}

#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: Option<T>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CfError {
    code: i64,
    message: String,
}

impl<T> CfResponse<T> {
    fn into_result(self) -> Result<T> {
        if self.success {
            self.result
                .ok_or_else(|| anyhow!("CF response missing `result` field"))
        } else {
            let msg = self
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow!("CF API error: {msg}"))
        }
    }
}

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(CF_API_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok<T: Serialize>(result: T) -> serde_json::Value {
        serde_json::json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": result,
        })
    }

    fn err_response(code: i64, message: &str) -> serde_json::Value {
        serde_json::json!({
            "success": false,
            "errors": [{ "code": code, "message": message }],
            "messages": [],
            "result": null,
        })
    }

    #[tokio::test]
    async fn find_zone_walks_suffixes() {
        let server = MockServer::start().await;

        // First two queries miss, the apex hits.
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "deep.sub.vallee.casa"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok::<Vec<Zone>>(vec![])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "sub.vallee.casa"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok::<Vec<Zone>>(vec![])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "vallee.casa"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok(vec![Zone {
                id: "zone-id".into(),
                name: "vallee.casa".into(),
            }])))
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("test-token".into(), server.uri());
        let zone = api
            .find_zone_for_domain("deep.sub.vallee.casa")
            .await
            .unwrap();
        assert_eq!(zone.id, "zone-id");
        assert_eq!(zone.name, "vallee.casa");
    }

    #[tokio::test]
    async fn find_zone_strips_wildcard_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "vallee.casa"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok(vec![Zone {
                id: "z".into(),
                name: "vallee.casa".into(),
            }])))
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("t".into(), server.uri());
        let zone = api.find_zone_for_domain("*.vallee.casa").await.unwrap();
        assert_eq!(zone.name, "vallee.casa");
    }

    #[tokio::test]
    async fn find_zone_no_match_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok::<Vec<Zone>>(vec![])))
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("t".into(), server.uri());
        let err = api
            .find_zone_for_domain("foo.example.com")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no Cloudflare zone"));
    }

    #[tokio::test]
    async fn create_txt_record_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zones/zone-id/dns_records"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok(serde_json::json!({
                    "id": "record-abc",
                    "type": "TXT",
                    "name": "_acme-challenge.vallee.casa",
                }))),
            )
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("t".into(), server.uri());
        let id = api
            .create_txt_record("zone-id", "_acme-challenge.vallee.casa", "challenge-value")
            .await
            .unwrap();
        assert_eq!(id, "record-abc");
    }

    #[tokio::test]
    async fn create_txt_record_propagates_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zones/zone-id/dns_records"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(err_response(9103, "Invalid token")),
            )
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("bad".into(), server.uri());
        let err = api
            .create_txt_record("zone-id", "_acme-challenge.x.com", "v")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("9103"));
        assert!(err.to_string().contains("Invalid token"));
    }

    #[tokio::test]
    async fn delete_dns_record_ok() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/zones/z/dns_records/r$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok(serde_json::json!({
                    "id": "r"
                }))),
            )
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("t".into(), server.uri());
        api.delete_dns_record("z", "r").await.unwrap();
    }

    #[tokio::test]
    async fn verify_token_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok(serde_json::json!({
                    "id": "tok",
                    "status": "active",
                }))),
            )
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("t".into(), server.uri());
        api.verify_token().await.unwrap();
    }

    #[tokio::test]
    async fn verify_token_rejects_bad_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(err_response(1000, "Unauthorized")),
            )
            .mount(&server)
            .await;

        let api = CloudflareApi::with_base_url("bad".into(), server.uri());
        let err = api.verify_token().await.unwrap_err();
        assert!(err.to_string().contains("1000"));
    }
}
