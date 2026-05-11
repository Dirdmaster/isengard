//! Phase 0.4: backend-agnostic runtime surface.
//!
//! The agent's compose / logs / deploy paths historically called bollard
//! directly. Phase 0.4 lifts those calls behind the [`RuntimeBackend`] trait
//! so a second backend (wisp, dispatch B) can slot in without touching the
//! callers. Bollard remains the default; selection is via the
//! `ISENGARD_RUNTIME` env var (see [`select::select_backend`]).
//!
//! Dispatch A (this commit set):
//! - A1 (this file): trait + types + RuntimeError.
//! - A2: env-driven `select_backend()` factory.
//! - A3: BollardBackend extracts the existing bollard call surface.
//! - A4: refactor existing call sites to construct + thread the trait.
//!
//! The trait shape is the agent's slice of bollard, plus a `run_healthcheck`
//! abstraction (bollard runs healthchecks in-container; wisp will run them
//! externally via nsenter / HTTP probes).

pub mod bollard_backend;
mod error;
pub mod select;
mod spec;
pub mod wisp_backend;
#[cfg(target_os = "linux")]
pub(crate) mod wisp_backend_attacher;

pub use error::RuntimeError;
pub use select::{BackendChoice, select_backend};
pub use spec::{
    ContainerCreateSpec, ContainerSnapshot, ContainerState, HealthState, HealthcheckSpec, HostPort,
    LinuxResources, ListFilter, LogChunk, LogOptions, LogSource, MountKind, MountSpec,
    NetworkSettings, PortProtocol, PortSpec, RestartPolicy, RuntimeEvent, RuntimeEventType,
    SecretMount,
};

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;

/// Backend-agnostic surface the agent uses to drive a container runtime.
/// Implemented today by `BollardBackend` (Phase 0.4 dispatch A) and, after
/// dispatch B, by `WispBackend`.
#[async_trait]
pub trait RuntimeBackend: Send + Sync + std::fmt::Debug {
    /// Pull `reference` if missing. Returns the manifest digest the runtime
    /// would record for the resolved image (e.g. `sha256:...`).
    async fn ensure_image(&self, reference: &str) -> Result<String, RuntimeError>;

    /// Create a container from `spec`. Returns the runtime's opaque handle
    /// (bollard returns the container ID; wisp will return the container
    /// name). Callers treat the handle as a String.
    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, RuntimeError>;

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError>;
    async fn stop_container(&self, id: &str, timeout_s: u32) -> Result<(), RuntimeError>;
    async fn remove_container(&self, id: &str, force: bool) -> Result<(), RuntimeError>;

