//! Per-deployment state machine driver.
//!
//! One [`Driver`] task owns one [`Deployment`] row and walks it through the
//! blue-green state machine: SpinningUp → Switching → Draining →
//! DestroyingBlue → Done. On failure (start, healthcheck, swap) the driver
//! transitions to Aborted, best-effort cleans up the green container, and
//! emits a `deployment.aborted` event so the dashboard / journal sees the
//! reason.
//!
//! All side effects (Docker calls, swap_upstream, healthcheck construction)
//! go through the [`DriverDeps`] trait so unit tests can swap in mocks
//! without standing up a real Docker daemon. The production binding is
//! [`RealDriverDeps`].
//!
//! See spec
//! `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`
//! §`driver.rs`.

use crate::deployment::healthcheck::DeploymentHealthcheck;
use crate::proxy::ProxyState;
use crate::proxy::healthcheck::HealthChecker;
use crate::proxy::swap::swap_upstream;
use crate::proxy::upstreams::{Upstream, UpstreamState};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use isengard_core::{Event, EventEmitter};
use isengard_storage::deployment::{Deployment, DeploymentState};
use isengard_storage::inventory::Inventory;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// All side effects the driver performs. Production uses [`RealDriverDeps`];
/// tests swap in a mock that records calls and returns canned results.
#[async_trait]
pub trait DriverDeps: Send + Sync + 'static {
    /// Spin up the green container. Returns its `(container_id, addr)` once
    /// Docker reports it as started. The driver will then run the
    /// healthcheck against the returned addr.
    async fn start_green(&self, deployment: &Deployment) -> Result<(String, SocketAddr)>;

    /// Stop and remove `container_id`. Idempotent — a "not found" error from
    /// Docker is treated as success. Used for both green-cleanup-on-abort
    /// and blue-destruction-on-success.
    async fn stop_and_remove(&self, container_id: &str) -> Result<()>;

    /// Build the per-deployment healthcheck primitive (HTTP if the
    /// deployment carries a `health_path`, TCP-only otherwise). Returned by
    /// reference so tests can vary it without owning an `HealthChecker`.
    fn build_health_checker(&self, deployment: &Deployment) -> HealthChecker;

    /// Atomically swap traffic from blue to the green upstream at
    /// `deployment.public_hostname`. Drains the old entry over `grace`.
    async fn swap_upstream_to_green(
        &self,
        deployment: &Deployment,
        green_container_id: &str,
        green_addr: SocketAddr,
        grace: Duration,
    ) -> Result<()>;
}

/// Default healthcheck cadence: 5s between probes, 2 consecutive passes
/// required, 120s deadline. Override per-driver via
/// [`Driver::with_healthcheck_overrides`] (used by tests with sub-second
/// deadlines).
const DEFAULT_HC_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_HC_SUCCESS_THRESHOLD: u32 = 2;
const DEFAULT_HC_DEADLINE: Duration = Duration::from_secs(120);

/// Grace period passed to [`swap_upstream`]: in-flight requests on blue have
/// this long to finish before the entry is removed from the registry.
const DEFAULT_SWAP_GRACE: Duration = Duration::from_secs(60);

/// Buffer added to the swap grace before transitioning DestroyingBlue, so a
/// small clock skew between [`swap_upstream`]'s cleanup task and the driver's
/// own timer can never race blue's destruction past in-flight requests.
const DRAIN_BUFFER: Duration = Duration::from_secs(5);

/// Per-deployment state-machine driver. One instance per [`Deployment`] row;
/// `run` consumes self and walks the machine to a terminal state.
pub struct Driver<D: DriverDeps> {
    pub deployment: Deployment,
    pub deps: Arc<D>,
    pub inventory: Inventory,
    pub emitter: Arc<dyn EventEmitter>,
    pub hc_interval: Duration,
    pub hc_success_threshold: u32,
    pub hc_deadline: Duration,
    pub swap_grace: Duration,
    pub drain_buffer: Duration,
}

