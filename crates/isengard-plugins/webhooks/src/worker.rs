//! Delivery worker.
//!
//! Ticks at a fixed interval, claims a batch of pending due
//! deliveries, POSTs each with HMAC signing, and writes the outcome
//! back to the row.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use isengard_storage::Inventory;
use isengard_storage::webhook::{DeliverySource, WebhookDelivery};
use reqwest::Client;
use tracing::{debug, warn};

use crate::backoff::{MAX_ATTEMPTS, next_delay};
use crate::sign::{SIGNATURE_HEADER, compute_signature};

/// Resolved destination for one delivery row.
///
/// For `source = Webhook` rows we look these up via the parent
/// `webhooks` table; for `source = Lifecycle` or `source = Gate`
/// rows the row carries them directly.
pub struct Endpoint {
    /// Destination URL.
    pub url: String,
    /// HMAC secret. Empty when no per-row secret is configured.
    pub secret: String,
}

/// Runs the worker loop forever.
///
/// The caller aborts the join handle on shutdown. The ticker uses
/// `MissedTickBehavior::Delay` so a slow tick doesn't burst-fire.
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

/// Runs one worker tick.
///
/// Claims up to `batch` due rows, dispatches each via
/// [`dispatch_one`]. Per-row failures log; the loop keeps going.
///
/// # Errors
///
/// Returns an error when the claim query itself fails. Per-row
/// failures are logged and don't propagate.
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

/// Resolves a delivery row's destination URL and secret.
///
/// For `Webhook` source: re-SELECTs the parent `webhooks` row on
/// every tick (cheap and keeps the URL/secret current if the
/// operator edits mid-flight). For `Lifecycle` and `Gate` sources:
/// uses the values stored on the row itself.
///
/// Lifecycle hooks without a per-container secret fall back to an
/// empty key. The HMAC computation accepts empty keys; receivers
/// that don't care about signing can ignore the header, and ones
/// that do care can still verify because the empty-key signature is
/// well-defined.
///
/// # Errors
///
/// Returns an error when the SELECT for a `Webhook`-source row
/// fails.
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
            let secret = delivery.secret.clone().unwrap_or_default();
            Ok(Some(Endpoint { url, secret }))
        }
    }
}

/// Dispatches one delivery and writes the outcome to the row.
///
/// The signature header carries
/// `HMAC-SHA256(secret, payload_json)`. Outcomes:
///
/// - 2xx: `mark_delivery_success`.
/// - 4xx: `mark_delivery_failed`, no retry.
/// - 5xx or other non-success: `schedule_retry`.
/// - Transport error: `schedule_retry` (treated as transient).
///
/// # Errors
///
/// Returns an error when the state-write back to storage fails. The
/// HTTP call itself never returns Err: all transport errors fold
/// into `schedule_retry`.
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
            let status = r.status();
            inventory
                .mark_delivery_failed(delivery.id, now, attempts, &format!("HTTP {status}"))
                .await?;
            Ok(())
        }
        Ok(r) => {
            let status = r.status();
            schedule_retry(inventory, delivery.id, attempts, &format!("HTTP {status}")).await?;
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            schedule_retry(inventory, delivery.id, attempts, &msg).await?;
            Ok(())
        }
    }
}

/// Marks a delivery for retry or, if the attempt count has hit
/// [`MAX_ATTEMPTS`], marks it `exhausted`.
///
/// The next-attempt timestamp uses [`next_delay`] when available;
/// the fallback 60s only triggers if the schedule ever returns
/// `None` for a non-exhausted state (it currently never does).
///
/// # Errors
///
/// Returns an error when the storage write fails.
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
