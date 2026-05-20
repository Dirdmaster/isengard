//! Integration tests for [`isd_runtime::DockerBackend`].
//!
//! The local-backend smoke tests are gated `#[ignore]` because they
//! require a running docker daemon (Docker Desktop, dockerd, orbstack)
//! which the CI shells do not always have. Run with
//! `cargo test -p isd-runtime --test docker -- --ignored` when
//! validating against a real engine.

use isd_runtime::DockerBackend;

/// `from_local` connects to the running daemon and returns a non-empty
/// version string from `ping`.
#[tokio::test]
#[ignore]
async fn from_local_connects_and_pings() {
    let backend = DockerBackend::from_local().expect("local backend");
    let info = backend.ping().await.expect("ping");
    assert!(!info.is_empty(), "ping should return a non-empty response");
}

/// `from_uri` rejects any scheme outside `ssh://`, `unix://`, and the
/// literal `local`. The error names the valid schemes.
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

/// `list_containers` against the local daemon succeeds and yields
/// well-formed rows. Does not assert on count: a clean dev machine may
/// have zero running containers.
#[tokio::test]
#[ignore]
async fn list_containers_against_local_daemon() {
    let backend = DockerBackend::from_local().expect("local backend");
    let containers = backend
        .list_containers(true)
        .await
        .expect("list containers");
    for c in &containers {
        assert!(!c.id.is_empty(), "every container has an id");
    }
}
