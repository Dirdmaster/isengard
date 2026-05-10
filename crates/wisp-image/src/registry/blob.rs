//! Blob fetch with digest verification.
//!
//! `fetch_to_store` is the single network-touching helper used by the
//! orchestrator for both layer and config blobs. It streams the
//! response body straight into `ContentStore::write_blob_streaming`,
//! which already handles sha256 hashing, expected-digest verification,
//! and atomic-rename cleanup on mismatch.
//!
//! On 401 the function bubbles the error up unchanged: the orchestrator
//! is responsible for the auth dance because tokens are scoped per
//! `(repo, action)` and the orchestrator owns that context.
//!
//! Streaming choice: `reqwest::blocking::Response` implements `io::Read`
//! directly (verified against reqwest 0.12.28 source), so we feed the
//! response into the store without buffering the whole body. This keeps
//! peak memory bounded by the 64KB chunk buffer in
//! `write_blob_streaming` regardless of layer size.

use crate::error::WispImageError;
use crate::store::ContentStore;

/// Fetch the blob at `url` and persist it to `store`. Returns the
/// canonical `sha256:<hex>` digest, which equals `expected_digest` when
/// caller passes one. On digest mismatch the partial tempfile is
/// dropped (via `write_blob_streaming`'s rollback path) and the error
/// surfaces as `WispImageError::Parse`.
pub fn fetch_to_store(
    http: &reqwest::blocking::Client,
    url: &str,
    bearer_token: Option<&str>,
    expected_digest: Option<&str>,
    store: &ContentStore,
) -> Result<String, WispImageError> {
    let mut req = http.get(url);
    if let Some(token) = bearer_token {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .map_err(|e| WispImageError::Network(format!("GET {url}: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        // Surface 401 as a Network error too: the orchestrator inspects
        // the message string to decide whether to re-auth, but the
        // caller can also just inspect WWW-Authenticate via a separate
        // probe if it needs the challenge details. In Phase 0.2 the
        // orchestrator does the dance pre-emptively (POST to /v2/ then
        // attach the token to every subsequent request), so this path
        // is the "token expired mid-pull" tail case.
        let body = resp.text().unwrap_or_else(|_| String::from("<no body>"));
        let snippet = clip(&body, 1024);
        return Err(WispImageError::Network(format!(
            "GET {url} returned {status}: {snippet}"
        )));
    }

    // Stream straight into the store. Memory is bounded by the
    // 64KB buffer inside write_blob_streaming.
    let digest = store.write_blob_streaming(resp, expected_digest)?;
    Ok(digest)
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn store() -> (TempDir, ContentStore) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ContentStore::new(tmp.path()).expect("store::new");
        (tmp, store)
    }

    fn sha256_digest(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }

    #[tokio::test]
    async fn fetch_to_store_streams_to_store() {
        let server = MockServer::start().await;
        let body = b"hello world";
        Mock::given(method("GET"))
            .and(path("/blobs/foo"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/blobs/foo", server.uri());
        let want = sha256_digest(body);

        let (_tmp, st) = store();
        let st_clone = st.clone();
        let got = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            fetch_to_store(&http, &url, None, None, &st_clone)
        })
        .await
        .expect("join")
        .expect("fetch");
        assert_eq!(got, want);
        assert!(st.has_blob(&got));
    }

    #[tokio::test]
    async fn fetch_to_store_verifies_digest() {
        let server = MockServer::start().await;
        let body = b"verified payload";
        Mock::given(method("GET"))
            .and(path("/blobs/v"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/blobs/v", server.uri());
        let expected = sha256_digest(body);
        let (_tmp, st) = store();
        let st_clone = st.clone();
        let exp_clone = expected.clone();
        let got = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            fetch_to_store(&http, &url, None, Some(&exp_clone), &st_clone)
        })
        .await
        .expect("join")
        .expect("fetch");
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn fetch_to_store_rejects_digest_mismatch() {
        let server = MockServer::start().await;
        let body = b"mismatch payload";
        Mock::given(method("GET"))
            .and(path("/blobs/m"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/blobs/m", server.uri());
        let bogus = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let (_tmp, st) = store();
        let st_clone = st.clone();
        let res = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            fetch_to_store(&http, &url, None, Some(bogus), &st_clone)
        })
        .await
        .expect("join");
        assert!(res.is_err(), "expected mismatch error");
        // The tempfile must have been cleaned up: blob dir empty.
        let blob_dir = crate::store::layout::sha256_dir(st.root());
        let count = std::fs::read_dir(&blob_dir).unwrap().count();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn fetch_to_store_handles_404_with_clear_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/blobs/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found in registry"))
            .mount(&server)
            .await;

        let url = format!("{}/blobs/missing", server.uri());
        let (_tmp, st) = store();
        let st_clone = st.clone();
        let url_clone = url.clone();
        let err = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            fetch_to_store(&http, &url_clone, None, None, &st_clone)
        })
        .await
        .expect("join")
        .expect_err("404 should error");
        let msg = format!("{err}");
        assert!(msg.contains("404"), "missing status: {msg}");
        assert!(msg.contains(&url), "missing url: {msg}");
        assert!(msg.contains("not found"), "missing body snippet: {msg}");
    }

    #[tokio::test]
    async fn fetch_to_store_passes_bearer_token() {
        let server = MockServer::start().await;
        let body = b"authed";
        Mock::given(method("GET"))
            .and(path("/blobs/auth"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/blobs/auth", server.uri());
        let (_tmp, st) = store();
        let st_clone = st.clone();
        let res = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            fetch_to_store(&http, &url, Some("secret-token"), None, &st_clone)
        })
        .await
        .expect("join");
        assert!(res.is_ok(), "bearer-auth fetch failed: {res:?}");
    }
}
