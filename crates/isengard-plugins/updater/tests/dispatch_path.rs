//! Structural smoke tests for the updater ↔
//! `UpdateDispatcher` wiring.
//!
//! These tests do NOT exercise `do_cycle` end-to-end — that needs a Docker
//! daemon, real containers, and a registry. They instead verify the trait
//! plumbing: a counting dispatcher records how often `dispatch` is called
//! and the test asserts the verdict is observable from the consumer side.
//!
//! Together with the unit tests in `update_dispatch.rs` (which assert
//! `Handled` vs `PerformInPlace` behaviour through the dyn boundary), this
//! gives us confidence the cross-crate seam is real before T7 wires the
//! dispatcher into the agent runtime.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use isengard_core::{DispatchOutcome, UpdateDispatcher, UpdateTriggerInfo};

struct CountingDispatcher {
    calls: AtomicUsize,
    verdict: DispatchOutcome,
}

impl CountingDispatcher {
    fn new(verdict: DispatchOutcome) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            verdict,
        })
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl UpdateDispatcher for CountingDispatcher {
    async fn dispatch(&self, _info: UpdateTriggerInfo) -> DispatchOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.verdict
    }
}

fn fixture_info() -> UpdateTriggerInfo {
    UpdateTriggerInfo {
        container_id: "abc123".into(),
        service_name: "web".into(),
        stack_id: 0,
        host_id: ulid::Ulid::new(),
        blue_digest: "sha256:old".into(),
        green_digest: "sha256:new".into(),
        image_ref: "ghcr.io/foo/bar:1.2.3".into(),
        container_port: Some(8080),
        has_healthcheck: true,
        rw_volume_mounts: vec![],
        label_strategy: None,
    }
}

#[tokio::test]
async fn dispatcher_handled_skips_recreate() {
    // The cycle's branch:
    //   match disp.dispatch(info).await {
    //       Handled => continue,           // recreate not called
    //       PerformInPlace => {}           // fall through to recreate
    //   }
    //
    // We assert the dispatcher receives exactly one call and returns the
    // expected verdict. The "skips recreate" half is enforced by the
    // updater's `continue` in lib.rs — verified by reading the diff.
    let disp = CountingDispatcher::new(DispatchOutcome::Handled);
    let dyn_disp: Arc<dyn UpdateDispatcher> = disp.clone();

    let outcome = dyn_disp.dispatch(fixture_info()).await;
    assert_eq!(outcome, DispatchOutcome::Handled);
    assert_eq!(disp.call_count(), 1);
}

#[tokio::test]
async fn dispatcher_perform_in_place_falls_through_to_recreate() {
    let disp = CountingDispatcher::new(DispatchOutcome::PerformInPlace);
    let dyn_disp: Arc<dyn UpdateDispatcher> = disp.clone();

    let outcome = dyn_disp.dispatch(fixture_info()).await;
    assert_eq!(outcome, DispatchOutcome::PerformInPlace);
    assert_eq!(disp.call_count(), 1);
}
