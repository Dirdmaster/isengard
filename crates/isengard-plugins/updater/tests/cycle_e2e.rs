//! Full-cycle e2e: pull two real nginx versions, label one as the "current"
//! version of the other (creating a stale local digest), run an in-process
//! updater cycle, verify the container was recreated against the real digest.
//!
//! This is the most expensive test in the suite — pulls two ~10MB images and
//! does a real recreate. Skips if Docker isn't available.

#![allow(clippy::result_large_err)]

use std::time::Duration;

use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions};
use bollard::image::{CreateImageOptions, TagImageOptions};
use futures_util::StreamExt;
use isengard_core::{AgentPlugin, HostMode, Plugin, PluginContext};
use isengard_plugin_updater::Updater;

const OLD_VERSION: &str = "nginx:1.24-alpine";
const NEW_TAG: &str = "nginx:1.25-alpine";
const TEST_CONTAINER: &str = "isengard-3e-cycle";

async fn docker_available() -> Option<Docker> {
    let docker = Docker::connect_with_local_defaults().ok()?;
    docker.version().await.ok()?;
    Some(docker)
}

async fn pull(docker: &Docker, image: &str) -> anyhow::Result<()> {
    let (repo, tag) = image.split_once(':').unwrap_or((image, "latest"));
    let opts = CreateImageOptions::<String> {
        from_image: repo.to_string(),
        tag: tag.to_string(),
        ..Default::default()
    };
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(frame) = stream.next().await {
        if let Err(e) = frame {
            return Err(anyhow::anyhow!("pull {image}: {e}"));
        }
    }
    Ok(())
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
async fn cycle_recreates_stale_container_against_real_registry() {
    let Some(docker) = docker_available().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    cleanup(&docker).await;

    // 1. Pull both versions.
    pull(&docker, OLD_VERSION).await.expect("pull 1.24-alpine");
    pull(&docker, NEW_TAG).await.expect("pull 1.25-alpine");

    // 2. Re-tag 1.24 as 1.25 — this creates the "stale" condition: local
    //    nginx:1.25-alpine now points at 1.24's contents, but the registry
    //    still has the real 1.25. The updater should detect this and pull.
    let (new_repo, new_tag_part) = NEW_TAG.split_once(':').unwrap();
    docker
        .tag_image(
            OLD_VERSION,
            Some(TagImageOptions {
                repo: new_repo,
                tag: new_tag_part,
            }),
        )
        .await
        .expect("retag 1.24 as 1.25");

    // 3. Inspect the local 1.25 to confirm the swap took effect.
    let stale_inspect = docker
        .inspect_image(NEW_TAG)
        .await
        .expect("inspect stale 1.25");
    let stale_id = stale_inspect.id.clone().expect("stale image has ID");

    // 4. Run a labelled container against the (now stale) 1.25 tag.
    let mut labels = std::collections::HashMap::new();
    labels.insert("isengard.enable".to_string(), "true".to_string());
    let create = docker
        .create_container(
            Some(CreateContainerOptions::<String> {
                name: TEST_CONTAINER.to_string(),
                platform: None,
            }),
            Config {
                image: Some(NEW_TAG.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .expect("create container");
    docker
        .start_container::<String>(&create.id, None)
        .await
        .expect("start container");

    // 5. Construct an Updater in-process and run one cycle.
    let mut plugin = Updater::new();
    let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);
    plugin.init(&ctx).await.expect("plugin init");
    plugin.start(&ctx).await.expect("plugin start");

    // run_cycle is the AgentPlugin entry point — synchronous one-cycle run
    // that doesn't depend on the internal ticker.
    plugin
        .run_cycle(&ctx)
        .await
        .expect("run_cycle should succeed");

    // Give the recreate path a moment to settle. run_cycle awaits the full
    // recreate, so there should be no race, but Docker's eventual-consistency
    // on inspect can lag.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 6. Inspect the test container — its image should now be the REAL 1.25.
    let updated_inspect = docker
        .inspect_container(TEST_CONTAINER, None)
        .await
        .expect("inspect after update");
    let new_image_id = updated_inspect.image.expect("container has image");

    assert_ne!(
        new_image_id, stale_id,
        "container image should have changed after update; old={stale_id} new={new_image_id}"
    );

    // 7. Cleanup
    plugin.stop().await.expect("plugin stop");
    cleanup(&docker).await;
    // Re-tag pull to restore real 1.25 (defensive; the local repo is shared
    // with the host's docker engine).
    let _ = pull(&docker, NEW_TAG).await;
}
