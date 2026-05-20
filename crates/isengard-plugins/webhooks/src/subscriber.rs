//! General webhook subscriber.
//!
//! Tails the controller event bus and, for each event, inserts a
//! `webhook_deliveries` row per enabled webhook that opted in to the
//! event's kind via its wildcard string.

use std::sync::Arc;

use isengard_core::Event;
use isengard_storage::Inventory;
use isengard_storage::webhook::{InsertDelivery, kind_matches};
use tokio::sync::broadcast::Receiver;
use tracing::{debug, warn};

/// Runs the subscriber loop until the bus closes.
///
/// Each iteration: receive an event, fan it out via [`on_event`]. A
/// lag from the broadcast channel logs at warn and resumes; closing
/// the channel ends the task.
pub async fn run(inventory: Arc<Inventory>, mut rx: Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                if let Err(e) = on_event(inventory.as_ref(), &event).await {
                    warn!(error = %e, kind = %event.kind, "webhooks subscriber: enqueue failed");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    skipped = n,
                    "webhooks subscriber broadcast lag, events dropped"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                debug!("webhooks subscriber: broadcast closed; ending task");
                break;
            }
        }
    }
}

/// Enqueues one event for every matching enabled webhook.
///
/// Serializes `event` to JSON once, then iterates enabled webhooks
/// and inserts a delivery row per kind match. Per-row failures log
/// at warn and don't stop the iteration.
///
/// # Errors
///
/// Returns an error when the initial enabled-webhooks list query
/// fails or when JSON serialization fails.
pub async fn on_event(inventory: &Inventory, event: &Event) -> anyhow::Result<()> {
    let webhooks = inventory.list_enabled_webhooks().await?;
    if webhooks.is_empty() {
        return Ok(());
    }

    let payload = serde_json::to_string(event)?;

    for w in webhooks {
        if !kind_matches(&w.event_kinds, &event.kind) {
            continue;
        }
        if let Err(e) = inventory
            .insert_delivery(InsertDelivery {
                webhook_id: w.id,
                event_kind: event.kind.clone(),
                payload_json: payload.clone(),
            })
            .await
        {
            warn!(
                webhook_id = w.id,
                kind = %event.kind,
                error = %e,
                "insert_delivery failed; event lost for this webhook"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use isengard_storage::webhook::InsertWebhook;

    fn ev(kind: &str) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            ..Default::default()
        }
    }

    async fn open_inv() -> Inventory {
        Inventory::open_in_memory().await.expect("open")
    }

    #[tokio::test]
    async fn enqueues_for_wildcard_filter() {
        let inv = open_inv().await;
        let w = inv
            .insert_webhook(InsertWebhook {
                url: "https://example.com".into(),
                secret: "k".into(),
                event_kinds: "*".into(),
                enabled: true,
            })
            .await
            .unwrap();
        on_event(&inv, &ev("update.success")).await.unwrap();
        let d = inv.list_deliveries(w.id, None, 10).await.unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].event_kind, "update.success");
    }

    #[tokio::test]
    async fn enqueues_only_matching_kinds() {
        let inv = open_inv().await;
        let w = inv
            .insert_webhook(InsertWebhook {
                url: "https://example.com".into(),
                secret: "k".into(),
                event_kinds: "update.failed".into(),
                enabled: true,
            })
            .await
            .unwrap();
        on_event(&inv, &ev("update.success")).await.unwrap();
        on_event(&inv, &ev("update.failed")).await.unwrap();
        let d = inv.list_deliveries(w.id, None, 10).await.unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].event_kind, "update.failed");
    }

    #[tokio::test]
    async fn enqueues_for_each_matching_webhook() {
        let inv = open_inv().await;
        let a = inv
            .insert_webhook(InsertWebhook {
                url: "https://a".into(),
                secret: "k".into(),
                event_kinds: "*".into(),
                enabled: true,
            })
            .await
            .unwrap();
        let b = inv
            .insert_webhook(InsertWebhook {
                url: "https://b".into(),
                secret: "k".into(),
                event_kinds: "update.success,update.failed".into(),
                enabled: true,
            })
            .await
            .unwrap();
        on_event(&inv, &ev("update.success")).await.unwrap();
        assert_eq!(inv.list_deliveries(a.id, None, 10).await.unwrap().len(), 1);
        assert_eq!(inv.list_deliveries(b.id, None, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn skips_disabled_webhooks() {
        let inv = open_inv().await;
        let w = inv
            .insert_webhook(InsertWebhook {
                url: "https://example.com".into(),
                secret: "k".into(),
                event_kinds: "*".into(),
                enabled: false,
            })
            .await
            .unwrap();
        on_event(&inv, &ev("update.success")).await.unwrap();
        let d = inv.list_deliveries(w.id, None, 10).await.unwrap();
        assert!(d.is_empty());
    }
}
