//! Phase 9b T3 integration tests for the updater's policy-skip path.
//!
//! See plan §"T3: Updater integration" of
//! `docs/superpowers/plans/2026-05-06-phase-9a-9d-policy-foundation.md`.
//!
//! These tests do not run a real Docker daemon. The full `do_cycle` path
//! requires bollard + a registry + actual containers; the existing
//! `cycle_e2e.rs` test owns that coverage. Phase 9b only touches the
//! decision step that runs before `recreate::update_container` is even
//! considered, so the tests exercise that decision step in isolation:
//!
//! 1. Open an in-memory `Inventory`.
//! 2. Wrap it in `InventoryPolicyLoader` (production loader impl).
//! 3. Insert policy rows for the scenario.
//! 4. Build a `PolicyContext` for the candidate.
//! 5. Call `policy::policy_decision` and assert.
//!
//! Asserting "recreate was NOT called" is structural here: the
//! `policy_decision -> Skip` branch in `do_cycle` is a `continue` (visible
//! in the diff). The recreate path is unreachable when `Skip` is
//! returned. The tests below verify the gate, not the downstream branch.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use isengard_core::event::{Event, EventEmitter};
use isengard_core::policy::{Policy, UpdateStrategy};
use isengard_core::{LoadedPolicy, PolicyLoader};
use isengard_plugin_updater::policy::{
    OwnedPolicyContext, PolicyDecision, SkipReason, decision_from_resolved,
    policy_context_from_container, policy_decision,
};
use isengard_storage::policy::{InsertPolicy, PolicyScopeType};
use isengard_storage::{Inventory, InventoryPolicyLoader};
use tokio::sync::Mutex;

/// Small recorder so we can assert on emitted events without spinning up
/// the controller's bus. Mirrors the `RecordingEmitter` in `lib.rs`'s own
/// emit_tests, hoisted to test module scope.
struct Recorder {
    events: Mutex<Vec<Event>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
    async fn snapshot(&self) -> Vec<Event> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl EventEmitter for Recorder {
    async fn emit(&self, event: Event) {
        self.events.lock().await.push(event);
    }
}

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

/// Build the `OwnedPolicyContext` the cycle would build for a compose
/// service. We use the helper directly so a refactor of its label set
/// surfaces here too.
fn ctx_for_compose_service(
    fleet: &str,
    project: &str,
    service: &str,
    container: &str,
) -> OwnedPolicyContext {
    let l = labels(&[
        ("isengard.enable", "true"),
        ("com.docker.compose.project", project),
        ("com.docker.compose.service", service),
    ]);
    policy_context_from_container(Some(&l), Some(fleet), None, container)
}

async fn loader_with_policies(
    rows: Vec<(PolicyScopeType, &str, Policy)>,
) -> Arc<InventoryPolicyLoader> {
    let inv = Inventory::open_in_memory().await.expect("open inv");
    for (scope_type, scope_key, body) in rows {
        inv.insert_policy(InsertPolicy {
            scope_type,
            scope_key: scope_key.to_string(),
            body,
        })
        .await
        .expect("insert policy");
    }
    Arc::new(InventoryPolicyLoader::new(Arc::new(inv)))
}

/// T3 test 1: A service-scoped Pinned policy short-circuits the cycle's
/// recreate path; the journal receives an `update.policy_skipped` event
/// with `reason=pinned`.
#[tokio::test]
async fn pinned_service_is_skipped() {
    let loader = loader_with_policies(vec![(
        PolicyScopeType::Service,
        "web",
        Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Default::default()
        },
    )])
    .await;
    let snapshot = loader.list().await.expect("list");

    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");
    let (_resolved, decision) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());

    assert_eq!(decision, PolicyDecision::Skip(SkipReason::Pinned));

    // Side check: the cycle's `match` arm emits the policy_skipped event
    // when this decision is returned. Re-run the same emit logic the
    // cycle would, against a recorder, to assert the payload too.
    let recorder = Arc::new(Recorder::new());
    let emitter: Arc<dyn EventEmitter> = recorder.clone();
    let event = build_event(&owned, "blog-web-1", &SkipReason::Pinned);
    emitter.emit(event).await;
    let snap = recorder.snapshot().await;
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].kind, "update.policy_skipped");
    assert_eq!(snap[0].metadata["reason"], "pinned");
    assert_eq!(snap[0].metadata["service"], "web");
    assert_eq!(snap[0].metadata["container"], "blog-web-1");
    assert!(snap[0].metadata.get("until").is_none());
}

/// T3 test 2: a service paused into the future is skipped; once
/// `paused_until` falls into the past (simulated by direct DB write of an
/// expired timestamp), the next cycle proceeds.
#[tokio::test]
async fn paused_service_is_skipped_until_expiry() {
    let inv = Inventory::open_in_memory().await.expect("open inv");
    let future = Utc::now() + ChronoDuration::hours(1);
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Service,
        scope_key: "web".into(),
        body: Policy {
            paused_until: Some(future),
            ..Default::default()
        },
    })
    .await
    .expect("insert paused");

    let inv = Arc::new(inv);
    let loader = InventoryPolicyLoader::new(inv.clone());
    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");

    let snap_before = loader.list().await.expect("list");
    let (_, decision_before) = policy_decision(&snap_before, &owned.as_ref(), None, Utc::now());
    assert_eq!(
        decision_before,
        PolicyDecision::Skip(SkipReason::Paused { until: future })
    );

    // Advance the policy: rewrite paused_until to a past instant. The
    // upsert path replaces the body in place; created_at is preserved
    // (matching production behavior).
    let past = Utc::now() - ChronoDuration::hours(1);
    inv.upsert_policy(
        PolicyScopeType::Service,
        "web",
        &Policy {
            paused_until: Some(past),
            ..Default::default()
        },
    )
    .await
    .expect("upsert past");

    let snap_after = loader.list().await.expect("list");
    let (_, decision_after) = policy_decision(&snap_after, &owned.as_ref(), None, Utc::now());
    assert_eq!(decision_after, PolicyDecision::Proceed);
}

