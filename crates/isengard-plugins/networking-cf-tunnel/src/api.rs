//! Thin client for the subset of the Cloudflare v4 REST API the adapter uses.
//!
//! Wraps `reqwest::Client` with bearer-auth, a fixed 30s per-request
//! timeout, and a uniform `Result<T>` shape. Every method posts to or
//! reads from a URL of the form `<base>/accounts/<id>/...` or
//! `<base>/zones/<id>/...` and decodes through the [`CfResponse`]
//! envelope.

use isengard_core::error::{CoreError, Result};
use serde::{Deserialize, Serialize};

/// Default base URL for Cloudflare's v4 API.
const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Per-request timeout for every CF call.
///
/// CF typically responds in under a second; 30s rides out brief slowness
/// without letting an `expose` or `unexpose` hang on a stuck TCP
/// connection. Without an explicit timeout `reqwest::Client::new()`
/// would default to "never" and a half-open connection would stall the
/// whole flow.
const CF_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Cloudflare API client.
///
/// Holds an authed `reqwest::Client` and the resolved base URL. Cheap
/// to construct: a new one per `expose` / `unexpose` call is fine.
pub struct CfApi {
    /// HTTP client with the per-request timeout applied.
    client: reqwest::Client,
    /// Bearer token supplied to every request.
    api_token: String,
    /// Resolved API base URL. Production points at [`CF_API_BASE`];
    /// tests override via [`CfApi::with_base_url`].
    base_url: String,
}

impl CfApi {
    /// Builds a client against the public Cloudflare API.
    pub fn new(api_token: String) -> Self {
        Self {
            client: build_client(),
            api_token,
            base_url: CF_API_BASE.to_string(),
        }
    }

    /// Test-only constructor that overrides the API base URL (e.g.
    /// against wiremock).
    pub fn with_base_url(api_token: String, base_url: String) -> Self {
        Self {
            client: build_client(),
            api_token,
            base_url,
        }
    }

    /// Attaches the bearer token to a request builder.
    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.api_token)
    }

    /// Creates a new Cloudflare tunnel.
    ///
    /// Returns the new tunnel's id, display name, and connection token.
    /// The caller persists `token` and feeds it to
    /// [`crate::cloudflared::supervise`] so subsequent agent restarts
    /// can reattach without recreating the tunnel.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when the HTTP call fails, when the
    /// JSON body can't be decoded, or when Cloudflare reports
    /// `success: false`.
    pub async fn create_tunnel(&self, account_id: &str, name: &str) -> Result<TunnelCreated> {
        let url = format!("{}/accounts/{}/cfd_tunnel", self.base_url, account_id);
        let body = serde_json::json!({
            "name": name,
            "config_src": "cloudflare",
        });
        let resp: CfResponse<TunnelCreated> = self
            .auth(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF create_tunnel: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF create_tunnel JSON: {e}")))?;
        resp.into_result()
    }

    /// Deletes a Cloudflare tunnel.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when the HTTP call fails, when the
    /// JSON body can't be decoded, or when Cloudflare reports
    /// `success: false`.
    pub async fn delete_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}",
            self.base_url, account_id, tunnel_id
        );
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete_tunnel: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete_tunnel JSON: {e}")))?;
        resp.into_result().map(|_| ())
    }

    /// Replaces the tunnel's ingress rules.
    ///
    /// CF's `configurations` PUT is whole-document: the supplied
    /// `ingress` vector becomes the new ingress, dropping anything that
    /// was there before. The adapter always passes a single
    /// hostname-bound rule followed by a `http_status:404` catch-all.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when the HTTP call fails, when the
    /// JSON body can't be decoded, or when Cloudflare reports
    /// `success: false`.
    pub async fn set_ingress(
        &self,
        account_id: &str,
        tunnel_id: &str,
        ingress: Vec<IngressRule>,
    ) -> Result<()> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}/configurations",
            self.base_url, account_id, tunnel_id
        );
        let body = serde_json::json!({
            "config": { "ingress": ingress },
        });
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.put(&url).json(&body))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF set_ingress: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF set_ingress JSON: {e}")))?;
        resp.into_result().map(|_| ())
    }

    /// Creates a proxied CNAME record pointing `hostname` at `target`.
    ///
    /// Returns the new record's id and resolved name. CF's create-DNS
    /// API replaces nothing implicitly; the caller should
    /// [`Self::list_dns_records`] + [`Self::delete_dns_record`] to
    /// remove stale rows when reconciling.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when the HTTP call fails, when the
    /// JSON body can't be decoded, or when Cloudflare reports
    /// `success: false`.
    pub async fn upsert_dns_cname(
        &self,
        zone_id: &str,
        hostname: &str,
        target: &str,
    ) -> Result<DnsRecordCreated> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone_id);
        let body = serde_json::json!({
            "type": "CNAME",
            "name": hostname,
            "content": target,
            "proxied": true,
            "ttl": 1,
        });
        let resp: CfResponse<DnsRecordCreated> = self
            .auth(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF dns_records: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF dns_records JSON: {e}")))?;
        resp.into_result()
    }

    /// Lists DNS records in `zone_id` matching the exact `name`.
    ///
    /// Returned rows carry only the record id and resolved name. The
    /// adapter uses this to enumerate the records it needs to delete
    /// during `unexpose`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when the HTTP call fails, when the
    /// JSON body can't be decoded, or when Cloudflare reports
    /// `success: false`.
    pub async fn list_dns_records(
        &self,
        zone_id: &str,
        name: &str,
    ) -> Result<Vec<DnsRecordSummary>> {
        let url = format!(
            "{}/zones/{}/dns_records?name={}",
            self.base_url, zone_id, name
        );
        let resp: CfResponse<Vec<DnsRecordSummary>> = self
            .auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF list dns: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF list dns JSON: {e}")))?;
        resp.into_result()
    }

    /// Deletes a DNS record by id.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when the HTTP call fails, when the
    /// JSON body can't be decoded, or when Cloudflare reports
    /// `success: false`.
    pub async fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone_id, record_id
        );
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete dns: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete dns JSON: {e}")))?;
        resp.into_result().map(|_| ())
    }
}

