//! Delivery worker: ticks at a fixed interval, claims pending due
//! deliveries, POSTs them with HMAC signing, updates row state.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use isengard_storage::Inventory;
use isengard_storage::webhook::{DeliverySource, WebhookDelivery};
use reqwest::Client;
use tracing::{debug, warn};

use crate::backoff::{MAX_ATTEMPTS, next_delay};
use crate::sign::{SIGNATURE_HEADER, compute_signature};

/// Resolved (url, secret) for one delivery row. For `source=webhook` rows
/// we look these up via the parent `webhooks` table; for `source=lifecycle`
/// or `source=gate` rows the row carries them directly.
pub struct Endpoint {
    pub url: String,
    pub secret: String,
}

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
        let endpoint = match resolve_endpoint(inventory, &delivery).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                debug!(
                    delivery_id = delivery.id,
                    source = ?delivery.source,
                    "endpoint gone; marking delivery failed"
                );
                let now = Utc::now();
                let _ = inventory
                    .mark_delivery_failed(
                        delivery.id,
                        now,
                        delivery.attempts,
                        "endpoint resolution failed",
                    )
                    .await;
                continue;
            }
            Err(e) => {
                warn!(delivery_id = delivery.id, error = %e, "endpoint resolution errored");
                continue;
            }
        };
        if let Err(e) = dispatch_one(inventory, http, &endpoint, &delivery).await {
            warn!(
                delivery_id = delivery.id,
                error = %e,
                "dispatch_one errored (state already written)"
            );
        }
    }
    Ok(())
}

/// Resolve a delivery row's destination URL+secret. For `Webhook` source we
/// look up the parent `webhooks` row (cheap re-SELECT each tick: keeps the
/// URL/secret current if the operator edits mid-flight). For `Lifecycle` /
/// `Gate` rows the values come from the row itself.
async fn resolve_endpoint(
    inventory: &Inventory,
    delivery: &WebhookDelivery,
) -> anyhow::Result<Option<Endpoint>> {
    match delivery.source {
        DeliverySource::Webhook => {
            let Some(id) = delivery.webhook_id else {
                return Ok(None);
            };
            match inventory.get_webhook(id).await? {
                Some(w) => Ok(Some(Endpoint {
                    url: w.url,
                    secret: w.secret,
                })),
                None => Ok(None),
            }
        }
        DeliverySource::Lifecycle | DeliverySource::Gate => {
            let url = match delivery.url.clone() {
                Some(u) if !u.is_empty() => u,
                _ => return Ok(None),
            };
            // Lifecycle hooks may run unsigned (no per-container secret
            // configured): fall back to an empty key. Receivers that don't
            // care about signing can ignore the header; receivers that do
            // can still verify because the empty-key signature is well-defined.
            let secret = delivery.secret.clone().unwrap_or_default();
            Ok(Some(Endpoint { url, secret }))
        }
    }
}

/// Dispatch one delivery. Updates the row's status based on the outcome.
pub async fn dispatch_one(
    inventory: &Inventory,
    http: &Client,
    endpoint: &Endpoint,
    delivery: &WebhookDelivery,
) -> anyhow::Result<()> {
    let body = delivery.payload_json.clone();
    let signature = compute_signature(endpoint.secret.as_bytes(), body.as_bytes());
    let attempts = delivery.attempts + 1;
    let now = Utc::now();

    let resp = http
        .post(&endpoint.url)
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
