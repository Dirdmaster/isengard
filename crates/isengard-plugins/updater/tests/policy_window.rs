//! Phase 9d integration tests for the maintenance-window decision path.
//!
//! See plan §"T3: do_cycle integration + event" of
//! `docs/superpowers/plans/2026-05-06-phase-9d-maintenance-windows.md`.
//!
//! Mirrors the harness used by `policy_skip.rs`: storage-backed
//! `InventoryPolicyLoader`, no Docker, decision-only assertions. Real
//! container recreation is covered by `cycle_e2e.rs`; the window check
//! lands before recreate is reached, so unit-resolution at
//! `policy_decision` is sufficient for coverage.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use isengard_core::policy::{MaintenanceWindow, Policy, UpdateStrategy};
use isengard_core::{LoadedPolicy, PolicyLoader};
use isengard_plugin_updater::policy::{
    OwnedPolicyContext, PolicyDecision, SkipReason, policy_context_from_container, policy_decision,
};
use isengard_storage::policy::{InsertPolicy, PolicyScopeType};
use isengard_storage::{Inventory, InventoryPolicyLoader};

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

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

/// Phase 9d test 1: in-window cycle proceeds. The window's previous firing
/// (Sunday 02:00 UTC) is 30 minutes ago; the resolver still considers
/// `now` in window.
#[tokio::test]
async fn in_window_cycle_proceeds() {
    let loader = loader_with_policies(vec![(
        PolicyScopeType::Service,
        "web",
        Policy {
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        },
    )])
    .await;
    let snapshot = loader.list().await.expect("list");

    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");
    // 2026-05-03 is a Sunday. 02:30 UTC is 30 min after the 02:00 firing.
    let now = Utc.with_ymd_and_hms(2026, 5, 3, 2, 30, 0).unwrap();
    let (_, decision) = policy_decision(&snapshot, &owned.as_ref(), None, now);

    assert_eq!(decision, PolicyDecision::Proceed);
}

/// Phase 9d test 2: outside-window cycle returns `Deferred` with the
/// upcoming firing as `next_window`. Mirrors what the cycle would emit on
/// `update.deferred`.
#[tokio::test]
async fn outside_window_cycle_returns_deferred() {
    let loader = loader_with_policies(vec![(
        PolicyScopeType::Service,
        "web",
        Policy {
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        },
    )])
    .await;
    let snapshot = loader.list().await.expect("list");

    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");
    // Tuesday 12:00 UTC: outside the Sunday 02:00 window. Next Sunday is
    // 2026-05-10 02:00 UTC.
    let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
    let (_, decision) = policy_decision(&snapshot, &owned.as_ref(), None, now);

    match decision {
        PolicyDecision::Deferred { next_window } => {
            let next = next_window.expect("next_window populated");
            assert_eq!(next, Utc.with_ymd_and_hms(2026, 5, 10, 2, 0, 0).unwrap());
        }
        other => panic!("expected Deferred, got {other:?}"),
    }
}

/// Phase 9d test 3: Pinned wins over outside-window. The cycle emits
/// `update.policy_skipped(reason=pinned)`, NOT `update.deferred`.
#[tokio::test]
async fn pinned_wins_over_outside_window() {
    let loader = loader_with_policies(vec![(
        PolicyScopeType::Service,
        "web",
        Policy {
            strategy: Some(UpdateStrategy::Pinned),
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        },
    )])
    .await;
    let snapshot = loader.list().await.expect("list");

    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");
    let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
    let (_, decision) = policy_decision(&snapshot, &owned.as_ref(), None, now);

    assert_eq!(decision, PolicyDecision::Skip(SkipReason::Pinned));
}

/// Phase 9d test 4: a malformed cron is fail-closed: the window check
/// returns `false` so the cycle defers (rather than letting an unparseable
/// row open a back door to unconstrained updates). The `next_window` is
/// `None` because `next_window_after` returns None on parse error.
#[tokio::test]
async fn malformed_cron_defers_with_no_next_window() {
    let loader = loader_with_policies(vec![(
        PolicyScopeType::Service,
        "web",
        Policy {
            window: Some(MaintenanceWindow {
                cron_expr: "definitely not a cron".to_string(),
                timezone: None,
            }),
            ..Default::default()
        },
    )])
    .await;
    let snapshot: Vec<LoadedPolicy> = loader.list().await.expect("list");

    let owned = ctx_for_compose_service("prod", "blog", "web", "blog-web-1");
    let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
    let (_, decision) = policy_decision(&snapshot, &owned.as_ref(), None, now);

    match decision {
        PolicyDecision::Deferred { next_window } => {
            assert!(next_window.is_none());
        }
        other => panic!("expected Deferred (fail-closed), got {other:?}"),
    }
}