/// Standard Cloudflare API response envelope.
///
/// Every CF endpoint wraps its payload in `{ success, errors, result }`.
/// This type peels the envelope and surfaces a flat `Result<T>` for
/// callers.
#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    /// CF's success flag. When false the `errors` array carries the
    /// reason and `result` is typically null.
    success: bool,
    /// Error descriptors populated when `success` is false.
    #[serde(default)]
    errors: Vec<CfError>,
    /// Decoded payload. Present when the call succeeded.
    result: Option<T>,
}

/// One entry from a Cloudflare API error array.
#[derive(Debug, Deserialize)]
struct CfError {
    /// Numeric CF error code.
    code: i64,
    /// Human-readable error message.
    message: String,
}

impl<T> CfResponse<T> {
    /// Flattens the envelope into a `Result<T>`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when `success` is false (with every
    /// CF error code and message joined into the message) or when
    /// `success` is true but `result` is missing.
    fn into_result(self) -> Result<T> {
        if self.success {
            self.result
                .ok_or_else(|| CoreError::Other("CF response missing `result` field".into()))
        } else {
            let msg = self
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join(", ");
            Err(CoreError::Other(format!("CF API error: {msg}")))
        }
    }
}

/// Decoded result of [`CfApi::create_tunnel`].
#[derive(Debug, Deserialize)]
pub struct TunnelCreated {
    /// New tunnel UUID.
    pub id: String,
    /// Operator-visible tunnel name.
    pub name: String,
    /// `cloudflared` connection token. Treat as a secret.
    pub token: String,
}

/// Decoded result of [`CfApi::upsert_dns_cname`].
#[derive(Debug, Deserialize)]
pub struct DnsRecordCreated {
    /// New DNS record UUID.
    pub id: String,
    /// Resolved record name (typically the requested hostname).
    pub name: String,
}

/// Compact DNS record row used by [`CfApi::list_dns_records`].
#[derive(Debug, Deserialize)]
pub struct DnsRecordSummary {
    /// DNS record UUID.
    pub id: String,
    /// Resolved record name.
    pub name: String,
}

/// One ingress rule on a Cloudflare tunnel.
///
/// Rules are evaluated in order. The last rule must always be a
/// catch-all with `hostname = None`; CF rejects configurations that
/// don't end in one.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IngressRule {
    /// Hostname this rule matches. `None` makes the rule a catch-all.
    pub hostname: Option<String>,
    /// Upstream service URL or special token like `http_status:404`.
    pub service: String,
}

/// Builds a `reqwest::Client` with [`CF_API_TIMEOUT`] applied.
///
/// Falls back to a default client when the builder fails. In practice
/// the builder only fails on TLS init issues that would also break the
/// default client.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(CF_API_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}
