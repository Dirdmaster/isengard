//! Thin client for the subset of CF v4 API we use.

use isengard_core::error::{CoreError, Result};
use serde::{Deserialize, Serialize};

const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Per-request timeout. CF's API typically responds in <1s; 30s is generous
/// enough to ride out brief slowness without letting the agent's caller
/// (`expose` / `unexpose`) hang indefinitely on a stuck connection. Without
/// this, `reqwest::Client::new()`'s default of "never" would let a half-open
/// TCP connection stall the whole flow.
const CF_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct CfApi {
    client: reqwest::Client,
    api_token: String,
    base_url: String,
}

impl CfApi {
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

#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct CfError {
    code: i64,
    message: String,
}

impl<T> CfResponse<T> {
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

#[derive(Debug, Deserialize)]
pub struct TunnelCreated {
    pub id: String,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct DnsRecordCreated {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct DnsRecordSummary {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IngressRule {
    pub hostname: Option<String>,
    pub service: String,
}

/// Build a reqwest client with the per-request timeout applied. Falls back
/// to the default client if builder fails (in practice never — `build()`
/// only fails on TLS init issues that would also break the default client).
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(CF_API_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}