impl<D: DriverDeps> Driver<D> {
    pub fn new(
        deployment: Deployment,
        deps: Arc<D>,
        inventory: Inventory,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self {
            deployment,
            deps,
            inventory,
            emitter,
            hc_interval: DEFAULT_HC_INTERVAL,
            hc_success_threshold: DEFAULT_HC_SUCCESS_THRESHOLD,
            hc_deadline: DEFAULT_HC_DEADLINE,
            swap_grace: DEFAULT_SWAP_GRACE,
            drain_buffer: DRAIN_BUFFER,
        }
    }

    /// Override the swap grace + drain buffer. Used by tests so the
    /// drain-sleep doesn't cost 65s of wall time per happy-path run.
    pub fn with_drain_overrides(mut self, swap_grace: Duration, drain_buffer: Duration) -> Self {
        self.swap_grace = swap_grace;
        self.drain_buffer = drain_buffer;
        self
    }

    /// Override the healthcheck cadence. Primary use: tests that need
    /// sub-second deadlines so the timeout case completes in milliseconds
    /// instead of the production 120s.
    pub fn with_healthcheck_overrides(
        mut self,
        interval: Duration,
        success_threshold: u32,
        deadline: Duration,
    ) -> Self {
        self.hc_interval = interval;
        self.hc_success_threshold = success_threshold;
        self.hc_deadline = deadline;
        self
    }

    /// Walk the state machine to a terminal state. On any error the driver
    /// transitions to `Aborted` (or `Failed` if the storage call itself
    /// breaks), records the error on the row, and emits
    /// `deployment.aborted`. Always runs to completion — this method does
    /// not propagate errors.
    pub async fn run(mut self) {
        match self.run_inner().await {
            Ok(()) => {}
            Err(e) => {
                // run_inner itself already handled per-state aborts. This
                // catches storage-layer breakage and similar surprises:
                // mark Failed and surface the error string.
                let msg = format!("driver_error: {e}");
                let _ = self
                    .inventory
                    .set_deployment_error(&self.deployment.id, &msg)
                    .await;
                let _ = self
                    .inventory
                    .update_deployment_state(&self.deployment.id, DeploymentState::Failed)
                    .await;
                self.emit("deployment.failed", Some(msg));
            }
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        // SpinningUp: create + start green.
        self.transition(DeploymentState::SpinningUp).await?;
        let (green_id, green_addr) = match self.deps.start_green(&self.deployment).await {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("spinup_failed: {e}");
                self.abort(msg, None).await?;
                return Ok(());
            }
        };
        self.deployment.green_container = Some(green_id.clone());
        self.inventory
            .set_deployment_green_container(&self.deployment.id, &green_id)
            .await?;

        // Healthcheck: wait for green to go healthy.
        let hc = DeploymentHealthcheck::new(self.deps.build_health_checker(&self.deployment))
            .with_interval(self.hc_interval)
            .with_success_threshold(self.hc_success_threshold)
            .with_deadline(self.hc_deadline);
        match hc.wait_for_healthy(green_addr).await {
            Ok(passed_at) => {
                self.deployment.healthcheck_passed_at = Some(passed_at);
                let _ = self
                    .inventory
                    .set_deployment_healthcheck_passed(&self.deployment.id, passed_at)
                    .await;
            }
            Err(timeout) => {
                let msg = format!(
                    "healthcheck_timeout: {} attempts logged",
                    timeout.last_attempts.len()
                );
                self.abort(msg, Some(green_id.clone())).await?;
                return Ok(());
            }
        }

        // Switching: atomic swap to green.
        self.transition(DeploymentState::Switching).await?;
        if let Err(e) = self
            .deps
            .swap_upstream_to_green(&self.deployment, &green_id, green_addr, self.swap_grace)
            .await
        {
            let msg = format!("swap_failed: {e}");
            self.abort(msg, Some(green_id.clone())).await?;
            return Ok(());
        }
        let switched_at = Utc::now();
        self.deployment.switched_at = Some(switched_at);
        let _ = self
            .inventory
            .set_deployment_switched(&self.deployment.id, switched_at)
            .await;

        // Draining: let in-flight requests on blue complete.
        self.transition(DeploymentState::Draining).await?;
        tokio::time::sleep(self.swap_grace + self.drain_buffer).await;
        let drained_at = Utc::now();
        self.deployment.drained_at = Some(drained_at);
        let _ = self
            .inventory
            .set_deployment_drained(&self.deployment.id, drained_at)
            .await;

        // DestroyingBlue: stop + rm the old container.
        self.transition(DeploymentState::DestroyingBlue).await?;
        if let Some(blue_id) = self.deployment.blue_container.clone() {
            // Best-effort: a missing blue (user removed it manually) is fine.
            let _ = self.deps.stop_and_remove(&blue_id).await;
        }

        // Done.
        let finished_at = Utc::now();
        self.deployment.finished_at = Some(finished_at);
        let _ = self
            .inventory
            .set_deployment_finished(&self.deployment.id, finished_at)
            .await;
        self.transition(DeploymentState::Done).await?;
        self.emit("deployment.completed", None);
        Ok(())
    }

