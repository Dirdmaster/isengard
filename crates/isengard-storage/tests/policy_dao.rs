//! Phase 9a T1 acceptance tests for the policies DAO.
//!
//! See plan §"T1: Storage migration 0016 + Policy DAO" of
//! `docs/superpowers/plans/2026-05-06-phase-9a-9d-policy-foundation.md`.

use chrono::{DateTime, TimeZone, Utc};
use isengard_core::policy::{FailureHandling, Policy, UpdateGate, UpdateStrategy};
use isengard_storage::Inventory;
use isengard_storage::policy::{InsertPolicy, PolicyScopeType};

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

fn full_policy() -> Policy {
    Policy {
        strategy: Some(UpdateStrategy::Pinned),
        gate: Some(UpdateGate::Auto),
        paused_until: Some(
            Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
                .single()
                .expect("valid datetime"),
        ),
        on_failure: Some(FailureHandling::Notify),
        approver_channel: Some("ops-team-chat".to_string()),
    }
}

#[tokio::test]
async fn insert_then_get_round_trip_preserves_all_fields() {
    let inv = fresh_inv().await;
    let body = full_policy();
    let row = inv
        .insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Service,
            scope_key: "prod/blog/web".to_string(),
            body: body.clone(),
        })
        .await
        .expect("insert");
    assert_eq!(row.scope_type, PolicyScopeType::Service);
    assert_eq!(row.scope_key, "prod/blog/web");
    assert_eq!(row.body, body);

    let got = inv
        .get_policy(PolicyScopeType::Service, "prod/blog/web")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.body, body);
    // paused_until survives round-trip via the RFC3339 default timestamp path.
    assert_eq!(got.body.paused_until, body.paused_until);
}

#[tokio::test]
async fn list_orders_by_scope_rank_then_id() {
    let inv = fresh_inv().await;
    // Insert in a deliberately scrambled order to prove the sort comes from
    // the SQL, not the insertion sequence.
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Container,
        scope_key: "deadbeef/web-c".into(),
        body: Policy::default(),
    })
    .await
    .unwrap();
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Stack,
        scope_key: "prod/blog".into(),
        body: Policy::default(),
    })
    .await
    .unwrap();
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Global,
        scope_key: "".into(),
        body: Policy::default(),
    })
    .await
    .unwrap();
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Service,
        scope_key: "prod/blog/web".into(),
        body: Policy::default(),
    })
    .await
    .unwrap();
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Fleet,
        scope_key: "prod".into(),
        body: Policy::default(),
    })
    .await
    .unwrap();

    let listed = inv.list_policies().await.unwrap();
    let scopes: Vec<_> = listed.iter().map(|r| r.scope_type).collect();
    assert_eq!(
        scopes,
        vec![
            PolicyScopeType::Global,
            PolicyScopeType::Fleet,
            PolicyScopeType::Stack,
            PolicyScopeType::Service,
            PolicyScopeType::Container,
        ]
    );
}

#[tokio::test]
async fn upsert_inserts_when_absent_then_updates_existing() {
    let inv = fresh_inv().await;
    let mut body = Policy {
        strategy: Some(UpdateStrategy::TagOnly),
        ..Default::default()
    };

    let inserted = inv
        .upsert_policy(PolicyScopeType::Fleet, "prod", &body)
        .await
        .expect("upsert insert");
    assert_eq!(inserted.body.strategy, Some(UpdateStrategy::TagOnly));
    let created_at = inserted.created_at;
    let first_updated_at = inserted.updated_at;

    // SQLite's strftime('%Y-%m-%dT%H:%M:%fZ', 'now') is millisecond resolution;
    // sleep just past one millisecond so the new updated_at is observably greater.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    body.strategy = Some(UpdateStrategy::Any);
    body.gate = Some(UpdateGate::Auto);
    let updated = inv
        .upsert_policy(PolicyScopeType::Fleet, "prod", &body)
        .await
        .expect("upsert update");
    assert_eq!(updated.body.strategy, Some(UpdateStrategy::Any));
    assert_eq!(updated.body.gate, Some(UpdateGate::Auto));
    assert_eq!(
        updated.created_at, created_at,
        "created_at should be preserved across upsert"
    );
    assert!(
        updated.updated_at >= first_updated_at,
        "updated_at should not regress; was {first_updated_at:?}, now {:?}",
        updated.updated_at
    );

    // Exactly one row exists for this scope.
    let listed = inv.list_policies().await.unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn delete_returns_true_for_existing_false_for_missing() {
    let inv = fresh_inv().await;
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Stack,
        scope_key: "prod/blog".into(),
        body: Policy::default(),
    })
    .await
    .unwrap();

    let removed = inv
        .delete_policy(PolicyScopeType::Stack, "prod/blog")
        .await
        .unwrap();
    assert!(removed);

    let removed_again = inv
        .delete_policy(PolicyScopeType::Stack, "prod/blog")
        .await
        .unwrap();
    assert!(!removed_again);

    let missing = inv
        .delete_policy(PolicyScopeType::Service, "never/inserted/here")
        .await
        .unwrap();
    assert!(!missing);
}

#[tokio::test]
async fn duplicate_insert_violates_unique_constraint() {
    let inv = fresh_inv().await;
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Fleet,
        scope_key: "prod".into(),
        body: Policy::default(),
    })
    .await
    .expect("first insert");

    let err = inv
        .insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Fleet,
            scope_key: "prod".into(),
            body: Policy::default(),
        })
        .await
        .expect_err("duplicate must fail");

    let isengard_storage::error::Error::Db(sqlx_err) = err else {
        panic!("expected Error::Db wrapping a sqlx::Error, got something else");
    };
    assert!(
        matches!(sqlx_err, sqlx::Error::Database(_)),
        "expected sqlx::Error::Database for UNIQUE violation, got {sqlx_err:?}"
    );
}

#[tokio::test]
async fn policy_json_round_trip_preserves_every_field() {
    let inv = fresh_inv().await;
    let body = full_policy();
    let inserted = inv
        .insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: "".into(),
            body: body.clone(),
        })
        .await
        .unwrap();
    // Round-trip via the DAO's JSON column.
    assert_eq!(inserted.body, body);
    assert_eq!(inserted.body.strategy, body.strategy);
    assert_eq!(inserted.body.gate, body.gate);
    assert_eq!(inserted.body.paused_until, body.paused_until);
    assert_eq!(inserted.body.on_failure, body.on_failure);
    assert_eq!(inserted.body.approver_channel, body.approver_channel);
}

#[tokio::test]
async fn paused_until_rfc3339_round_trip_preserves_exact_instant() {
    let inv = fresh_inv().await;
    let pause: DateTime<Utc> = Utc
        .with_ymd_and_hms(2027, 1, 15, 9, 30, 45)
        .single()
        .expect("valid datetime");
    let body = Policy {
        paused_until: Some(pause),
        ..Default::default()
    };
    inv.insert_policy(InsertPolicy {
        scope_type: PolicyScopeType::Service,
        scope_key: "prod/blog/web".into(),
        body,
    })
    .await
    .unwrap();

    let got = inv
        .get_policy(PolicyScopeType::Service, "prod/blog/web")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.body.paused_until, Some(pause));
}
