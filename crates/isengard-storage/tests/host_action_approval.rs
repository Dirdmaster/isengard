//! Acceptance tests for the pending-approval DAO.
//!
//! See plan §"T1: Storage extensions for pending approvals" of
//! Approval queue storage tests.

use chrono::{Duration, Utc};
use isengard_storage::host_action::{
    APPROVAL_KIND, ApprovalDecision, ApprovalFilter, ApprovalState, ApprovalStateFilter,
    InsertPendingApproval, UpdateApprovalBody,
};
use isengard_storage::{EnrollHost, HostId, Inventory};

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

async fn enroll(inv: &Inventory, fingerprint: &str) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: fingerprint.into(),
        hostname: fingerprint.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
    })
    .await
    .expect("enroll")
}

fn body_for(
    host_id: HostId,
    stack: &str,
    service: &str,
    proposed_digest: &str,
) -> UpdateApprovalBody {
    UpdateApprovalBody {
        host_id,
        stack: stack.into(),
        service: service.into(),
        container_name: format!("{stack}_{service}_1"),
        image: "ghcr.io/example/web".into(),
        current_digest: "sha256:aaaaaaaa".into(),
        proposed_digest: proposed_digest.into(),
        diff_url: Some("https://example.com/diff".into()),
        approver_channel: Some("ops-team".into()),
    }
}

#[tokio::test]
async fn insert_then_get_round_trips_all_body_fields() {
    let inv = fresh_inv().await;
    let host_id = enroll(&inv, "h1").await;
    let body = body_for(host_id, "blog", "web", "sha256:bbbb");
    let inserted = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body.clone(),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: Some("ops-team".into()),
        })
        .await
        .expect("insert");

    assert_eq!(inserted.state, ApprovalState::PendingOpen);
    assert_eq!(inserted.body, body);
    assert!(inserted.decided_at.is_none());
    assert!(inserted.decided_by.is_none());
    assert_eq!(inserted.action_id.len(), 26, "ULID is 26 chars");

    let got = inv
        .get_pending_approval(&inserted.action_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got, inserted);
}

#[tokio::test]
async fn body_json_round_trip_preserves_diff_url_none_vs_some() {
    let inv = fresh_inv().await;
    let host_id = enroll(&inv, "h1").await;
    let mut body = body_for(host_id, "blog", "web", "sha256:zz");
    body.diff_url = None;
    body.approver_channel = None;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body.clone(),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .expect("insert");
    assert_eq!(row.body.diff_url, None);
    assert_eq!(row.body.approver_channel, None);

    // And a populated variant from a separate row.
    let body2 = body_for(host_id, "blog", "api", "sha256:yy");
    let row2 = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body2.clone(),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: Some("ops".into()),
        })
        .await
        .expect("insert");
    assert_eq!(
        row2.body.diff_url.as_deref(),
        Some("https://example.com/diff")
    );
    assert_eq!(row2.body.approver_channel.as_deref(), Some("ops-team"));
}

#[tokio::test]
async fn list_filters_by_state() {
    let inv = fresh_inv().await;
    let host_id = enroll(&inv, "h1").await;
    let a = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(host_id, "s", "svc-a", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    let _b = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(host_id, "s", "svc-b", "sha256:2"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    inv.decide_pending_approval(&a.action_id, ApprovalDecision::Approve, "tester")
        .await
        .unwrap();

    let open = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::Open),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].body.service, "svc-b");

    let decided = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::Decided),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].body.service, "svc-a");

    let all = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn list_filters_by_host_stack_service_and_orders_newest_first() {
    let inv = fresh_inv().await;
    let h1 = enroll(&inv, "h1").await;
    let h2 = enroll(&inv, "h2").await;

    let r1 = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h1, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    // Tiny gap so created_at differs.
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let r2 = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h1, "blog", "api", "sha256:2"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    let _r3 = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h2, "shop", "web", "sha256:3"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let only_h1 = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            host_id: Some(h1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(only_h1.len(), 2);
    assert_eq!(only_h1[0].action_id, r2.action_id, "newest first");
    assert_eq!(only_h1[1].action_id, r1.action_id);

    let only_blog = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            stack: Some("blog".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(only_blog.len(), 2);

    let only_web = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            service: Some("web".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(only_web.len(), 2);
    let services: std::collections::HashSet<_> =
        only_web.iter().map(|r| r.body.service.as_str()).collect();
    assert!(services.contains("web"));
    assert_eq!(services.len(), 1);
}

#[tokio::test]
async fn list_filters_by_since() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let _old = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "s", "old", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let cutoff = Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let new = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "s", "new", "sha256:2"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let recent = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::All),
            since: Some(cutoff),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].action_id, new.action_id);
}