    /// Move the row to `new` and emit `deployment.<state>`.
    async fn transition(&mut self, new: DeploymentState) -> Result<()> {
        self.inventory
            .update_deployment_state(&self.deployment.id, new)
            .await
            .map_err(|e| anyhow!("update_deployment_state({:?}): {e}", new))?;
        self.deployment.state = new;
        self.emit(&format!("deployment.{}", new.as_str()), None);
        Ok(())
    }

    /// Mark the deployment Aborted with `error_msg`, best-effort clean up
    /// `green_id` if present, and emit `deployment.aborted`. Used for every
    /// failure path that has already crossed the SpinningUp boundary.
    ///
    /// Cleanup is best-effort: a failed `stop_and_remove` doesn't block the
    /// transition to Aborted (the row state is more important than green's
    /// disposal), but it's logged at warn so persistent Docker issues are
    /// observable in production logs instead of silently leaking.
    async fn abort(&mut self, error_msg: String, green_id: Option<String>) -> Result<()> {
        if let Some(gid) = green_id {
            if let Err(e) = self.deps.stop_and_remove(&gid).await {
                tracing::warn!(
                    deployment_id = %self.deployment.id,
                    green_container = %gid,
                    error = %e,
                    "deployment abort: green container cleanup failed (may leak)"
                );
            }
        }
        self.deployment.error = Some(error_msg.clone());
        if let Err(e) = self
            .inventory
            .set_deployment_error(&self.deployment.id, &error_msg)
            .await
        {
            tracing::warn!(
                deployment_id = %self.deployment.id,
                error = %e,
                "deployment abort: failed to persist error message to row"
            );
        }
        self.transition(DeploymentState::Aborted).await?;
        self.emit("deployment.aborted", Some(error_msg));
        Ok(())
    }

    /// Fire-and-forget event emission. Spawned so the state machine never
    /// blocks on emitter latency (network, journal flush, etc.).
    fn emit(&self, kind: &str, error: Option<String>) {
        let emitter = self.emitter.clone();
        let event = Event {
            kind: kind.to_string(),
            occurred_at: Utc::now(),
            host_id: Some(self.deployment.host_id.0),
            summary: format!(
                "deployment {} ({}): {}",
                self.deployment.id, self.deployment.service_name, kind
            ),
            container_name: self.deployment.green_container.clone(),
            old_digest: Some(self.deployment.blue_digest.clone()),
            new_digest: Some(self.deployment.green_digest.clone()),
            error,
            ..Default::default()
        };
        tokio::spawn(async move { emitter.emit(event).await });
    }
}

/// Production [`DriverDeps`] binding: bollard for Docker, the
/// [`HealthChecker`] primitive for probes, and [`swap_upstream`] for the
/// traffic shift.
pub struct RealDriverDeps {
    pub docker: Arc<bollard::Docker>,
    pub proxy_state: ProxyState,
}

