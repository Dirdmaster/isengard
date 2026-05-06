//! Delivery worker: ticks at a fixed interval, claims pending due
//! deliveries, POSTs them with HMAC signing, updates row state.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use isengard_storage::Inventory;
use isengard_storage::webhook::{Webhook, WebhookDelivery};
use reqwest::Client;
use tracing::{debug, warn};

use crate::backoff::{MAX_ATTEMPTS, next_delay};
use crate::sign::{SIGNATURE_HEADER, compute_signature};

/// Run the worker forever. The caller aborts the JoinHandle on shutdown.
pub async fn run(inventory: Arc<Inventory>, http: Client, tick: Duration, batch: i64) {
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = tick_once(&inventory, &http, batch).await {
            warn!(error = %e, "webhooks worker tick errored");
        }
    }
}

/// Drive one batch: claim pending due deliveries, dispatch each one.
pub async fn tick_once(inventory: &Inventory, http: &Client, batch: i64) -> anyhow::Result<()> {
    let now = Utc::now();
    let due = inventory.claim_pending_deliveries(now, batch).await?;
    if due.is_empty() {
        return Ok(());
    }
    for delivery in due {
        // Re-fetch the webhook each time: cheap (single SELECT) and avoids
        // stale URL/secret if the operator edited the row mid-flight.
        let webhook = match inventory.get_webhook(delivery.webhook_id).await? {
            Some(w) => w,
            None => {
                debug!(
                    delivery_id = delivery.id,
                    webhook_id = delivery.webhook_id,
                    "webhook gone; marking delivery failed"
                );
                let now = Utc::now();
                let _ = inventory
                    .mark_delivery_failed(delivery.id, now, delivery.attempts, "webhook deleted")
                    .await;
                continue;
            }
        };
        if let Err(e) = dispatch_one(inventory, http, &webhook, &delivery).await {
            warn!(
                delivery_id = delivery.id,
                error = %e,
                "dispatch_one errored (state already written)"
            );
        }
    }
    Ok(())
}

/// Dispatch one delivery. Updates the row's status based on the outcome.
pub async fn dispatch_one(
    inventory: &Inventory,
    http: &Client,
    webhook: &Webhook,
    delivery: &WebhookDelivery,
) -> anyhow::Result<()> {
    let body = delivery.payload_json.clone();
    let signature = compute_signature(webhook.secret.as_bytes(), body.as_bytes());
    let attempts = delivery.attempts + 1;
    let now = Utc::now();

    let resp = http
        .post(&webhook.url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(SIGNATURE_HEADER, &signature)
        .body(body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            inventory
                .mark_delivery_success(delivery.id, now, attempts)
                .await?;
            Ok(())
        }
        Ok(r) if r.status().is_client_error() => {
            // 4xx: receiver says the request is permanently bad. No retry.
            let status = r.status();
            inventory
                .mark_delivery_failed(delivery.id, now, attempts, &format!("HTTP {status}"))
                .await?;
            Ok(())
        }
        Ok(r) => {
            // 5xx or other non-success: retry per backoff.
            let status = r.status();
            schedule_retry(inventory, delivery.id, attempts, &format!("HTTP {status}")).await?;
            Ok(())
        }
        Err(e) => {
            // Network / timeout / DNS: treat as transient, retry per backoff.
            let msg = e.to_string();
            schedule_retry(inventory, delivery.id, attempts, &msg).await?;
            Ok(())
        }
    }
}

/// Mark a delivery for retry. If the attempt count has hit the cap, the
/// row is marked `exhausted` instead.
async fn schedule_retry(
    inventory: &Inventory,
    delivery_id: i64,
    attempts: i64,
    err: &str,
) -> anyhow::Result<()> {
    let now = Utc::now();
    if attempts >= MAX_ATTEMPTS {
        inventory
            .mark_delivery_exhausted(delivery_id, now, attempts, err)
            .await?;
        return Ok(());
    }
    let delay = next_delay(attempts).unwrap_or(Duration::from_secs(60));
    let next =
        now + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::seconds(60));
    inventory
        .mark_delivery_pending(delivery_id, now, attempts, next, err)
        .await?;
    Ok(())
}
