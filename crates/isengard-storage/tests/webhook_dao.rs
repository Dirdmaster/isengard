//! Acceptance tests for the webhooks DAO.
//!
//! See plan §"T1: Storage migration 0020 + DAO" of
//! Webhook storage tests.

use chrono::{Duration, Utc};
use isengard_storage::Inventory;
use isengard_storage::webhook::{DeliveryStatus, InsertDelivery, InsertWebhook, UpdateWebhook};

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

fn sample_insert() -> InsertWebhook {
    InsertWebhook {
        url: "https://example.com/hook".into(),
        secret: "s3cret".into(),
        event_kinds: "update.success,update.failed".into(),
        enabled: true,
    }
}

#[tokio::test]
async fn insert_then_get_round_trips_fields() {
    let inv = fresh_inv().await;
    let row = inv.insert_webhook(sample_insert()).await.expect("insert");
    assert_eq!(row.url, "https://example.com/hook");
    assert_eq!(row.secret, "s3cret");
    assert_eq!(row.event_kinds, "update.success,update.failed");
    assert!(row.enabled);

    let got = inv
        .get_webhook(row.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got, row);
}

#[tokio::test]
async fn list_returns_inserted_rows_in_id_order() {
    let inv = fresh_inv().await;
    let a = inv.insert_webhook(sample_insert()).await.expect("a");
    let b = inv
        .insert_webhook(InsertWebhook {
            url: "https://example.com/b".into(),
            secret: "x".into(),
            event_kinds: "*".into(),
            enabled: false,
        })
        .await
        .expect("b");

    let all = inv.list_webhooks().await.expect("list");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, a.id);
    assert_eq!(all[1].id, b.id);

    let enabled = inv.list_enabled_webhooks().await.expect("list_enabled");
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].id, a.id);
}

#[tokio::test]
async fn update_coalesces_unset_fields() {
    let inv = fresh_inv().await;
    let row = inv.insert_webhook(sample_insert()).await.expect("insert");

    let updated = inv
        .update_webhook(
            row.id,
            UpdateWebhook {
                enabled: Some(false),
                event_kinds: Some("*".into()),
                ..Default::default()
            },
        )
        .await
        .expect("update")
        .expect("present");

    assert_eq!(updated.url, row.url, "url unchanged");
    assert_eq!(updated.secret, row.secret, "secret unchanged");
    assert_eq!(updated.event_kinds, "*");
    assert!(!updated.enabled);
}

#[tokio::test]
async fn delete_cascades_deliveries() {
    let inv = fresh_inv().await;
    let w = inv
        .insert_webhook(sample_insert())
        .await
        .expect("insert webhook");
    let d = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: r#"{"kind":"update.success"}"#.into(),
        })
        .await
        .expect("insert delivery");

    assert_eq!(d.status, DeliveryStatus::Pending);
    assert_eq!(d.attempts, 0);

    let deleted = inv.delete_webhook(w.id).await.expect("delete");
    assert!(deleted);

    // Cascade: delivery row is gone too.
    let after = inv.list_deliveries(w.id, None, 100).await.expect("list");
    assert!(after.is_empty());
}

#[tokio::test]
async fn claim_pending_respects_next_retry_at() {
    let inv = fresh_inv().await;
    let w = inv.insert_webhook(sample_insert()).await.expect("insert");
    let now = Utc::now();

    // Two deliveries: one due, one scheduled in the future.
    let due = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("d1");
    let later = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.failed".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("d2");

    inv.mark_delivery_pending(later.id, now, 1, now + Duration::hours(1), "transient")
        .await
        .expect("schedule retry");

    let claimed = inv.claim_pending_deliveries(now, 10).await.expect("claim");
    let ids: Vec<i64> = claimed.iter().map(|d| d.id).collect();
    assert!(ids.contains(&due.id), "due delivery should be claimed");
    assert!(
        !ids.contains(&later.id),
        "future-scheduled delivery should NOT be claimed yet"
    );
}

