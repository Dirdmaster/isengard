//! Renewal scheduler. Wakes every hour, scans `tls_certs` for any cert
//! whose `next_renewal_at` has passed, triggers a fresh ACME order, and
//! installs the cert in the cert_store. Emits journal events on result.
//!
//! Rate-limit guard: refuses to retry within an exponential backoff window
//! after a failure (1h, 2h, 4h, 8h, max 24h based on attempt_count).

use crate::tls::{AcmeClient, CertStore};
use chrono::{DateTime, Utc};
use isengard_core::{Event, EventEmitter};
use isengard_storage::Inventory;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

const TICK_INTERVAL: Duration = Duration::from_secs(3600);

pub fn spawn(
    inv: Arc<Inventory>,
    cert_store: Arc<CertStore>,
    acme_client: Arc<AcmeClient>,
    emitter: Arc<dyn EventEmitter>,
) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick(&inv, &cert_store, &acme_client, &emitter).await {
                warn!(error = %e, "tls: renewal tick failed");
            }
            sleep(TICK_INTERVAL).await;
        }
    });
}

async fn tick(
    inv: &Inventory,
    cert_store: &Arc<CertStore>,
    acme_client: &Arc<AcmeClient>,
    emitter: &Arc<dyn EventEmitter>,
) -> anyhow::Result<()> {
    let now = Utc::now();
    let due = inv.list_tls_certs_due(now).await?;

    for meta in due {
        if !should_retry(&meta, now) {
            continue;
        }
        let hostname = meta.public_hostname.clone();
        match acme_client.order(&hostname).await {
            Ok(cert) => {
                if let Err(e) = cert_store
                    .install(&hostname, &cert.cert_pem, &cert.key_pem)
                    .await
                {
                    let _ = inv
                        .record_tls_attempt(&hostname, false, Some(format!("install failed: {e}")))
                        .await;
                    warn!(host = %hostname, error = %e, "tls: cert install failed");
                    continue;
                }
                let _ = inv.record_tls_attempt(&hostname, true, None).await;
                emitter
                    .emit(Event {
                        kind: "tls.cert.renewed".into(),
                        summary: format!("renewed cert for {hostname}"),
                        container_name: Some(hostname.clone()),
                        occurred_at: Utc::now(),
                        ..Default::default()
                    })
                    .await;
                info!(host = %hostname, "tls: cert renewed");
            }
            Err(e) => {
                let _ = inv
                    .record_tls_attempt(&hostname, false, Some(e.to_string()))
                    .await;
                emitter
                    .emit(Event {
                        kind: "tls.acme.failed".into(),
                        summary: format!("ACME failed for {hostname}: {e}"),
                        container_name: Some(hostname.clone()),
                        error: Some(e.to_string()),
                        occurred_at: Utc::now(),
                        ..Default::default()
                    })
                    .await;
                warn!(host = %hostname, error = %e, "tls: acme failed");
            }
        }
    }
    Ok(())
}

/// Exponential-backoff guard. After `attempt_count` consecutive failures, the
/// scheduler refuses to retry within the window: 1h, 2h, 4h, 8h, then capped
/// at 24h. Returns `true` when a fresh attempt is permitted.
///
/// `pub` because PB-T13 unit-tests this in isolation without the full tick.
pub fn should_retry(meta: &isengard_storage::TlsCertMeta, now: DateTime<Utc>) -> bool {
    let Some(last) = meta.last_attempt_at else {
        return true;
    };
    let backoff_hours = match meta.attempt_count {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        _ => 24,
    };
    let next_allowed = last + chrono::Duration::hours(backoff_hours);
    now >= next_allowed
}
