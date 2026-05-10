//! Phase 0.4 dispatch A3 stub. The full BollardBackend implementation lands
//! in commit A3; A2 only needs the type to exist so [`super::select`] can
//! construct it.

use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;

use super::{
    ContainerCreateSpec, ContainerSnapshot, HealthState, HealthcheckSpec, ListFilter, LogChunk,
    LogOptions, RuntimeBackend, RuntimeError, RuntimeEvent,
};

/// Bollard-backed [`super::RuntimeBackend`]. A2 ships only the constructor
/// stub; A3 fills in every trait method by extracting the bollard call
/// surface from compose_apply / logs / deployment/driver / labels / etc.
#[derive(Debug)]
pub struct BollardBackend {
    pub(crate) docker: std::sync::Arc<bollard::Docker>,
    #[allow(dead_code)] // wisp backend will need this; bollard backend doesn't yet
    pub(crate) state_dir: std::path::PathBuf,
}

impl BollardBackend {
    /// Connect to dockerd via the local default endpoint (matches the
    /// existing agent.rs behavior), wrap it in an Arc, and return a
    /// backend ready for the trait impls A3 will add.
    pub async fn from_env(state_dir: &Path) -> Result<Self, RuntimeError> {
        let docker = bollard::Docker::connect_with_local_defaults()
            .map_err(|e| RuntimeError::Docker(format!("connect_with_local_defaults: {e}")))?;
        Ok(Self {
            docker: std::sync::Arc::new(docker),
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// Borrow the underlying bollard handle. Phase 0.4 dispatch A4 keeps
    /// internal call sites in compose_apply / deployment/driver using the
    /// raw handle while the trait methods cover the surface used by lib.rs
    /// + sync.rs + the WispBackend translation in dispatch B.
    pub fn docker(&self) -> std::sync::Arc<bollard::Docker> {
        self.docker.clone()
    }
}

/// Stub trait impl. Phase 0.4 dispatch A2 ships this so [`super::select`]
/// can produce an `Arc<dyn RuntimeBackend>` for the docker arm; dispatch
/// A3 replaces every method with the real bollard call extracted from
/// compose_apply / logs / deployment/driver / labels.
#[async_trait]
impl RuntimeBackend for BollardBackend {
    async fn ensure_image(&self, _reference: &str) -> Result<String, RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::ensure_image not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn create_container(&self, _spec: &ContainerCreateSpec) -> Result<String, RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::create_container not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::start_container not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn stop_container(&self, _id: &str, _timeout_s: u32) -> Result<(), RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::stop_container not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn remove_container(&self, _id: &str, _force: bool) -> Result<(), RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::remove_container not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn list_containers(
        &self,
        _filter: ListFilter,
    ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::list_containers not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn inspect_container(
        &self,
        _id: &str,
    ) -> Result<Option<ContainerSnapshot>, RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::inspect_container not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn connect_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::connect_network not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    async fn disconnect_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Docker(
            "BollardBackend::disconnect_network not implemented (Phase 0.4 dispatch A3)".into(),
        ))
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
        Err(RuntimeError::Healthcheck(
            "BollardBackend::run_healthcheck not implemented (Phase 0.4 dispatch A3)".into(),
        ))
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}
