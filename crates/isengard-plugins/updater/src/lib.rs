//! Isengard `updater` plugin.
//!
//! Watches running Docker containers and (in later sub-phases) keeps their
//! images up to date. Phase 3a: just lists containers on a schedule.

#![allow(clippy::result_large_err)]

#[allow(dead_code)]
mod image_ref;

#[allow(dead_code)]
mod labels;

#[allow(dead_code)]
mod auth;

#[allow(dead_code)]
mod registry;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::ListContainersOptions;
use isengard_core::{
    AgentPlugin, Capability, CoreError, Plugin, PluginContext, PluginRegistration, Result,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const PLUGIN_NAME: &str = "updater";
const CYCLE_INTERVAL: Duration = Duration::from_secs(30);

pub struct Updater {
    /// Lazily set in `init`. Wrapped in Option so the struct can be constructed
    /// by the inventory factory before init runs.
    docker: Option<Docker>,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            cancel: Arc::new(Notify::new()),
            task: None,
        }
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

// --- error-wrapping helpers --------------------------------------------------
// The Plugin trait returns `isengard_core::Result<()>` which is
// `Result<(), CoreError>`. Bollard / anyhow errors don't auto-coerce, so we
// wrap them per-lifecycle-stage. Phase 3b can refactor if a `From` impl lands
// in isengard-core.

fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Run one cycle of work. Phase 3a: list running containers, log count + names.
async fn do_cycle(docker: &Docker) -> anyhow::Result<()> {
    let opts = ListContainersOptions::<String> {
        all: false, // running only
        ..Default::default()
    };
    let containers = docker
        .list_containers(Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("listing containers: {e}"))?;

    info!(
        count = containers.len(),
        "updater cycle: containers running"
    );

    for c in &containers {
        let name = c
            .names
            .as_ref()
            .and_then(|ns| ns.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unknown>".into());
        let image = c.image.as_deref().unwrap_or("<unknown>");
        debug!(container = %name, image = %image, "running");
    }

    Ok(())
}

#[async_trait]
impl Plugin for Updater {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> {
        // Connect to the local Docker daemon. Honors DOCKER_HOST + standard
        // socket paths.
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| init_err(format!("connecting to docker daemon: {e}")))?;

        // Verify connectivity early — a bad daemon URL or missing socket
        // should fail init, not silently break the cycle.
        let version = docker
            .version()
            .await
            .map_err(|e| init_err(format!("docker version probe failed: {e}")))?;
        info!(
            api_version = %version.api_version.as_deref().unwrap_or("?"),
            engine_version = %version.version.as_deref().unwrap_or("?"),
            "updater connected to docker daemon"
        );

        self.docker = Some(docker);
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let cancel = self.cancel.clone();

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CYCLE_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancel.notified() => {
                        debug!("updater cycle task cancelled");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = do_cycle(&docker).await {
                            // Don't crash the task on a single bad cycle; just log
                            // and try again next tick. Phase 3b adds retry policy.
                            warn!(error = %e, "updater cycle failed");
                        }
                    }
                }
            }
        });

        self.task = Some(task);
        info!("updater started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.cancel.notify_waiters();
        if let Some(task) = self.task.take() {
            // Give the task a moment to exit cleanly; if it hangs, abort.
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Ok(())) => debug!("updater cycle task ended cleanly"),
                Ok(Err(e)) => warn!(error = %e, "updater cycle task panicked"),
                Err(_) => warn!("updater cycle task timed out on stop"),
            }
        }
        info!("updater stopped");
        Ok(())
    }
}

#[async_trait]
impl AgentPlugin for Updater {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        // External-invocation entry point. Phase 3a: same as the internal task.
        // Phase 3e+: controller-triggered "force update now" lands here.
        let docker = self
            .docker
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        do_cycle(docker)
            .await
            .map_err(|e| init_err(format!("cycle failed: {e}")))
    }
}

inventory::submit! {
    PluginRegistration {
        name: "updater",
        capabilities: &[Capability::Agent],
        constructor: || Box::new(Updater::new()) as Box<dyn Plugin>,
    }
}

// Compile-time assertion: Updater must remain Send + Sync because the
// inventory factory hands it across threads.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync + 'static>() {}
    assert::<Updater>();
}
