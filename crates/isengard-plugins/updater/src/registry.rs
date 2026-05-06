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
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue, LINK, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;

/// Phase 9e: hard cap on `tags/list` pagination so a misbehaving registry
/// can't OOM the agent. After this many pages, the loop logs a warning and
/// returns the tags collected so far.
const MAX_TAGS_LIST_PAGES: usize = 5;
/// Phase 9e: hard cap on the number of tags returned across all pages.
const MAX_TAGS_LIST_ENTRIES: usize = 5000;

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
        self.head_digest_with_scheme(image, "https").await
    }

    /// Test-friendly variant. Production callers must use [`head_digest`],
    /// which hardcodes `https`. The scheme override exists so wiremock
    /// (HTTP only by default) can exercise the minor-bump end-to-end test
    /// without TLS plumbing.
    #[doc(hidden)]
    pub async fn head_digest_with_scheme(
        &self,
        image: &ImageRef,
        scheme: &str,
    ) -> anyhow::Result<Option<String>> {
        let url = if scheme == "https" {
            image.manifest_url()
        } else {
            image
                .manifest_url()
                .replacen("https://", &format!("{scheme}://"), 1)
        };

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

    /// Phase 9e: list every tag the registry advertises for `image`'s
    /// repository. Used by the `Minor` update strategy to pick the highest
    /// semver tag whose major matches the running container.
    ///
    /// Auth dance mirrors `head_digest`: try anonymous, retry with the
    /// resolved bearer/Basic challenge on 401. Pagination follows the
    /// `Link: <...>; rel="next"` header (Docker registry v2 convention).
    /// Capped at [`MAX_TAGS_LIST_PAGES`] / [`MAX_TAGS_LIST_ENTRIES`] so a
    /// pathological registry can't run away with us.
    pub async fn list_tags(&self, image: &ImageRef) -> anyhow::Result<Vec<String>> {
        self.list_tags_with_scheme(image, "https").await
    }

    /// Test-friendly variant. Production callers must use [`list_tags`],
    /// which hardcodes `https`. The scheme override exists so wiremock
    /// (HTTP only by default) can exercise the pagination + auth paths
    /// without TLS plumbing.
    #[doc(hidden)]
    pub async fn list_tags_with_scheme(
        &self,
        image: &ImageRef,
        scheme: &str,
    ) -> anyhow::Result<Vec<String>> {
        // First page comes from the canonical tags-list URL; subsequent
        // pages come from the absolute or relative URL the registry hands
        // us in `Link`.
        let initial = {
            let canonical = image.tags_list_url();
            // Substitute the requested scheme. The default URL is
            // `https://...`. For tests we may be hitting a local http
            // mock.
            if scheme == "https" {
                canonical
            } else {
                canonical.replacen("https://", &format!("{scheme}://"), 1)
            }
        };
        let mut next_url = Some(initial.clone());
        let mut auth_header: Option<HeaderValue> = None;
        let mut out: Vec<String> = Vec::new();
        let creds = self.config.credentials_for(&image.registry);

        for page in 0..MAX_TAGS_LIST_PAGES {
            let Some(url) = next_url.take() else { break };

            // Build the request, attaching cached auth if we resolved it
            // on a previous page or from a 401 retry.
            let mut req = self.http.get(&url);
            if let Some(h) = &auth_header {
                req = req.header(AUTHORIZATION, h.clone());
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;

            let resp = match resp.status() {
                StatusCode::OK => resp,
                StatusCode::UNAUTHORIZED if auth_header.is_none() => {
                    // Resolve the challenge once, cache for subsequent pages.
                    let www_auth = resp
                        .headers()
                        .get(WWW_AUTHENTICATE)
                        .and_then(|h| h.to_str().ok())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "registry returned 401 without WWW-Authenticate for {url}"
                            )
                        })?
                        .to_string();
                    let h = self.resolve_auth_header(&www_auth, creds.as_ref()).await?;
                    auth_header = Some(h.clone());
                    let retry = self
                        .http
                        .get(&url)
                        .header(AUTHORIZATION, h)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("authed GET {url}: {e}"))?;
                    if !retry.status().is_success() {
                        return Err(anyhow::anyhow!(
                            "tags/list {url} returned {} after auth",
                            retry.status()
                        ));
                    }
                    retry
                }
                StatusCode::NOT_FOUND => {
                    // No such repo: return whatever we have (empty on the
                    // first iteration).
                    break;
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "tags/list {url} returned unexpected status {other}"
                    ));
                }
            };

            // Capture the next-page header BEFORE consuming the body.
            let link_header = resp
                .headers()
                .get(LINK)
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string());

            let body: TagsListResponse = resp
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("parsing tags/list body: {e}"))?;
            for tag in body.tags {
                if out.len() >= MAX_TAGS_LIST_ENTRIES {
                    tracing::warn!(
                        url = %url,
                        cap = MAX_TAGS_LIST_ENTRIES,
                        "tags/list cap reached; truncating"
                    );
                    return Ok(out);
                }
                out.push(tag);
            }

            // Resolve the next page URL, if any.
            if let Some(rel) = link_header.as_deref().and_then(parse_next_link) {
                next_url = Some(absolute_link_url(&initial, rel));
            }

            // Hard cap: if we've exhausted MAX_TAGS_LIST_PAGES iterations
            // and the registry is still pointing at next, bail with what we
            // have.
            if page + 1 == MAX_TAGS_LIST_PAGES && next_url.is_some() {
                tracing::warn!(
                    url = %initial,
                    cap = MAX_TAGS_LIST_PAGES,
                    "tags/list page cap reached; truncating"
                );
            }
        }

        Ok(out)
    }

    /// Translate a `WWW-Authenticate` value into an `Authorization` header
    /// (bearer token or basic). Used by the Phase 9e tags/list path; the
    /// per-request retry in `head_digest` predates this helper and stays
    /// inlined to keep the diff focused.
    async fn resolve_auth_header(
        &self,
        www_auth: &str,
        creds: Option<&(String, String)>,
    ) -> anyhow::Result<HeaderValue> {
        if let Some(challenge) = parse_bearer(www_auth) {
            let token = self.fetch_bearer_token(&challenge, creds).await?;
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| anyhow::anyhow!("bad bearer token: {e}"))
        } else if www_auth.to_ascii_lowercase().starts_with("basic") {
            let (u, p) = creds.ok_or_else(|| {
                anyhow::anyhow!("registry requires Basic auth but no credentials in docker config")
            })?;
            let blob = B64.encode(format!("{u}:{p}"));
            HeaderValue::from_str(&format!("Basic {blob}"))
                .map_err(|e| anyhow::anyhow!("bad basic header: {e}"))
        } else {
            Err(anyhow::anyhow!(
                "unsupported WWW-Authenticate scheme: {www_auth}"
            ))
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

/// Phase 9e: registry response shape for `/v2/<repo>/tags/list`.
#[derive(Debug, Deserialize)]
struct TagsListResponse {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

/// Parse the `<url>; rel="next"` form of a registry pagination Link header.
/// Returns the URL inside the angle brackets when `rel=next`, else `None`.
/// Conservative; bails on any unusual shape.
fn parse_next_link(header: &str) -> Option<&str> {
    // Header may carry multiple comma-separated values; we only care about
    // the one whose rel is `next`.
    for entry in header.split(',') {
        let entry = entry.trim();
        let (url_part, rel_part) = entry.split_once(';')?;
        let url = url_part.trim();
        if !url.starts_with('<') || !url.ends_with('>') {
            continue;
        }
        let url = &url[1..url.len() - 1];
        let rel = rel_part.trim();
        if rel.contains("rel=\"next\"") || rel.contains("rel=next") {
            return Some(url);
        }
    }
    None
}

/// Resolve a Link target against the request URL. Registries vary: some
/// emit absolute URLs, some emit `/v2/<repo>/tags/list?last=...`. We treat
/// `http://` / `https://` as absolute, otherwise we splice path+query onto
/// the original URL's scheme + host.
fn absolute_link_url(initial: &str, link: &str) -> String {
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_string();
    }
    // Strip path/query from `initial` to get scheme + host.
    let host_end = initial
        .find("://")
        .and_then(|i| initial[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(initial.len());
    let base = &initial[..host_end];
    if link.starts_with('/') {
        format!("{base}{link}")
    } else {
        format!("{base}/{link}")
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

    #[test]
    fn parse_next_link_extracts_url() {
        let h = r#"</v2/foo/bar/tags/list?last=v1.2.3>; rel="next""#;
        assert_eq!(
            parse_next_link(h),
            Some("/v2/foo/bar/tags/list?last=v1.2.3")
        );
    }

    #[test]
    fn parse_next_link_returns_none_when_no_next() {
        let h = r#"</v2/foo/bar>; rel="prev""#;
        assert!(parse_next_link(h).is_none());
    }

    #[test]
    fn absolute_link_url_handles_relative_path() {
        let abs = absolute_link_url(
            "https://ghcr.io/v2/foo/bar/tags/list",
            "/v2/foo/bar/tags/list?last=v1",
        );
        assert_eq!(abs, "https://ghcr.io/v2/foo/bar/tags/list?last=v1");
    }

    #[test]
    fn absolute_link_url_passes_absolute_through() {
        let abs = absolute_link_url(
            "https://ghcr.io/v2/foo/bar/tags/list",
            "https://other.example/path?x=1",
        );
        assert_eq!(abs, "https://other.example/path?x=1");
    }

    // ---------------- Phase 9e: list_tags HTTP integration ----------------
    //
    // Mocked-registry tests via `wiremock`. The fake server doesn't honor
    // the docker-content-digest header behaviors, but for tags/list we
    // only need the JSON body + Link header round-trip to exercise our
    // pagination + auth-retry paths.

    use crate::auth::DockerConfig;
    use crate::image_ref::ImageRef;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn imageref_for(host: &str) -> ImageRef {
        // Build an ImageRef whose `tags_list_url` points at the mock server.
        // We piggyback on the existing parser by stuffing the mock host
        // into the registry slot.
        ImageRef {
            registry: host.to_string(),
            repository: "foo/bar".into(),
            tag: "1.2.3".into(),
        }
    }

    fn make_client() -> RegistryClient {
        // Empty docker config: anonymous requests only. Sufficient for
        // a wiremock that doesn't challenge.
        let cfg = DockerConfig::default();
        RegistryClient::new(cfg).expect("registry client")
    }

    /// We can't hit the mock with `https://`, so override the URL builder
    /// for tests via a thin helper that calls list_tags through a custom
    /// URL. To keep the path minimal, we use `tags_list_url` and then
    /// rewrite the scheme via reqwest. wiremock's MockServer URL is
    /// `http://127.0.0.1:<port>`.
    ///
    /// Approach: build `ImageRef` with the registry set to a stub like
    /// `MOCK_HOST_PLACEHOLDER`, then fix up the host inside the test by
    /// constructing a custom `RegistryClient` that talks to the mock.
    /// Because the existing list_tags helper computes URLs from the
    /// ImageRef + uses https://, we use the simpler shape: feed it an
    /// ImageRef whose `registry` is the mock authority (host:port), and
    /// rely on reqwest's tls-or-not behavior. wiremock serves https when
    /// `MockServer::start_https()` is used; we use that.
    ///
    /// The tag pagination test uses `MockServer::start()` (http) so we
    /// can avoid TLS plumbing. We confirm parse_next_link + the
    /// aggregator works without the final HTTPS leg.
    #[tokio::test]
    async fn list_tags_returns_anonymous_response_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/foo/bar/tags/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "foo/bar",
                "tags": ["1.2.3", "1.2.4", "1.3.0"],
            })))
            .mount(&server)
            .await;

        let client = make_client();
        // Use the http-only test entry point so we skip TLS.
        let host = server.address().to_string();
        let image = imageref_for(&host);
        let tags = client
            .list_tags_with_scheme(&image, "http")
            .await
            .expect("list_tags");
        assert_eq!(
            tags,
            vec!["1.2.3".to_string(), "1.2.4".into(), "1.3.0".into()]
        );
    }

    #[tokio::test]
    async fn list_tags_aggregates_paginated_pages() {
        let server = MockServer::start().await;
        // Page 1: returns Link: <...?last=1.2.4>; rel="next"
        // Wiremock matches on path + query; force this mock to respond
        // only when the `last` param is absent (page 1).
        Mock::given(method("GET"))
            .and(path("/v2/foo/bar/tags/list"))
            .and(wiremock::matchers::query_param_is_missing("last"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Link", r#"</v2/foo/bar/tags/list?last=1.2.4>; rel="next""#)
                    .set_body_json(serde_json::json!({
                        "name": "foo/bar",
                        "tags": ["1.2.3", "1.2.4"],
                    })),
            )
            .mount(&server)
            .await;

        // Page 2 (no further Link, terminates the loop).
        Mock::given(method("GET"))
            .and(path("/v2/foo/bar/tags/list"))
            .and(wiremock::matchers::query_param("last", "1.2.4"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"name": "foo/bar", "tags": ["1.3.0"]})),
            )
            .mount(&server)
            .await;

        let client = make_client();
        let host = server.address().to_string();
        let image = imageref_for(&host);
        let tags = client
            .list_tags_with_scheme(&image, "http")
            .await
            .expect("list_tags");
        assert_eq!(
            tags,
            vec!["1.2.3".to_string(), "1.2.4".into(), "1.3.0".into()]
        );
    }
}