#[tokio::test]
async fn delivery_state_machine_transitions() {
    let inv = fresh_inv().await;
    let w = inv.insert_webhook(sample_insert()).await.expect("insert");
    let d = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("insert delivery");

    let now = Utc::now();

    inv.mark_delivery_pending(d.id, now, 1, now + Duration::seconds(30), "boom")
        .await
        .expect("pending");
    let after_pending = inv.get_delivery(d.id).await.expect("get").expect("present");
    assert_eq!(after_pending.status, DeliveryStatus::Pending);
    assert_eq!(after_pending.attempts, 1);
    assert_eq!(after_pending.last_error.as_deref(), Some("boom"));
    assert!(after_pending.next_retry_at.is_some());

    inv.mark_delivery_failed(d.id, now, 1, "client error")
        .await
        .expect("failed");
    let after_failed = inv.get_delivery(d.id).await.expect("get").expect("present");
    assert_eq!(after_failed.status, DeliveryStatus::Failed);
    assert!(after_failed.next_retry_at.is_none());

    // Brand new delivery for the success path.
    let d2 = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("insert d2");
    inv.mark_delivery_success(d2.id, now, 2)
        .await
        .expect("success");
    let after_success = inv
        .get_delivery(d2.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(after_success.status, DeliveryStatus::Success);
    assert_eq!(after_success.attempts, 2);
    assert!(after_success.last_error.is_none());

    // Exhausted path on a third row.
    let d3 = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.failed".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("insert d3");
    inv.mark_delivery_exhausted(d3.id, now, 5, "ran out")
        .await
        .expect("exhausted");
    let after_ex = inv
        .get_delivery(d3.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(after_ex.status, DeliveryStatus::Exhausted);
    assert_eq!(after_ex.attempts, 5);
}

#[tokio::test]
async fn list_deliveries_filters_by_status_and_limit() {
    let inv = fresh_inv().await;
    let w = inv.insert_webhook(sample_insert()).await.expect("insert");
    for _ in 0..3 {
        inv.insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("insert");
    }
    let one = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.failed".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("insert one");
    inv.mark_delivery_success(one.id, Utc::now(), 1)
        .await
        .expect("mark");

    let pending = inv
        .list_deliveries(w.id, Some(DeliveryStatus::Pending), 50)
        .await
        .expect("pending");
    assert_eq!(pending.len(), 3);

    let success = inv
        .list_deliveries(w.id, Some(DeliveryStatus::Success), 50)
        .await
        .expect("success");
    assert_eq!(success.len(), 1);

    let limited = inv.list_deliveries(w.id, None, 2).await.expect("limit");
    assert_eq!(limited.len(), 2);
}

// Additions: lifecycle + gate delivery sources.

use isengard_storage::webhook::{DeliverySource, InsertGateDelivery, InsertLifecycleDelivery};

#[tokio::test]
async fn lifecycle_delivery_inserts_with_inline_url_and_secret() {
    let inv = fresh_inv().await;
    let d = inv
        .insert_lifecycle_delivery(InsertLifecycleDelivery {
            url: "https://hooks.example.com/pre".into(),
            secret: Some("shh".into()),
            event_kind: "deployment.spinning_up".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("lifecycle insert");
    assert_eq!(d.source, DeliverySource::Lifecycle);
    assert!(d.webhook_id.is_none());
    assert_eq!(d.url.as_deref(), Some("https://hooks.example.com/pre"));
    assert_eq!(d.secret.as_deref(), Some("shh"));
    assert_eq!(d.event_kind, "deployment.spinning_up");
    assert_eq!(d.status, DeliveryStatus::Pending);
}

#[tokio::test]
async fn gate_delivery_inserts_with_inline_url() {
    let inv = fresh_inv().await;
    let d = inv
        .insert_gate_delivery(InsertGateDelivery {
            url: "https://gate.example.com/decide".into(),
            secret: None,
            event_kind: "update.gate".into(),
            payload_json: "{}".into(),
        })
        .await
        .expect("gate insert");
    assert_eq!(d.source, DeliverySource::Gate);
    assert!(d.webhook_id.is_none());
    assert_eq!(d.url.as_deref(), Some("https://gate.example.com/decide"));
    assert!(d.secret.is_none());
}

#[tokio::test]
async fn list_deliveries_by_source_filters_by_kind() {
    let inv = fresh_inv().await;

    // One webhook delivery (12a flow).
    let w = inv.insert_webhook(sample_insert()).await.expect("webhook");
    let _wd = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();

    // One lifecycle, one gate.
    let _l = inv
        .insert_lifecycle_delivery(InsertLifecycleDelivery {
            url: "https://hooks.example.com/post".into(),
            secret: None,
            event_kind: "deployment.completed".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();
    let _g = inv
        .insert_gate_delivery(InsertGateDelivery {
            url: "https://gate.example.com/decide".into(),
            secret: None,
            event_kind: "update.gate".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();

    let webhook_only = inv
        .list_deliveries_by_source(DeliverySource::Webhook, 100)
        .await
        .unwrap();
    assert_eq!(webhook_only.len(), 1);
    let lifecycle_only = inv
        .list_deliveries_by_source(DeliverySource::Lifecycle, 100)
        .await
        .unwrap();
    assert_eq!(lifecycle_only.len(), 1);
    let gate_only = inv
        .list_deliveries_by_source(DeliverySource::Gate, 100)
        .await
        .unwrap();
    assert_eq!(gate_only.len(), 1);
}

#[tokio::test]
async fn webhook_delivery_default_source_is_webhook() {
    let inv = fresh_inv().await;
    let w = inv.insert_webhook(sample_insert()).await.expect("webhook");
    let d = inv
        .insert_delivery(InsertDelivery {
            webhook_id: w.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();
    assert_eq!(d.source, DeliverySource::Webhook);
    assert_eq!(d.webhook_id, Some(w.id));
    assert!(d.url.is_none());
    assert!(d.secret.is_none());
}

#[tokio::test]
async fn delivery_source_round_trips_through_str() {
    use std::str::FromStr;
    for s in [
        DeliverySource::Webhook,
        DeliverySource::Lifecycle,
        DeliverySource::Gate,
    ] {
        let parsed = DeliverySource::from_str(s.as_str()).expect("roundtrip");
        assert_eq!(parsed, s);
    }
    assert!(DeliverySource::from_str("nope").is_err());
}

#[tokio::test]
async fn claim_pending_returns_lifecycle_and_webhook_rows() {
    let inv = fresh_inv().await;
    let w = inv.insert_webhook(sample_insert()).await.expect("w");
    inv.insert_delivery(InsertDelivery {
        webhook_id: w.id,
        event_kind: "update.success".into(),
        payload_json: "{}".into(),
    })
    .await
    .unwrap();
    inv.insert_lifecycle_delivery(InsertLifecycleDelivery {
        url: "https://hooks.example.com/x".into(),
        secret: None,
        event_kind: "deployment.completed".into(),
        payload_json: "{}".into(),
    })
    .await
    .unwrap();

    let due = inv
        .claim_pending_deliveries(Utc::now() + Duration::seconds(1), 50)
        .await
        .unwrap();
    assert_eq!(due.len(), 2);
    let sources: Vec<_> = due.iter().map(|d| d.source).collect();
    assert!(sources.contains(&DeliverySource::Webhook));
    assert!(sources.contains(&DeliverySource::Lifecycle));
}
