//! Phase 9e T2 integration tests for the updater's `gate=Approval` branch.
//!
//! See plan §"T2: Updater integration" of
//! `docs/superpowers/plans/2026-05-06-phase-9e-9f-approval-flow.md`.
//!
//! These tests do not run a real Docker daemon. The full `do_cycle` path
//! requires bollard + a registry + actual containers; the existing
//! `cycle_e2e.rs` test owns that coverage. T2 tests focus on the seam the
//! cycle relies on:
//!
//! 1. Resolver returns `gate=Approval` for the candidate.
//! 2. `decision_from_resolved` with both digests yields `PendingApproval(body)`.
//! 3. The `ApprovalStore` (production `InventoryApprovalStore`) persists
//!    the row, dedupes the next call, and surfaces `decide_pending_approval`
//!    output to the caller.
//!
//! The "did the cycle skip the recreate?" assertion is structural in the
//! lib.rs source: the `PolicyDecision::PendingApproval` arm in `do_cycle`
//! is a `continue`. The integration tests assert the storage state.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};
use isengard_core::approval_store::{ApprovalStore, PendingApprovalBody};
use isengard_core::policy::{Policy, UpdateGate};
use isengard_core::{HostId, LoadedPolicy, PolicyLoader};
use isengard_plugin_updater::policy::{
    ApprovalContext, OwnedPolicyContext, PolicyDecision, decision_from_resolved,
    policy_context_from_container, policy_decision,
};
use isengard_storage::host_action::{
    ApprovalDecision, ApprovalFilter, ApprovalState, ApprovalStateFilter,
};
use isengard_storage::policy::{InsertPolicy, PolicyScopeType};
use isengard_storage::{EnrollHost, Inventory, InventoryApprovalStore, InventoryPolicyLoader};

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

/// Assemble the test world: an in-memory `Inventory`, an enrolled host,
/// a service-scoped `gate=Approval` policy, plus the production
/// `InventoryPolicyLoader` and `InventoryApprovalStore` wrapping it.
struct World {
    inv: Arc<Inventory>,
    host_id: HostId,
    policy_loader: Arc<InventoryPolicyLoader>,
    approval_store: Arc<InventoryApprovalStore>,
}

async fn setup_world() -> World {
    let inv = Inventory::open_in_memory().await.expect("open inv");
    let storage_host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            fleet: "default".into(),
        })
        .await
        .expect("enroll");
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Service,
        scope_key: "web".into(),
        body: Policy {
            gate: Some(UpdateGate::Approval),
            approver_channel: Some("ops-team".into()),
            ..Default::default()
        },
    })
    .await
    .expect("insert policy");
    let inv = Arc::new(inv);
    let policy_loader = Arc::new(InventoryPolicyLoader::new(inv.clone()));
    let approval_store = Arc::new(InventoryApprovalStore::new(inv.clone()));
    World {
        inv,
        host_id: storage_host.0,
        policy_loader,
        approval_store,
    }
}

fn ctx_for(host_id: HostId) -> OwnedPolicyContext {
    let l = labels(&[
        ("isengard.enable", "true"),
        ("com.docker.compose.project", "blog"),
        ("com.docker.compose.service", "web"),
    ]);
    let host_str = host_id.to_string();
    // Need to pass the host_id_hex via an owned string; the helper takes
    // a `&str` so we materialise the OwnedPolicyContext with a leak-free
    // round-trip by way of the helper.
    policy_context_from_container(
        Some(&l),
        Some("default"),
        Some(host_str.as_str()),
        "blog_web_1",
    )
}

fn approval_ctx<'a>() -> ApprovalContext<'a> {
    ApprovalContext {
        image: "ghcr.io/owner/repo:v1",
        current_digest: "sha256:cur",
        proposed_digest: "sha256:new",
    }
}

