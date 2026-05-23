//! Cloudflare zone discovery for `isd configure`.
//!
//! Distinct from [`crate::acme::cf_api`] (which manages TXT records for
//! DNS-01) and from the cf-tunnel plugin's client (which manages CNAMEs).
//! Both of those are tightly coupled to a specific feature path; this
//! module is the operator-facing zone enumerator behind the `routing.zones`
//! "Fetch from Cloudflare" button.
//!
//! Single public surface: [`list_zones`] returns every zone name the
//! supplied API token can see, paginated up to a safety cap. The HTTP
//! base URL is overridable for tests via [`list_zones_with_base`].

use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::time::Duration;

/// Production CF v4 API base URL.
const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";
/// Per-request timeout. Matches the ACME path's 30s ceiling.
const CF_API_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size on `/zones`. CF supports up to 50 per request.
const PAGE_SIZE: usize = 50;
/// Maximum number of pages we walk. 5 * 50 = 250 zones, comfortably
/// above any sane operator's fleet without giving a misconfigured token
/// (or a CF outage) the chance to spin us forever.
const MAX_PAGES: usize = 5;

/// Returns every zone name visible to `token`, in CF's response order.
///
/// Walks the paginated `/zones` endpoint until either the server signals
/// the last page or we hit `MAX_PAGES`. Filters strictly to the
/// `result[].name` strings: zone ids, account info, plan tier, etc. are
/// dropped. Empty list is a valid result (token has no zones yet).
///
/// # Errors
///
/// Returns `Err` on transport failure, non-success HTTP status (with the
/// server's body in the message so the dashboard can surface the
/// upstream hint), or a malformed CF response envelope.
pub async fn list_zones(token: &str) -> Result<Vec<String>> {
    list_zones_with_base(token, CF_API_BASE).await
}

/// Test-only entry point: same as [`list_zones`] but uses `base_url`
/// instead of the public CF endpoint. The dashboard does not call this
/// directly; the wiremock-backed unit tests do.
pub async fn list_zones_with_base(token: &str, base_url: &str) -> Result<Vec<String>> {
    if token.is_empty() {
        return Err(anyhow!("cloudflare token is empty"));
    }
    let client = reqwest::Client::builder()
        .timeout(CF_API_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut out: Vec<String> = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!("{base_url}/zones?per_page={PAGE_SIZE}&page={page}");
        let resp = client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| anyhow!("CF zones GET: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("CF zones GET {status}: {body}"));
        }
        let envelope: CfZonesEnvelope = resp
            .json()
            .await
            .map_err(|e| anyhow!("CF zones JSON: {e}"))?;
        if !envelope.success {
            let msg = envelope
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!("CF API error: {msg}"));
        }
        let result = envelope.result.unwrap_or_default();
        let page_len = result.len();
        for z in result {
            out.push(z.name);
        }
        // Stop when CF reports we're caught up, or the page was short
        // (CF returns < per_page on the last page). Also stop on an
        // empty page so we never loop on a token with zero zones.
        let info = envelope.result_info.unwrap_or_default();
        if info.total_pages > 0 && page >= info.total_pages {
            break;
        }
        if page_len < PAGE_SIZE {
            break;
        }
    }
    Ok(out)
}

/// CF v4 paginated envelope. Only the fields we need.
#[derive(Debug, Deserialize)]
struct CfZonesEnvelope {
    /// True on a successful response.
    success: bool,
    /// Empty on success; populated on failure with the per-error
    /// `{code, message}` pairs.
    #[serde(default)]
    errors: Vec<CfError>,
    /// The zone list. `None` when `success == false`.
    result: Option<Vec<Zone>>,
    /// Pagination footer. CF omits this on error responses.
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

/// One zone row from `/zones`.
#[derive(Debug, Deserialize)]
struct Zone {
    /// Zone name (e.g. `weavers.engineering`).
    name: String,
}

/// CF error code + message pair.
#[derive(Debug, Deserialize)]
struct CfError {
    /// Numeric CF error code.
    code: i64,
    /// Human-readable description.
    message: String,
}

/// Pagination footer in the CF envelope.
#[derive(Debug, Default, Deserialize)]
struct ResultInfo {
    /// Total number of pages for the current `per_page`.
    #[serde(default)]
    total_pages: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: build the JSON envelope CF returns for a `/zones` page.
    fn ok_page(zones: &[&str], total_pages: usize) -> serde_json::Value {
        let result: Vec<serde_json::Value> = zones
            .iter()
            .map(|n| json!({ "id": "zid", "name": n }))
            .collect();
        json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": result,
            "result_info": { "total_pages": total_pages },
        })
    }

    #[tokio::test]
    async fn list_zones_returns_names_on_single_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("page", "1"))
            .and(header("authorization", "Bearer cf-token-test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(ok_page(&["weavers.engineering", "vallee.casa"], 1)),
            )
            .mount(&server)
            .await;

        let zones = list_zones_with_base("cf-token-test", &server.uri())
            .await
            .unwrap();
        assert_eq!(zones, vec!["weavers.engineering", "vallee.casa"]);
    }

    #[tokio::test]
    async fn list_zones_walks_pages_until_total() {
        let server = MockServer::start().await;
        // Two pages. Page 1 returns 50 zones, page 2 returns 1 zone.
        let page1: Vec<String> = (0..50).map(|i| format!("zone{i}.example")).collect();
        let page1_refs: Vec<&str> = page1.iter().map(String::as_str).collect();
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_page(&page1_refs, 2)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_page(&["last.example"], 2)))
            .mount(&server)
            .await;

        let zones = list_zones_with_base("t", &server.uri()).await.unwrap();
        assert_eq!(zones.len(), 51);
        assert_eq!(zones.last().map(String::as_str), Some("last.example"));
    }

    #[tokio::test]
    async fn list_zones_stops_on_short_page() {
        // Without `total_pages` we infer end-of-stream when the page
        // returns fewer than PAGE_SIZE rows.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_page(&["one.example"], 0)))
            .mount(&server)
            .await;
        let zones = list_zones_with_base("t", &server.uri()).await.unwrap();
        assert_eq!(zones, vec!["one.example"]);
    }

    #[tokio::test]
    async fn list_zones_rejects_empty_token() {
        let err = list_zones("").await.unwrap_err();
        assert!(format!("{err}").contains("empty"));
    }

    #[tokio::test]
    async fn list_zones_surfaces_cf_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false,
                "errors": [{ "code": 6003, "message": "Invalid request headers" }],
                "messages": [],
                "result": null,
            })))
            .mount(&server)
            .await;
        let err = list_zones_with_base("bad-token", &server.uri())
            .await
            .unwrap_err();
        let msg = format!("{err}");
        // 403 trips the HTTP-status branch so the upstream body comes
        // through verbatim in the message.
        assert!(msg.contains("403"), "msg: {msg}");
        assert!(msg.contains("Invalid request headers"), "msg: {msg}");
    }

    #[tokio::test]
    async fn list_zones_surfaces_envelope_error_on_200() {
        // CF can return success: false with status 200 in some
        // configurations. Make sure we still propagate the message.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "errors": [{ "code": 1000, "message": "Account suspended" }],
                "result": null,
            })))
            .mount(&server)
            .await;
        let err = list_zones_with_base("t", &server.uri()).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Account suspended"), "msg: {msg}");
        assert!(msg.contains("1000"), "msg: {msg}");
    }
}
