//! Isengard `updater` plugin.
//!
//! Watches running Docker containers and (in later sub-phases) keeps their
//! images up to date. Phase 3b: filters containers by `isengard.enable=true`,
//! compares each one's local digest against its remote registry digest, and
//! classifies as `up_to_date | needs_update | unknown`.

#![allow(clippy::result_large_err)]

pub mod auth;
pub mod image_ref;
pub mod labels;
pub mod recreate;
pub mod registry;
pub mod self_id;
pub mod self_update;

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

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;
use crate::labels::isengard_enabled;
use crate::registry::RegistryClient;

const PLUGIN_NAME: &str = "updater";
const CYCLE_INTERVAL: Duration = Duration::from_secs(30);

pub struct Updater {
    /// Lazily set in `init`. Wrapped in Option so the struct can be constructed
    /// by the inventory factory before init runs.
    docker: Option<Docker>,
    registry: Option<Arc<RegistryClient>>,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            registry: None,
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

/// One cycle of work. Filter candidates by `isengard.enable=true`, compare
/// each one's local digest against its remote registry digest, classify, log.
async fn do_cycle(docker: &Docker, registry: &RegistryClient) -> anyhow::Result<()> {
    let opts = ListContainersOptions::<String> {
        all: false,
        ..Default::default()
    };
    let containers = docker
        .list_containers(Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("listing containers: {e}"))?;

    let candidates: Vec<_> = containers
        .iter()
        .filter(|c| isengard_enabled(c.labels.as_ref()))
        .collect();

    let mut up_to_date = 0usize;
    let mut needs_update = 0usize;
    let mut unknown = 0usize;

    for c in &candidates {
        let name = c
            .names
            .as_ref()
            .and_then(|ns| ns.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unknown>".into());
        let image_str = c.image.as_deref().unwrap_or("");

        let Some(image_ref) = ImageRef::parse(image_str) else {
            debug!(container = %name, image = %image_str, "skipping digest-pinned or unparseable image");
            continue;
        };

        let local_digest = match docker.inspect_image(image_str).await {
            Ok(i) => i
                .repo_digests
                .as_ref()
                .and_then(|v| v.first())
                .and_then(|d| d.split_once('@'))
                .map(|(_, dig)| dig.to_string()),
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "inspect_image failed");
                None
            }
        };

        let remote_digest = match registry.head_digest(&image_ref).await {
            Ok(d) => d,
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "registry HEAD failed");
                unknown += 1;
                continue;
            }
        };

        match (local_digest.as_deref(), remote_digest.as_deref()) {
            (Some(local), Some(remote)) if local == remote => {
                info!(container = %name, image = %image_str, status = "up_to_date");
                up_to_date += 1;
            }
            (Some(local), Some(remote)) => {
                info!(
                    container = %name,
                    image = %image_str,
                    current_digest = %local,
                    remote_digest = %remote,
                    status = "needs_update"
                );
                needs_update += 1;

                // Self-protection: skip if this container is the agent itself.
                if let Some(self_id) = crate::self_id::current_container_id() {
                    if c.id.as_deref() == Some(self_id.as_str()) {
                        warn!(
                            container = %name,
                            "skipping self-update; deferred to phase 3d"
                        );
                        continue;
                    }
                }

                let Some(container_id) = c.id.as_deref() else {
                    warn!(container = %name, "no container ID; cannot update");
                    continue;
                };

                if let Err(e) = recreate::update_container(docker, container_id, &image_ref).await {
                    warn!(container = %name, error = %e, "update failed");
                }
            }
            _ => {
                debug!(container = %name, image = %image_str, "could not classify (missing local or remote digest)");
                unknown += 1;
            }
        }
    }

    info!(
        candidates = candidates.len(),
        up_to_date, needs_update, unknown, "updater cycle complete"
    );
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

        let docker_config = DockerConfig::load_default().unwrap_or_else(|e| {
            warn!(error = %e, "failed to read ~/.docker/config.json — proceeding without registry creds");
            DockerConfig::default()
        });
        let registry = RegistryClient::new(docker_config)
            .map_err(|e| init_err(format!("registry client: {e}")))?;
        self.registry = Some(Arc::new(registry));

        // If we're running inside a container, clean up any leftover
        // `<our-name>-replaced-*` siblings from a prior self-update.
        if let Some(my_id) = crate::self_id::current_container_id() {
            match docker.inspect_container(&my_id, None).await {
                Ok(self_inspect) => {
                    let my_name = self_inspect
                        .name
                        .as_deref()
                        .map(|s| s.trim_start_matches('/').to_string())
                        .unwrap_or_default();
                    if !my_name.is_empty() {
                        crate::self_update::cleanup_replaced_siblings(&docker, &my_name).await;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "could not inspect self container; skipping replaced-sibling cleanup");
                }
            }
        }

        self.docker = Some(docker);
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let registry = self
            .registry
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
                        if let Err(e) = do_cycle(&docker, &registry).await {
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
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        do_cycle(docker, registry)
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