#[tokio::test]
async fn decide_approve_transitions_and_returns_dispatch_flag() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let decided = inv
        .decide_pending_approval(&row.action_id, ApprovalDecision::Approve, "alice")
        .await
        .unwrap();
    assert_eq!(decided.row.state, ApprovalState::PendingApproved);
    assert_eq!(decided.row.decided_by.as_deref(), Some("alice"));
    assert!(decided.row.decided_at.is_some());
    assert!(decided.should_dispatch_apply_update);
}

#[tokio::test]
async fn decide_reject_transitions_without_dispatch() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let decided = inv
        .decide_pending_approval(&row.action_id, ApprovalDecision::Reject, "bob")
        .await
        .unwrap();
    assert_eq!(decided.row.state, ApprovalState::PendingRejected);
    assert!(!decided.should_dispatch_apply_update);
}

#[tokio::test]
async fn decide_snooze_transitions_to_snoozed_state() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let decided = inv
        .decide_pending_approval(&row.action_id, ApprovalDecision::SnoozeHours(24), "carol")
        .await
        .unwrap();
    assert_eq!(decided.row.state, ApprovalState::PendingSnoozed);
    assert!(!decided.should_dispatch_apply_update);
}

#[tokio::test]
async fn decide_rejects_when_not_pending_open() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    inv.decide_pending_approval(&row.action_id, ApprovalDecision::Approve, "alice")
        .await
        .unwrap();

    let err = inv
        .decide_pending_approval(&row.action_id, ApprovalDecision::Reject, "bob")
        .await
        .expect_err("second decide must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("pending_approved") || msg.contains("not pending_open"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn decide_unknown_action_id_returns_conflict() {
    let inv = fresh_inv().await;
    let err = inv
        .decide_pending_approval(
            "01HHHHZZZZZZZZZZZZZZZZZZZZ",
            ApprovalDecision::Approve,
            "alice",
        )
        .await
        .expect_err("decide on missing id must fail");
    let msg = format!("{err}");
    assert!(msg.contains("not found"), "got: {msg}");
}

#[tokio::test]
async fn expire_only_transitions_past_expiry_open_rows() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;

    let past = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "s", "old", "sha256:1"),
            expires_at: Utc::now() - Duration::hours(1),
            approver_channel: None,
        })
        .await
        .unwrap();
    let future = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "s", "fresh", "sha256:2"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    // Already-decided rows must not be touched even if their expiry is in
    // the past.
    let decided = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "s", "rejected", "sha256:3"),
            expires_at: Utc::now() - Duration::hours(2),
            approver_channel: None,
        })
        .await
        .unwrap();
    inv.decide_pending_approval(&decided.action_id, ApprovalDecision::Reject, "ops")
        .await
        .unwrap();

    let expired = inv.expire_pending_approvals(Utc::now()).await.unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].action_id, past.action_id);
    assert_eq!(expired[0].state, ApprovalState::PendingExpired);
    assert_eq!(expired[0].decided_by.as_deref(), Some("system:auto-expire"));

    // Future row still open.
    let still_open = inv
        .get_pending_approval(&future.action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_open.state, ApprovalState::PendingOpen);

    // Already-rejected row stays rejected.
    let still_rejected = inv
        .get_pending_approval(&decided.action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_rejected.state, ApprovalState::PendingRejected);

    // Re-running expire is a no-op (idempotent).
    let again = inv.expire_pending_approvals(Utc::now()).await.unwrap();
    assert!(again.is_empty());
}

