//! Integration: construct Updater, init against the host's Docker
//! daemon, start, wait briefly, stop cleanly.
//!
//! Skips if Docker is not available on the host (e.g. CI without Docker-in-Docker).

#![allow(clippy::result_large_err)]

use std::time::Duration;

use bollard::Docker;
use isengard_core::{HostMode, Plugin, PluginContext};
use isengard_plugin_updater::Updater;

async fn docker_available() -> bool {
    match Docker::connect_with_local_defaults() {
        Ok(d) => d.version().await.is_ok(),
        Err(_) => false,
    }
}

#[tokio::test]
async fn updater_lifecycle_against_real_docker() {
    if !docker_available().await {
        eprintln!("skipping: Docker daemon not reachable");
        return;
    }

    let mut plugin = Updater::new();
    let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);

    plugin
        .init(&ctx)
        .await
        .expect("init should succeed against real Docker");
    plugin.start(&ctx).await.expect("start should succeed");

    // Cycle interval is 30s. Test would be too slow waiting for the natural
    // tick. Instead, verify the plugin lifecycles cleanly across
    // start/stop without waiting for a tick.
    tokio::time::sleep(Duration::from_millis(200)).await;

    plugin.stop().await.expect("stop should succeed");
}