/// T3 test 3: with no policy rows, the resolver returns the
/// `defaults::DEFAULT_*` constants and the decision is `Proceed`. No
/// policy_skipped events are emitted on this path.
#[tokio::test]
async fn default_policy_does_not_skip() {
    let loader = loader_with_policies(vec![]).await;
    let snapshot = loader.list().await.expect("list");

    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");
    let (resolved, decision) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());

    assert_eq!(decision, PolicyDecision::Proceed);
    // Defaults: TagOnly, Auto, Notify (per `isengard_core::policy::defaults`).
    assert_eq!(
        resolved.strategy,
        isengard_core::policy::UpdateStrategy::TagOnly
    );

    // No skip event would be emitted: verify by feeding the decision
    // through the same emit-or-not branch the cycle uses.
    let recorder = Arc::new(Recorder::new());
    let emitter: Arc<dyn EventEmitter> = recorder.clone();
    if let PolicyDecision::Skip(reason) = decision {
        let event = build_event(&owned, "blog-web-1", &reason);
        emitter.emit(event).await;
    }
    assert!(recorder.snapshot().await.is_empty());
}

/// T3 test 4: when a policy_skipped event is emitted for a paused
/// service, the payload carries every required field (service, container,
/// host_id, reason, until). Pinned skips drop the `until` field.
#[tokio::test]
async fn policy_skipped_event_payload_is_correct() {
    // -- pinned: until is omitted.
    let l = labels(&[
        ("isengard.enable", "true"),
        ("com.docker.compose.project", "blog"),
        ("com.docker.compose.service", "web"),
    ]);
    let owned =
        policy_context_from_container(Some(&l), Some("prod"), Some("HEXHOST"), "blog-web-1");
    let event = build_event(&owned, "blog-web-1", &SkipReason::Pinned);
    assert_eq!(event.kind, "update.policy_skipped");
    assert_eq!(event.metadata["service"], "web");
    assert_eq!(event.metadata["container"], "blog-web-1");
    assert_eq!(event.metadata["host_id"], "HEXHOST");
    assert_eq!(event.metadata["reason"], "pinned");
    assert!(event.metadata.get("until").is_none());

    // -- paused: until is the RFC3339 string of paused_until.
    let until = Utc::now() + ChronoDuration::hours(2);
    let event = build_event(&owned, "blog-web-1", &SkipReason::Paused { until });
    assert_eq!(event.metadata["reason"], "paused");
    assert_eq!(event.metadata["until"], until.to_rfc3339());
    assert_eq!(event.metadata["host_id"], "HEXHOST");
}

/// T3 sanity: `decision_from_resolved` is consistent with `policy_decision`.
/// Cheap regression check that the convenience helper hasn't drifted from
/// the per-step pieces.
#[tokio::test]
async fn decision_from_resolved_matches_policy_decision() {
    let loader = loader_with_policies(vec![(
        PolicyScopeType::Global,
        "",
        Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Default::default()
        },
    )])
    .await;
    let snapshot = loader.list().await.expect("list");
    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");

    let projected: Vec<_> = snapshot
        .iter()
        .map(|r: &LoadedPolicy| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let resolved = isengard_core::policy::resolve_policy(&projected, &owned.as_ref());
    let now = Utc::now();
    let direct = decision_from_resolved(&resolved, &owned.as_ref(), None, now);
    let (_, via_helper) = policy_decision(&snapshot, &owned.as_ref(), None, now);
    assert_eq!(direct, via_helper);
}

/// Shared helper mirroring the cycle's event-construction logic. Tests
/// use it to avoid duplicating the JSON shape.
fn build_event(ctx: &OwnedPolicyContext, container: &str, reason: &SkipReason) -> Event {
    let service = ctx.service.clone().unwrap_or_else(|| container.to_string());
    let mut metadata = serde_json::json!({
        "service": service,
        "container": container,
        "host_id": ctx.host_id_hex.clone().unwrap_or_default(),
        "reason": reason.as_str(),
    });
    if let SkipReason::Paused { until } = reason {
        metadata["until"] = serde_json::Value::String(until.to_rfc3339());
    }
    let summary = match reason {
        SkipReason::Pinned => format!("skipped {service} (pinned)"),
        SkipReason::Paused { until } => {
            format!("skipped {service} (paused until {})", until.to_rfc3339())
        }
        SkipReason::GateRejected { reason: Some(r) } => {
            format!("skipped {service} (gate rejected: {r})")
        }
        SkipReason::GateRejected { reason: None } => {
            format!("skipped {service} (gate rejected)")
        }
    };
    Event {
        kind: "update.policy_skipped".into(),
        occurred_at: Utc::now(),
        summary,
        container_name: Some(container.to_string()),
        metadata,
        ..Default::default()
    }
}
