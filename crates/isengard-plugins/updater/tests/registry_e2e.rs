//! Integration test: hit the real Docker Hub registry for `library/hello-world:latest`,
//! expect a sha256 digest. Skips if the network is unreachable.
//!
//! `hello-world` is the canonical "tiniest public image" and is extremely
//! unlikely to disappear, so this test is a stable smoke for the bearer-token
//! flow against `auth.docker.io`.

#![allow(clippy::result_large_err)]

use isengard_plugin_updater::{auth::DockerConfig, image_ref::ImageRef, registry::RegistryClient};

async fn network_reachable() -> bool {
    // Try a tiny request against auth.docker.io. If DNS fails or we can't
    // reach the host, treat as offline and skip.
    reqwest::Client::new()
        .head("https://auth.docker.io")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .is_ok()
}

#[tokio::test]
async fn head_digest_for_hello_world_returns_sha256() {
    if !network_reachable().await {
        eprintln!("skipping: network not reachable");
        return;
    }

    let client = RegistryClient::new(DockerConfig::default()).unwrap();
    let image = ImageRef::parse("library/hello-world:latest").unwrap();

    let digest = client
        .head_digest(&image)
        .await
        .expect("registry HEAD should succeed");

    let digest = digest.expect("hello-world:latest should exist");
    assert!(
        digest.starts_with("sha256:"),
        "expected sha256 digest, got: {digest}"
    );
    assert!(
        digest.len() > "sha256:".len() + 32,
        "digest looks truncated: {digest}"
    );
}
