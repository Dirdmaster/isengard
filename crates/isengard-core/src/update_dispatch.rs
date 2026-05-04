//! `UpdateDispatcher`: cross-crate seam between the updater plugin and the
//! agent's blue-green deployment supervisor.
//!
//! The updater (a `isengard-plugin-updater` crate) detects that a container's
//! image has drifted from the registry and wants to react. By default it falls
//! through to in-place recreate. When the agent runtime installs a dispatcher
//! into [`crate::PluginContext`], the updater hands it the trigger first; the
//! dispatcher decides whether to short-circuit (because it has spawned a
//! blue-green driver of its own) or let the in-place path proceed.
//!
//! Only one direction of information crosses the boundary today: the updater
//! → dispatcher call. The dispatcher does not call back into the updater; it
//! owns its own driver task lifecycle once it returns [`DispatchOutcome::Handled`].

use crate::HostId;

/// Everything the dispatcher needs to decide between blue-green and in-place,
/// and (in the blue-green case) to seed an [`UpdateTrigger`] for the
/// agent-side supervisor without re-inspecting the container.
///
/// Built by the updater from its existing bollard inspect data. `stack_id`
/// may be `0` when the updater can't resolve the compose project to a
/// `Stack` row; the dispatcher is expected to handle that gracefully.
#[derive(Debug, Clone)]
pub struct UpdateTriggerInfo {
    pub container_id: String,
    pub service_name: String,
    /// Best-effort stack ID; `0` if the updater could not resolve the
    /// `com.docker.compose.project` label to a `Stack` row.
    pub stack_id: i64,
    pub host_id: HostId,
    pub blue_digest: String,
    pub green_digest: String,
    pub image_ref: String,
    /// First (lowest-numbered) container port exposed by the inspect data.
    /// `None` when the container exposes no ports.
    pub container_port: Option<u16>,
    /// True when the container has a Docker healthcheck configured (or is
    /// reporting a health state).
    pub has_healthcheck: bool,
    /// Read-write bind mounts and named volumes. Used by the eligibility
    /// classifier to disqualify stateful workloads from blue-green.
    pub rw_volume_mounts: Vec<String>,
    /// Value of the `isengard.deploy.strategy` label, if set. Allows the
    /// classifier to honour an explicit user override.
    pub label_strategy: Option<String>,
}

/// What the dispatcher told the updater to do with this trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Run the existing in-place recreate path. The dispatcher either
    /// classified the container as ineligible for blue-green or could not
    /// gather enough context (e.g. routing-rule lookup failed).
    PerformInPlace,
    /// The dispatcher has taken ownership: a blue-green driver is now
    /// running (or one was already in flight). The updater MUST NOT
    /// recreate.
    Handled,
}

/// Async sink the updater calls per `needs_update` container before
/// recreating. Implementations live in higher crates (e.g. the agent's
/// `SupervisorDispatcher`).
#[async_trait::async_trait]
pub trait UpdateDispatcher: Send + Sync + 'static {
    async fn dispatch(&self, info: UpdateTriggerInfo) -> DispatchOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingDispatcher {
        calls: AtomicUsize,
        verdict: DispatchOutcome,
    }

    #[async_trait::async_trait]
    impl UpdateDispatcher for CountingDispatcher {
        async fn dispatch(&self, _info: UpdateTriggerInfo) -> DispatchOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict
        }
    }

    fn sample_info() -> UpdateTriggerInfo {
        UpdateTriggerInfo {
            container_id: "deadbeef".into(),
            service_name: "web".into(),
            stack_id: 0,
            host_id: ulid::Ulid::new(),
            blue_digest: "sha256:aaa".into(),
            green_digest: "sha256:bbb".into(),
            image_ref: "ghcr.io/foo/bar:1.0".into(),
            container_port: Some(8080),
            has_healthcheck: true,
            rw_volume_mounts: vec![],
            label_strategy: None,
        }
    }

    #[tokio::test]
    async fn dispatcher_returns_handled() {
        let d = Arc::new(CountingDispatcher {
            calls: AtomicUsize::new(0),
            verdict: DispatchOutcome::Handled,
        });
        let dyn_d: Arc<dyn UpdateDispatcher> = d.clone();
        assert_eq!(
            dyn_d.dispatch(sample_info()).await,
            DispatchOutcome::Handled
        );
        assert_eq!(d.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatcher_returns_perform_in_place() {
        let d = Arc::new(CountingDispatcher {
            calls: AtomicUsize::new(0),
            verdict: DispatchOutcome::PerformInPlace,
        });
        let dyn_d: Arc<dyn UpdateDispatcher> = d.clone();
        assert_eq!(
            dyn_d.dispatch(sample_info()).await,
            DispatchOutcome::PerformInPlace
        );
    }
}