#[async_trait]
impl DriverDeps for RealDriverDeps {
    async fn start_green(&self, deployment: &Deployment) -> Result<(String, SocketAddr)> {
        use bollard::container::{Config, CreateContainerOptions, StartContainerOptions};
        use bollard::image::CreateImageOptions;
        use futures_util::StreamExt;

        let blue_id = deployment
            .blue_container
            .as_deref()
            .ok_or_else(|| anyhow!("no blue container to base green on"))?;
        let blue = self
            .docker
            .inspect_container(blue_id, None)
            .await
            .with_context(|| format!("inspect blue {blue_id}"))?;

        let image = blue
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .ok_or_else(|| anyhow!("blue has no image"))?;
        // For BG, the green image is the same name with the new digest. The
        // updater detected the digest change against the registry; the image
        // tag is unchanged. Pulling re-fetches the new digest under that tag.
        let pull_opts = CreateImageOptions {
            from_image: image.as_str(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(pull_opts), None, None);
        while let Some(item) = stream.next().await {
            item.with_context(|| format!("pulling {image}"))?;
        }

        // Build green Config from blue (mirror updater's recreate.rs::capture_config
        // pattern, but DON'T set image to a new tag — we're keeping the tag, just
        // running the new digest).
        let cfg_in = blue.config.clone().unwrap_or_default();
        let host_cfg = blue.host_config.clone().unwrap_or_default();
        let cfg: Config<String> = Config {
            image: Some(image.clone()),
            cmd: cfg_in.cmd,
            entrypoint: cfg_in.entrypoint,
            env: cfg_in.env,
            labels: cfg_in.labels,
            working_dir: cfg_in.working_dir,
            user: cfg_in.user,
            exposed_ports: cfg_in.exposed_ports,
            host_config: Some(host_cfg),
            healthcheck: cfg_in.healthcheck,
            ..Default::default()
        };

        // Green container name: <service>-green-<deployment_id_prefix>
        let id_short = &deployment.id[..deployment.id.len().min(8)];
        let green_name = format!("{}-green-{}", deployment.service_name, id_short);
        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: green_name.clone(),
                    platform: None,
                }),
                cfg,
            )
            .await
            .with_context(|| format!("create container {green_name}"))?;

        // Reconnect to non-bridge networks (mirror recreate.rs).
        if let Some(ns) = blue
            .network_settings
            .as_ref()
            .and_then(|s| s.networks.as_ref())
        {
            for (net_name, settings) in ns {
                if net_name == "bridge" {
                    continue;
                }
                let _ = self
                    .docker
                    .connect_network(
                        net_name,
                        bollard::network::ConnectNetworkOptions {
                            container: create.id.clone(),
                            endpoint_config: settings.clone(),
                        },
                    )
                    .await; // best-effort; some networks may have changed
            }
        }

        self.docker
            .start_container(&create.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("start container {green_name}"))?;

        // Re-inspect to get the assigned IP.
        let started = self.docker.inspect_container(&create.id, None).await?;
        let ip = started
            .network_settings
            .as_ref()
            .and_then(|s| s.ip_address.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                // Fall back to the first network's IP.
                started
                    .network_settings
                    .as_ref()
                    .and_then(|s| s.networks.as_ref())
                    .and_then(|nets| {
                        nets.values().find_map(|settings| {
                            settings.ip_address.clone().filter(|s| !s.is_empty())
                        })
                    })
            })
            .ok_or_else(|| anyhow!("green container has no IP"))?;
        let port = deployment.container_port.unwrap_or(8080) as u16;
        let addr: SocketAddr = format!("{ip}:{port}")
            .parse()
            .with_context(|| format!("parse addr {ip}:{port}"))?;

        Ok((create.id, addr))
    }

    async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
        use bollard::container::{RemoveContainerOptions, StopContainerOptions};
        // Best-effort stop: a missing-or-already-stopped container should
        // not fail the cleanup — the subsequent remove_container handles
        // both cases idempotently below.
        let _ = self
            .docker
            .stop_container(container_id, Some(StopContainerOptions { t: 10 }))
            .await;
        match self
            .docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // bollard's "no such container" surfaces as a 404; treat
                // as success so manual cleanup races don't fail the abort
                // path. Anything else is a real failure.
                let s = e.to_string();
                if s.contains("404") || s.to_lowercase().contains("no such container") {
                    Ok(())
                } else {
                    Err(anyhow!("remove {container_id}: {e}"))
                }
            }
        }
    }

    fn build_health_checker(&self, deployment: &Deployment) -> HealthChecker {
        match &deployment.health_path {
            Some(p) => HealthChecker::new(p.clone(), Duration::from_secs(2)),
            None => HealthChecker::tcp_only(Duration::from_secs(2)),
        }
    }

    async fn swap_upstream_to_green(
        &self,
        deployment: &Deployment,
        green_container_id: &str,
        green_addr: SocketAddr,
        grace: Duration,
    ) -> Result<()> {
        let hostname = deployment
            .public_hostname
            .as_deref()
            .ok_or_else(|| anyhow!("deployment {} has no public_hostname", deployment.id))?;
        let upstream = Upstream {
            container_id: green_container_id.to_string(),
            addr: green_addr,
            healthy: true,
            health_path: deployment.health_path.clone(),
            health_interval: Duration::from_secs(5),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        };
        swap_upstream(&self.proxy_state, hostname, upstream, grace).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_core::NoopEmitter;
    use isengard_storage::deployment::{DeployStrategy, InsertDeployment};
    use isengard_storage::host::EnrollHost;
    use isengard_storage::inventory::Inventory;
    use isengard_storage::stack::{InsertStack, StackSource};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Insert a fresh in-memory inventory, host, stack, and Pending
    /// deployment row. Returns the inventory + the row so each test can
    /// drive its own driver against them.
    async fn setup_inventory_and_row(state: DeploymentState) -> (Inventory, Deployment) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "24.0".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let d = inv
            .insert_deployment(InsertDeployment {
                id: ulid::Ulid::new().to_string(),
                host_id,
                stack_id,
                service_name: "web".into(),
                strategy: DeployStrategy::BlueGreen,
                state,
                blue_container: Some("blue-id".into()),
                green_container: None,
                blue_digest: "sha256:aaa".into(),
                green_digest: "sha256:bbb".into(),
                public_hostname: Some("blog.test".into()),
                health_path: None,
                container_port: Some(8080),
                metadata_json: None,
            })
            .await
            .unwrap();
        (inv, d)
    }

    /// A live TCP listener whose addr is healthy for [`HealthChecker::tcp_only`].
    /// Returned task is aborted at end of scope by drop.
    fn live_addr() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let h = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        (addr, h)
    }

    /// An addr that will refuse all connections (for healthcheck timeout).
    fn dead_addr() -> SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    }

    // ---- Test 1: happy path ----

    struct HappyDeps {
        addr: SocketAddr,
        stop_remove_calls: Arc<AtomicUsize>,
        last_stop_id: Arc<std::sync::Mutex<Option<String>>>,
        swap_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl DriverDeps for HappyDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green-id".into(), self.addr))
        }
        async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
            self.stop_remove_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_stop_id.lock().unwrap() = Some(container_id.to_string());
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(100))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            self.swap_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn happy_path_advances_to_done() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();
        let stop_remove_calls = Arc::new(AtomicUsize::new(0));
        let last_stop_id = Arc::new(std::sync::Mutex::new(None));
        let swap_called = Arc::new(AtomicBool::new(false));
        let deps = Arc::new(HappyDeps {
            addr,
            stop_remove_calls: stop_remove_calls.clone(),
            last_stop_id: last_stop_id.clone(),
            swap_called: swap_called.clone(),
        });
        // Real wall-clock here (sqlx pool dislikes tokio's paused clock).
        // Tiny grace + buffer keeps the run under ~100ms even with the
        // drain sleep.
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500))
            .with_drain_overrides(Duration::from_millis(20), Duration::from_millis(10));
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Done, "expected Done");
        assert!(swap_called.load(Ordering::SeqCst), "swap was called");
        assert_eq!(
            stop_remove_calls.load(Ordering::SeqCst),
            1,
            "exactly one stop_and_remove (blue)"
        );
        assert_eq!(
            last_stop_id.lock().unwrap().as_deref(),
            Some("blue-id"),
            "blue is the one destroyed"
        );
        assert!(after.green_container.as_deref() == Some("green-id"));
        assert!(after.healthcheck_passed_at.is_some());
        assert!(after.switched_at.is_some());
        assert!(after.drained_at.is_some());
        assert!(after.finished_at.is_some());
    }

    // ---- Test 2: spinup failure marks Aborted ----

    struct SpinupFailDeps;

    #[async_trait]
    impl DriverDeps for SpinupFailDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Err(anyhow!("docker create: image pull failed: 404 not found"))
        }
        async fn stop_and_remove(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(100))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn spinup_failure_marks_aborted() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let driver = Driver::new(
            d,
            Arc::new(SpinupFailDeps),
            inv.clone(),
            Arc::new(NoopEmitter),
        );
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.starts_with("spinup_failed:"),
            "expected spinup_failed prefix, got {err:?}"
        );
        assert!(after.green_container.is_none(), "green never assigned");
    }

    // ---- Test 3: healthcheck timeout marks Aborted + cleans green ----

    struct HealthTimeoutDeps {
        dead: SocketAddr,
        stop_remove_calls: Arc<AtomicUsize>,
        last_stop_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl DriverDeps for HealthTimeoutDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green-id".into(), self.dead))
        }
        async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
            self.stop_remove_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_stop_id.lock().unwrap() = Some(container_id.to_string());
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(20))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn healthcheck_timeout_marks_aborted_and_cleans_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let stop_remove_calls = Arc::new(AtomicUsize::new(0));
        let last_stop_id = Arc::new(std::sync::Mutex::new(None));
        let deps = Arc::new(HealthTimeoutDeps {
            dead: dead_addr(),
            stop_remove_calls: stop_remove_calls.clone(),
            last_stop_id: last_stop_id.clone(),
        });
        // Sub-second deadline so the test completes fast. Real-time clock
        // here (no start_paused) because the healthcheck loop polls a
        // refused TCP addr — that's a real syscall, not a sleep.
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(20), 3, Duration::from_millis(200));
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("healthcheck_timeout"),
            "expected healthcheck_timeout, got {err:?}"
        );
        assert_eq!(
            stop_remove_calls.load(Ordering::SeqCst),
            1,
            "green should be cleaned up exactly once"
        );
        assert_eq!(
            last_stop_id.lock().unwrap().as_deref(),
            Some("green-id"),
            "the cleaned container is green"
        );
    }

    // ---- Test 4: swap failure marks Aborted + cleans green ----

    struct SwapFailDeps {
        addr: SocketAddr,
        stop_remove_calls: Arc<AtomicUsize>,
        last_stop_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl DriverDeps for SwapFailDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green-id".into(), self.addr))
        }
        async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
            self.stop_remove_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_stop_id.lock().unwrap() = Some(container_id.to_string());
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(100))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            Err(anyhow!("registry write contention"))
        }
    }

    #[tokio::test]
    async fn swap_failure_marks_aborted_and_cleans_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();
        let stop_remove_calls = Arc::new(AtomicUsize::new(0));
        let last_stop_id = Arc::new(std::sync::Mutex::new(None));
        let deps = Arc::new(SwapFailDeps {
            addr,
            stop_remove_calls: stop_remove_calls.clone(),
            last_stop_id: last_stop_id.clone(),
        });
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500));
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("swap_failed"),
            "expected swap_failed, got {err:?}"
        );
        assert_eq!(
            stop_remove_calls.load(Ordering::SeqCst),
            1,
            "green should be cleaned up exactly once"
        );
        assert_eq!(
            last_stop_id.lock().unwrap().as_deref(),
            Some("green-id"),
            "the cleaned container is green (blue still serving)"
        );
    }
}
