//! Backend-agnostic runtime surface. Introduced the trait so
//! the agent's compose / logs / deploy paths speak one shape; today
//! Bollard is the only implementor.

pub mod bollard_backend;
mod error;
mod spec;

pub use error::RuntimeError;
pub use spec::{
    ContainerCreateSpec, ContainerNetworkMode, ContainerSnapshot, ContainerState, HealthState,
    HealthcheckSpec, HostPort, IngressEndpoint, IngressEndpointMode, LinuxResources, ListFilter,
    LogChunk, LogOptions, LogSource, MountKind, MountSpec, NetworkSettings, PortProtocol, PortSpec,
    RestartPolicy, RuntimeEvent, RuntimeEventType, SecretMount, UnresolvedIngressReason,
};

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;

/// Backend-agnostic surface the agent uses to drive a container runtime.
/// Implemented today by `BollardBackend`.
#[async_trait]
pub trait RuntimeBackend: Send + Sync + std::fmt::Debug {
    /// Pull `reference` if missing. Returns the manifest digest the runtime
    /// would record for the resolved image (e.g. `sha256:...`).
    async fn ensure_image(&self, reference: &str) -> Result<String, RuntimeError>;

    /// Create a container from `spec`. Returns the runtime's opaque handle
    /// (bollard returns the container ID). Callers treat the handle as a
    /// String.
    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, RuntimeError>;

    /// Start a previously-created container.
    async fn start_container(&self, id: &str) -> Result<(), RuntimeError>;
    /// Stop a running container with `timeout_s` for graceful SIGTERM.
    async fn stop_container(&self, id: &str, timeout_s: u32) -> Result<(), RuntimeError>;
    /// Remove a container. `force` removes a still-running one.
    async fn remove_container(&self, id: &str, force: bool) -> Result<(), RuntimeError>;

    /// List containers matching `filter`.
    async fn list_containers(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<ContainerSnapshot>, RuntimeError>;
    /// Inspect one container by id. Returns `None` when the runtime has no
    /// such container.
    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerSnapshot>, RuntimeError>;

    /// Attach `container_id` to `network`. Bollard / dockerd uses this to
    /// add secondary networks beyond the create-time primary.
    async fn connect_network(&self, container_id: &str, network: &str) -> Result<(), RuntimeError>;
    /// Detach `container_id` from `network`.
    async fn disconnect_network(
        &self,
        container_id: &str,
        network: &str,
    ) -> Result<(), RuntimeError>;

    /// Idempotently ensure a named network exists on the host before any
    /// container is asked to attach to it. Bollard / dockerd manages
    /// network lifetime itself, so the default is a no-op.
    ///
    /// Compose_apply's parallel container creates would race
    /// each other on the per-container `ensure_bridge` call (concurrent
    /// `iptables-restore` invocations on the same chain collide). Doing
    /// a sequential pre-pass over the distinct network names before the
    /// parallel fan-out removes the race; the per-container create still
    /// re-checks the registry but the work is now a fast no-op.
    async fn ensure_network(&self, _network: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Ensure `container_ref` has a proxy-reachable ingress endpoint.
    ///
    /// The default implementation is conservative: it only inspects the container
    /// and uses an existing `isengard-proxy` IP when present. Backends that can
    /// mutate networking should override this method.
    async fn ensure_ingress_attachment(
        &self,
        container_ref: &str,
    ) -> Result<IngressEndpoint, RuntimeError> {
        let Some(snapshot) = self.inspect_container(container_ref).await? else {
            return Ok(IngressEndpoint::Unresolved(
                UnresolvedIngressReason::ContainerMissing,
            ));
        };
        if snapshot.state != ContainerState::Running {
            return Ok(IngressEndpoint::Unresolved(
                UnresolvedIngressReason::ContainerStopped,
            ));
        }
        if let Some(ip) = snapshot.network_settings.ip_addresses.get("isengard-proxy") {
            return Ok(IngressEndpoint::Ready {
                ip: *ip,
                mode: IngressEndpointMode::IsengardNetwork,
            });
        }
        Ok(IngressEndpoint::Unresolved(
            UnresolvedIngressReason::NoUsableContainerIp,
        ))
    }

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

    /// Borrow the underlying bollard handle. Docker is now the only
    /// backend, so this always returns `Some`. Kept as a trait method
    /// because several internal helpers (compose_apply,
    /// deployment/driver, labels, the log decoder, proxy IP discovery)
    /// still call bollard directly; rewriting them onto the trait is a
    /// later cleanup.
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
