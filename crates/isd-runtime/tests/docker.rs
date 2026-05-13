//! DockerBackend integration tests. Local backend ping is gated
//! `#[ignore]` because it requires a running docker daemon (Docker
//! Desktop / dockerd / orbstack) which the CI shells do not always
//! have. Run with `cargo test -p isd-runtime --test docker -- --ignored`
//! when validating against a real engine.

use isd_runtime::DockerBackend;

#[tokio::test]
#[ignore]
async fn from_local_connects_and_pings() {
    let backend = DockerBackend::from_local().expect("local backend");
    let info = backend.ping().await.expect("ping");
    assert!(!info.is_empty(), "ping should return a non-empty response");
}

#[tokio::test]
async fn from_uri_rejects_invalid_scheme() {
    let result = DockerBackend::from_uri("ftp://example.com").await;
    let err = result.expect_err("ftp:// is not a valid docker endpoint");
    let message = format!("{err}");
    assert!(
        message.contains("ssh://"),
        "error message should hint at valid schemes: {message}"
    );
}