/// Persist + emit logic mirroring `do_cycle`'s `PendingApproval` arm.
/// Returns `(persisted, deduped)` tracking which counter would tick.
async fn run_cycle_decision(world: &World) -> (bool, bool) {
    let snapshot = world.policy_loader.list().await.expect("list");
    let owned = ctx_for(world.host_id);
    let projected: Vec<_> = snapshot
        .iter()
        .map(|r: &LoadedPolicy| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let resolved = isengard_core::policy::resolve_policy(&projected, &owned.as_ref());
    // Early-cycle skip pass: must be Proceed (no Pinned, no Paused).
    let early = decision_from_resolved(&resolved, &owned.as_ref(), None, Utc::now());
    assert!(matches!(early, PolicyDecision::Proceed));
    // Post-digest pass: gate=Approval must yield PendingApproval.
    let post = decision_from_resolved(&resolved, &owned.as_ref(), Some(approval_ctx()), Utc::now());
    let body: PendingApprovalBody = match post {
        PolicyDecision::PendingApproval(b) => b,
        other => panic!("expected PendingApproval, got {other:?}"),
    };

    // Mirror do_cycle's helper: dedup-then-insert.
    let dedup = world
        .approval_store
        .find_open_approval_for_proposed_digest(
            body.host_id,
            &body.stack,
            &body.service,
            &body.proposed_digest,
        )
        .await
        .expect("dedup lookup");
    if dedup.is_some() {
        return (false, true);
    }

    let approver = body.approver_channel.clone();
    world
        .approval_store
        .insert_pending_approval(isengard_core::InsertPendingApproval {
            body,
            expires_at: Utc::now() + ChronoDuration::hours(24),
            approver_channel: approver,
        })
        .await
        .expect("insert");
    (true, false)
}

/// T2 test 1: the first cycle on a `gate=Approval` service persists a
/// pending row.
#[tokio::test]
async fn first_cycle_persists_pending_approval() {
    let world = setup_world().await;
    let (persisted, deduped) = run_cycle_decision(&world).await;
    assert!(persisted, "first cycle should persist");
    assert!(!deduped);

    let rows = world
        .inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::Open),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(rows.len(), 1, "exactly one open row");
    let row = &rows[0];
    assert_eq!(row.state, ApprovalState::PendingOpen);
    assert_eq!(row.body.host_id.0, world.host_id);
    assert_eq!(row.body.stack, "blog");
    assert_eq!(row.body.service, "web");
    assert_eq!(row.body.image, "ghcr.io/owner/repo:v1");
    assert_eq!(row.body.current_digest, "sha256:cur");
    assert_eq!(row.body.proposed_digest, "sha256:new");
    assert_eq!(row.body.approver_channel.as_deref(), Some("ops-team"));
    // GHCR diff URL populated for ghcr.io/<owner>/<repo>.
    assert_eq!(
        row.body.diff_url.as_deref(),
        Some("https://github.com/owner/repo/compare/sha256:cur...sha256:new")
    );
}

/// T2 test 2: the second cycle dedupes; no duplicate row created.
#[tokio::test]
async fn second_cycle_dedups_no_duplicate_row() {
    let world = setup_world().await;
    let (p1, d1) = run_cycle_decision(&world).await;
    assert!(p1 && !d1);
    let (p2, d2) = run_cycle_decision(&world).await;
    assert!(!p2, "second cycle must NOT persist");
    assert!(d2, "second cycle must dedupe");

    let rows = world
        .inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(rows.len(), 1, "still exactly one row across two cycles");
}

/// T2 test 3: approving the action via the storage DAO transitions the row
/// to `pending_approved`. The next cycle's dedupe lookup excludes the
/// approved row (it's no longer `pending_open`), so a brand-new row would
/// be persisted IF the digest were still in the approval state. Here we
/// assert the dedupe lookup correctly returns `None` after approval.
#[tokio::test]
async fn approve_action_clears_dedupe_window() {
    let world = setup_world().await;
    let (p, _) = run_cycle_decision(&world).await;
    assert!(p);

    let rows = world
        .inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::Open),
            ..Default::default()
        })
        .await
        .expect("list");
    let action_id = rows[0].action_id.clone();

    // Approve via the DAO. After this, the row is pending_approved and
    // the dedupe lookup (which filters state=pending_open) sees nothing.
    let decided = world
        .inv
        .decide_pending_approval(&action_id, ApprovalDecision::Approve, "test:operator")
        .await
        .expect("approve");
    assert_eq!(decided.row.state, ApprovalState::PendingApproved);
    assert!(decided.should_dispatch_apply_update);

    // Dedupe lookup against the SAME proposed_digest must return None
    // because the only matching row is no longer `pending_open`.
    let owned = ctx_for(world.host_id);
    let snapshot = world.policy_loader.list().await.expect("list");
    let projected: Vec<_> = snapshot
        .iter()
        .map(|r: &LoadedPolicy| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let resolved = isengard_core::policy::resolve_policy(&projected, &owned.as_ref());
    let post = decision_from_resolved(&resolved, &owned.as_ref(), Some(approval_ctx()), Utc::now());
    let body = match post {
        PolicyDecision::PendingApproval(b) => b,
        other => panic!("expected PendingApproval, got {other:?}"),
    };
    let still_open = world
        .approval_store
        .find_open_approval_for_proposed_digest(
            body.host_id,
            &body.stack,
            &body.service,
            &body.proposed_digest,
        )
        .await
        .expect("lookup");
    assert!(
        still_open.is_none(),
        "approved rows must not block new cycles"
    );
}

