//! HTTP HEAD against a registry's manifest endpoint, returns the
//! `Docker-Content-Digest` header.
//!
//! Auth flow:
//!   1. HEAD the manifest URL with no auth.
//!      2a. 200 → done, read `Docker-Content-Digest`.
//!      2b. 401 → parse `WWW-Authenticate`. Two flavours:
//!       - `Basic ...` → retry with Basic creds from DockerConfig.
//!       - `Bearer realm=...,service=...,scope=...` → fetch a bearer token
//!         from the realm (with optional Basic creds for private repos),
//!         retry HEAD with `Authorization: Bearer <token>`.
//!         2c. 404 → `Ok(None)` (image/tag genuinely doesn't exist).
//!         2d. anything else → Err.
//!
//! Manifest media types we accept (newest first; the registry picks):
//!   - application/vnd.oci.image.index.v1+json
//!   - application/vnd.oci.image.manifest.v1+json
//!   - application/vnd.docker.distribution.manifest.list.v2+json
//!   - application/vnd.docker.distribution.manifest.v2+json

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;

const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

pub struct RegistryClient {
    http: Client,
    config: DockerConfig,
}

impl RegistryClient {
    pub fn new(config: DockerConfig) -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("isengard/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| anyhow::anyhow!("building http client: {e}"))?;
        Ok(Self { http, config })
    }

    /// Returns the remote manifest digest, or `None` if the tag doesn't exist.
    pub async fn head_digest(&self, image: &ImageRef) -> anyhow::Result<Option<String>> {
        let url = image.manifest_url();

        // Initial unauthenticated HEAD.
        let resp = self
            .http
            .head(&url)
            .header(ACCEPT, ACCEPT_MANIFESTS)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HEAD {url}: {e}"))?;

        match resp.status() {
            StatusCode::OK => return Ok(extract_digest(resp.headers())),
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::UNAUTHORIZED => { /* fall through to auth */ }
            other => {
                return Err(anyhow::anyhow!(
                    "unexpected status from registry: {other} for {url}"
                ));
            }
        }

        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("registry returned 401 without WWW-Authenticate for {url}")
            })?;

        let creds = self.config.credentials_for(&image.registry);

        let auth_header = if let Some(challenge) = parse_bearer(www_auth) {
            let token = self.fetch_bearer_token(&challenge, creds.as_ref()).await?;
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| anyhow::anyhow!("bad bearer token: {e}"))?
        } else if www_auth.to_ascii_lowercase().starts_with("basic") {
            let (u, p) = creds.as_ref().ok_or_else(|| {
                anyhow::anyhow!("registry requires Basic auth but no credentials in docker config")
            })?;
            let blob = B64.encode(format!("{u}:{p}"));
            HeaderValue::from_str(&format!("Basic {blob}"))
                .map_err(|e| anyhow::anyhow!("bad basic header: {e}"))?
        } else {
            return Err(anyhow::anyhow!(
                "unsupported WWW-Authenticate scheme: {www_auth}"
            ));
        };

        let resp = self
            .http
            .head(&url)
            .header(ACCEPT, ACCEPT_MANIFESTS)
            .header(AUTHORIZATION, auth_header)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("authed HEAD {url}: {e}"))?;

        match resp.status() {
            StatusCode::OK => Ok(extract_digest(resp.headers())),
            StatusCode::NOT_FOUND => Ok(None),
            other => Err(anyhow::anyhow!(
                "registry returned {other} after auth for {url}"
            )),
        }
    }

    async fn fetch_bearer_token(
        &self,
        challenge: &BearerChallenge,
        creds: Option<&(String, String)>,
    ) -> anyhow::Result<String> {
        let mut req = self.http.get(&challenge.realm);
        if let Some(service) = &challenge.service {
            req = req.query(&[("service", service.as_str())]);
        }
        if let Some(scope) = &challenge.scope {
            req = req.query(&[("scope", scope.as_str())]);
        }
        if let Some((u, p)) = creds {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("bearer token fetch from {}: {e}", challenge.realm))?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "bearer endpoint {} returned {}",
                challenge.realm,
                resp.status()
            ));
        }

        let body: TokenResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing bearer token response: {e}"))?;

        // Some registries return `token`, some `access_token`.
        body.token.or(body.access_token).ok_or_else(|| {
            anyhow::anyhow!("bearer token response had neither token nor access_token")
        })
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_bearer(www_auth: &str) -> Option<BearerChallenge> {
    let trimmed = www_auth.trim();
    if !trimmed.to_ascii_lowercase().starts_with("bearer ") {
        return None;
    }
    let rest = &trimmed["bearer ".len()..];

    let mut realm = None;
    let mut service = None;
    let mut scope = None;

    for part in split_kv(rest) {
        match part {
            ("realm", v) => realm = Some(v.to_string()),
            ("service", v) => service = Some(v.to_string()),
            ("scope", v) => scope = Some(v.to_string()),
            _ => {}
        }
    }

    realm.map(|r| BearerChallenge {
        realm: r,
        service,
        scope,
    })
}

/// Split `realm="x",service="y",scope="z"` honoring quoted values.
#[allow(clippy::while_let_on_iterator)]
fn split_kv(input: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_whitespace() || c == ',' {
            continue;
        }
        // Find '='
        let key_start = i;
        while let Some(&(_, ch)) = chars.peek() {
            if ch == '=' {
                break;
            }
            chars.next();
        }
        let key_end = chars.peek().map(|&(j, _)| j).unwrap_or(input.len());
        let key = &input[key_start..key_end];
        // Consume '='
        chars.next();
        // Value: quoted or until ','
        let val = if let Some(&(_, '"')) = chars.peek() {
            chars.next();
            let val_start = chars.peek().map(|&(j, _)| j).unwrap_or(input.len());
            let mut val_end = val_start;
            while let Some((j, ch)) = chars.next() {
                if ch == '"' {
                    val_end = j;
                    break;
                }
            }
            &input[val_start..val_end]
        } else {
            let val_start = chars.peek().map(|&(j, _)| j).unwrap_or(input.len());
            let mut val_end = input.len();
            while let Some(&(j, ch)) = chars.peek() {
                if ch == ',' {
                    val_end = j;
                    break;
                }
                chars.next();
            }
            &input[val_start..val_end]
        };
        out.push((key, val));
    }
    out
}

fn extract_digest(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("docker-content-digest")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dockerhub_bearer_challenge() {
        let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/hello-world:pull""#;
        let c = parse_bearer(h).unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(
            c.scope.as_deref(),
            Some("repository:library/hello-world:pull")
        );
    }

    #[test]
    fn parses_realm_only_bearer() {
        let h = r#"Bearer realm="https://example.com/token""#;
        let c = parse_bearer(h).unwrap();
        assert_eq!(c.realm, "https://example.com/token");
        assert!(c.service.is_none());
        assert!(c.scope.is_none());
    }

    #[test]
    fn returns_none_for_non_bearer_scheme() {
        assert!(parse_bearer("Basic realm=\"x\"").is_none());
    }
}