    async fn list_containers(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<ContainerSnapshot>, RuntimeError>;
    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerSnapshot>, RuntimeError>;

    async fn connect_network(&self, container_id: &str, network: &str) -> Result<(), RuntimeError>;
    async fn disconnect_network(
        &self,
        container_id: &str,
        network: &str,
    ) -> Result<(), RuntimeError>;

    /// Stream container logs. Each frame keeps its native framing; the
    /// caller is responsible for newline splitting and (when
    /// [`LogOptions::timestamps`] is set) timestamp parsing.
    fn stream_logs(
        &self,
        id: &str,
        opts: LogOptions,
    ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>>;

    /// Stream runtime events (start, die, healthcheck transitions). Wisp's
    /// 0.4 impl polls + diffs; bollard's wraps the native event stream.
    fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>>;

    /// Run a one-shot healthcheck against a container. Bollard's impl
    /// returns the docker-recorded health state; wisp's impl probes
    /// externally (HTTP for `["CMD", "curl", ...]` shapes, nsenter
    /// otherwise).
    async fn run_healthcheck(
        &self,
        id: &str,
        hc: &HealthcheckSpec,
    ) -> Result<HealthState, RuntimeError>;

    /// Backend identity for diagnostics (`"docker"` or `"wisp"`).
    fn name(&self) -> &'static str;

    /// Does this backend support attaching additional networks to a
    /// container after `create_container` has returned?
    ///
    /// Bollard / dockerd: yes (returns `true`). The compose reconciler
    /// calls `connect_network` once per declared network because
    /// dockerd's `Create` only takes the primary network at create
    /// time. See `compose_apply::ensure_container_started`.
    ///
    /// Wisp: no (returns `false`). The full list of networks is passed
    /// at `create_container` time and the runtime wires every veth /
    /// IP / iptables rule during the pre-exec window. A subsequent
    /// `connect_network` would mean reconstructing the netns and is
    /// out of scope for 0.5. Compose can skip the per-network
    /// `connect_network` loop for backends that report `false` here.
    fn supports_live_network_attach(&self) -> bool {
        true
    }

    /// Boot-time orphan cleanup hook. Wisp's impl walks kernel network
    /// state (bridges + veths + iptables) against the on-disk registry
    /// and the live container list, removing anything not accounted
    /// for. Bollard's impl returns 0 (docker daemon owns its own
    /// reconcile). Called once from `run_agent` before the first
    /// compose reconcile fires, so a fresh container start doesn't
    /// race a stale bridge from a previous boot.
    ///
    /// Returns the number of cleanup actions taken. Non-fatal: errors
    /// are logged at the call site; the agent keeps booting.
    async fn reconcile_network_orphans(&self) -> Result<usize, RuntimeError> {
        Ok(0)
    }

    /// Borrow the underlying bollard handle when this backend is the
    /// bollard one. Returns `None` for non-bollard backends (today: wisp,
    /// once dispatch B lands). Phase 0.4 dispatch A keeps a handful of
    /// internal helpers (compose_apply, deployment/driver, labels, the
    /// log decoder, proxy IP discovery) calling bollard directly; this
    /// escape hatch lets them keep working without poisoning the public
    /// surface. Dispatch B replaces those callers with trait-driven
    /// equivalents and removes the accessor.
    fn as_bollard(&self) -> Option<std::sync::Arc<bollard::Docker>> {
        None
    }
}

#[cfg(test)]
mod tests {
    //! A1 ships shape-only smoke coverage: implement [`RuntimeBackend`] on a
    //! stub and ensure the trait compiles + dispatches dynamically.

    use super::*;
    use std::sync::Arc;

    #[derive(Debug, Default)]
    struct MockBackend;

    #[async_trait]
    impl RuntimeBackend for MockBackend {
        async fn ensure_image(&self, _reference: &str) -> Result<String, RuntimeError> {
            Ok("sha256:00".into())
        }

        async fn create_container(
            &self,
            _spec: &ContainerCreateSpec,
        ) -> Result<String, RuntimeError> {
            Ok("mock-id".into())
        }

        async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn stop_container(&self, _id: &str, _timeout_s: u32) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn remove_container(&self, _id: &str, _force: bool) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn list_containers(
            &self,
            _filter: ListFilter,
        ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
            Ok(Vec::new())
        }

        async fn inspect_container(
            &self,
            _id: &str,
        ) -> Result<Option<ContainerSnapshot>, RuntimeError> {
            Ok(None)
        }

        async fn connect_network(
            &self,
            _container_id: &str,
            _network: &str,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn disconnect_network(
            &self,
            _container_id: &str,
            _network: &str,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn stream_logs(
            &self,
            _id: &str,
            _opts: LogOptions,
        ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
            Box::pin(futures_util::stream::empty())
        }

        fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>> {
            Box::pin(futures_util::stream::empty())
        }

        async fn run_healthcheck(
            &self,
            _id: &str,
            _hc: &HealthcheckSpec,
        ) -> Result<HealthState, RuntimeError> {
            Ok(HealthState::Healthy)
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    #[tokio::test]
    async fn mock_backend_satisfies_dyn_trait() {
        let backend: Arc<dyn RuntimeBackend> = Arc::new(MockBackend);
        assert_eq!(backend.name(), "mock");
        assert_eq!(backend.ensure_image("nginx").await.unwrap(), "sha256:00");
        assert_eq!(
            backend.create_container(&dummy_spec()).await.unwrap(),
            "mock-id"
        );
        backend.start_container("x").await.unwrap();
        backend.stop_container("x", 10).await.unwrap();
        backend.remove_container("x", true).await.unwrap();
        assert!(
            backend
                .list_containers(ListFilter::default())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(backend.inspect_container("x").await.unwrap().is_none());
        backend
            .connect_network("c", "isengard-proxy")
            .await
            .unwrap();
        backend.disconnect_network("c", "bridge").await.unwrap();
        let _logs = backend.stream_logs("c", LogOptions::default());
        let _events = backend.stream_events();
        let hc = HealthcheckSpec {
            test: vec!["CMD".into(), "true".into()],
            interval: std::time::Duration::from_secs(1),
            timeout: std::time::Duration::from_secs(1),
            retries: 1,
            start_period: std::time::Duration::from_secs(0),
        };
        assert_eq!(
            backend.run_healthcheck("x", &hc).await.unwrap(),
            HealthState::Healthy,
        );
    }

    fn dummy_spec() -> ContainerCreateSpec {
        ContainerCreateSpec {
            container_name: "c".into(),
            image: "nginx".into(),
            stack: "s".into(),
            service: "svc".into(),
            command: None,
            entrypoint: None,
            env: Default::default(),
            labels: Default::default(),
            mounts: Vec::new(),
            ports: Vec::new(),
            networks: Vec::new(),
            restart: RestartPolicy::No,
            healthcheck: None,
            user: None,
            working_dir: None,
            hostname: None,
            linux_resources: None,
            secrets: Vec::new(),
        }
    }
}