/// T2 test 4: rejecting the action also clears the dedupe window. The
/// next cycle would create a NEW pending row for the same digest (the
/// resolver still says `gate=Approval`). This is intentional: rejecting
/// a single proposal doesn't permanently disable the gate. Operators
/// who want to silence the prompt use Snooze (which writes
/// `paused_until` on the policy) instead.
#[tokio::test]
async fn reject_action_clears_dedupe_window() {
    let world = setup_world().await;
    let (p, _) = run_cycle_decision(&world).await;
    assert!(p);

    let rows = world
        .inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::Open),
            ..Default::default()
        })
        .await
        .expect("list");
    let action_id = rows[0].action_id.clone();
    let decided = world
        .inv
        .decide_pending_approval(&action_id, ApprovalDecision::Reject, "test:operator")
        .await
        .expect("reject");
    assert_eq!(decided.row.state, ApprovalState::PendingRejected);
    assert!(!decided.should_dispatch_apply_update);

    // Same shape as the approve test: the open-row dedupe must miss.
    let owned = ctx_for(world.host_id);
    let snapshot = world.policy_loader.list().await.expect("list");
    let projected: Vec<_> = snapshot
        .iter()
        .map(|r: &LoadedPolicy| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let resolved = isengard_core::policy::resolve_policy(&projected, &owned.as_ref());
    let post = decision_from_resolved(&resolved, &owned.as_ref(), Some(approval_ctx()), Utc::now());
    let body = match post {
        PolicyDecision::PendingApproval(b) => b,
        other => panic!("expected PendingApproval, got {other:?}"),
    };
    let still_open = world
        .approval_store
        .find_open_approval_for_proposed_digest(
            body.host_id,
            &body.stack,
            &body.service,
            &body.proposed_digest,
        )
        .await
        .expect("lookup");
    assert!(
        still_open.is_none(),
        "rejected rows must not block new cycles"
    );

    // And running the cycle helper again now persists a fresh row.
    let (p2, d2) = run_cycle_decision(&world).await;
    assert!(p2, "post-reject cycle must persist a new row");
    assert!(!d2);
    let all = world
        .inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(all.len(), 2, "rejected + new pending = 2 rows");
}

/// Sanity: `policy_decision` (the convenience helper) yields the same
/// outcome as the explicit resolve + `decision_from_resolved` pair.
#[tokio::test]
async fn policy_decision_helper_matches_explicit_path() {
    let world = setup_world().await;
    let snapshot = world.policy_loader.list().await.expect("list");
    let owned = ctx_for(world.host_id);
    let now = Utc::now();
    let (_, helper) = policy_decision(&snapshot, &owned.as_ref(), Some(approval_ctx()), now);
    let projected: Vec<_> = snapshot
        .iter()
        .map(|r: &LoadedPolicy| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let resolved = isengard_core::policy::resolve_policy(&projected, &owned.as_ref());
    let direct = decision_from_resolved(&resolved, &owned.as_ref(), Some(approval_ctx()), now);
    // PendingApproval body equality: both paths build the same struct.
    match (helper, direct) {
        (PolicyDecision::PendingApproval(a), PolicyDecision::PendingApproval(b)) => {
            assert_eq!(a, b);
        }
        (h, d) => panic!("expected matching PendingApproval; got {h:?} / {d:?}"),
    }
}