#[tokio::test]
async fn idempotence_query_finds_existing_open_row() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:bbbb"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let hit = inv
        .find_open_approval_for_proposed_digest(h, "blog", "web", "sha256:bbbb")
        .await
        .unwrap()
        .expect("should find the open row");
    assert_eq!(hit.action_id, row.action_id);

    // Different digest -> no hit.
    let miss = inv
        .find_open_approval_for_proposed_digest(h, "blog", "web", "sha256:other")
        .await
        .unwrap();
    assert!(miss.is_none());

    // After a decision, the row is no longer "open" -> no hit.
    inv.decide_pending_approval(&row.action_id, ApprovalDecision::Approve, "alice")
        .await
        .unwrap();
    let miss2 = inv
        .find_open_approval_for_proposed_digest(h, "blog", "web", "sha256:bbbb")
        .await
        .unwrap();
    assert!(miss2.is_none());
}

#[tokio::test]
async fn approval_rows_do_not_bleed_into_pending_actions() {
    // Approval rows must NOT show up in the agent's pending_actions stream;
    // their delivered_at is set on insert specifically to filter them out.
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    inv.insert_pending_approval(InsertPendingApproval {
        body: body_for(h, "blog", "web", "sha256:1"),
        expires_at: Utc::now() + Duration::hours(24),
        approver_channel: None,
    })
    .await
    .unwrap();

    let pending = inv.pending_actions(h).await.unwrap();
    assert!(
        pending.is_empty(),
        "approval rows must be filtered out of pending_actions"
    );
}

#[tokio::test]
async fn set_approval_message_metadata_persists_chat_and_message_ids() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    inv.set_approval_message_metadata(&row.action_id, 12345, 67890)
        .await
        .unwrap();

    let updated = inv
        .get_pending_approval(&row.action_id)
        .await
        .unwrap()
        .unwrap();
    let meta = updated.metadata_json.expect("metadata persisted");
    assert_eq!(meta["notifier_chat_id"], serde_json::json!(12345));
    assert_eq!(meta["notifier_message_id"], serde_json::json!(67890));

    // Overwrite is idempotent.
    inv.set_approval_message_metadata(&row.action_id, 11, 22)
        .await
        .unwrap();
    let again = inv
        .get_pending_approval(&row.action_id)
        .await
        .unwrap()
        .unwrap();
    let meta = again.metadata_json.unwrap();
    assert_eq!(meta["notifier_chat_id"], serde_json::json!(11));
    assert_eq!(meta["notifier_message_id"], serde_json::json!(22));
}

#[tokio::test]
async fn concurrent_decides_yield_exactly_one_winner() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "blog", "web", "sha256:1"),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();

    let inv1 = inv.clone();
    let inv2 = inv.clone();
    let id = row.action_id.clone();
    let id_b = id.clone();

    let t1 = tokio::spawn(async move {
        inv1.decide_pending_approval(&id, ApprovalDecision::Approve, "dashboard")
            .await
    });
    let t2 = tokio::spawn(async move {
        inv2.decide_pending_approval(&id_b, ApprovalDecision::Reject, "telegram")
            .await
    });

    let (r1, r2) = tokio::join!(t1, t2);
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();

    let oks = [r1.is_ok(), r2.is_ok()];
    let n_ok = oks.iter().filter(|x| **x).count();
    assert_eq!(n_ok, 1, "exactly one decide must win");

    // Final state matches the winner.
    let final_row = inv
        .get_pending_approval(&row.action_id)
        .await
        .unwrap()
        .unwrap();
    assert!(final_row.state.is_decided());
    assert_ne!(final_row.state, ApprovalState::PendingOpen);
}

#[tokio::test]
async fn list_respects_limit() {
    let inv = fresh_inv().await;
    let h = enroll(&inv, "h1").await;
    for i in 0..5 {
        inv.insert_pending_approval(InsertPendingApproval {
            body: body_for(h, "s", &format!("svc{i}"), &format!("sha256:{i}")),
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: None,
        })
        .await
        .unwrap();
    }
    let limited = inv
        .list_pending_approvals(ApprovalFilter {
            state: Some(ApprovalStateFilter::Open),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
}

#[tokio::test]
async fn approval_kind_constant_is_stable() {
    // The dashboard + notifier filter on this string. Lock it so a rename
    // forces the test to be updated explicitly.
    assert_eq!(APPROVAL_KIND, "update_pending_approval");
}
