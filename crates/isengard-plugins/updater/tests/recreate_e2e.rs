//! Integration test: pull `library/hello-world:latest`, run a labelled
//! container, force a cycle, verify the orchestration runs without panic.
//!
//! This test does NOT verify recreation END-TO-END (that requires the upstream
//! image to actually move, which we can't synthesise reliably without
//! testcontainers). What it verifies:
//!
//! 1. pull_image works against real Docker Hub
//! 2. update_container's inspect path works on a real running container
//! 3. The capture_config translation produces a Config that bollard accepts
//!    (i.e. round-trip: inspect → capture → recreate → start succeeds)
//!
//! Skipped if Docker isn't reachable.

#![allow(clippy::result_large_err)]

use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions};
use isengard_plugin_updater::image_ref::ImageRef;
use isengard_plugin_updater::recreate::{pull_image, update_container};

const TEST_IMAGE: &str = "hello-world:latest";
const TEST_CONTAINER: &str = "isengard-3c-test";

async fn docker_available() -> Option<Docker> {
    let docker = Docker::connect_with_local_defaults().ok()?;
    docker.version().await.ok()?;
    Some(docker)
}

async fn cleanup(docker: &Docker) {
    let _ = docker
        .remove_container(
            TEST_CONTAINER,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

#[tokio::test]
async fn pull_then_recreate_round_trip() {
    let Some(docker) = docker_available().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    cleanup(&docker).await;

    let image = ImageRef::parse(TEST_IMAGE).unwrap();

    // Pull the image.
    pull_image(&docker, &image)
        .await
        .expect("pull should succeed");

    // Create + start a labelled container against the pulled image.
    let mut labels = std::collections::HashMap::new();
    labels.insert("isengard.enable".to_string(), "true".to_string());
    let create = docker
        .create_container(
            Some(CreateContainerOptions {
                name: TEST_CONTAINER.to_string(),
                platform: None,
            }),
            Config {
                image: Some(TEST_IMAGE.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .expect("create should succeed");

    docker
        .start_container::<String>(&create.id, None)
        .await
        .expect("start should succeed");

    // hello-world exits immediately; that's fine. update_container will inspect
    // the (exited) container and recreate it.
    let result = update_container(&docker, &create.id, &image).await;

    cleanup(&docker).await;

    // The orchestration should not error on a hello-world recreate.
    if let Err(e) = result {
        panic!("update_container failed: {e}");
    }
}
