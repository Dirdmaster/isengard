//! Wildcard-cert renewal scheduler. Sister to the agent's HTTP-01 scheduler
//! (`crates/isengard-agent/src/tls/renewal.rs`); this one runs controller-side
//! and drives DNS-01 issuance.
//!
//! Cadence: tick every 6h, re-issue any cert whose `next_renewal_at` has
//! passed. LE certs are 90 days; we set renewal to 30 days before expiry, so
//! the typical hot path is "no work to do" and the timer is mostly idle.
//!
//! Backoff: same shape as the agent's scheduler. After consecutive failures,
//! refuse to retry within a window (1h, 2h, 4h, 8h, capped at 24h). Without
//! this guard, a misconfigured CF token would burn LE's 5-orders/hour
//! per-account quota in minutes.

use crate::acme::dns01_cf::{AcmeDns01Client, DnsProvider, IssuedCert};
use crate::acme::store::WildcardCertStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use isengard_storage::{HostId, Inventory, TlsCertMeta, UpsertTlsCertMeta};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

/// Production tick interval. Tests can drive `tick()` directly without this
/// loop.
const TICK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Days-before-expiry to schedule renewal. LE issues 90-day certs; setting
/// this to 30 means we hit the next renewal at the 60-day mark with 30 days
/// of buffer for retries / outages before the cert actually expires.
pub const RENEW_DAYS_BEFORE_EXPIRY: i64 = 30;

/// Synthetic `host_id` used for wildcard certs. Wildcard certs aren't owned
/// by any single host (every agent that routes traffic for the zone gets the
/// same cert), so we burn one all-zero ULID-shaped value in the existing
/// `tls_certs.host_id` column. This keeps the schema stable and lets the
/// wildcard rows coexist with per-host HTTP-01 rows.
fn wildcard_host_id() -> HostId {
    HostId::from_db_bytes(vec![0u8; 16]).expect("16-byte zero ULID is always valid")
}

/// Each scheduler invocation iterates over the configured wildcard domain
/// groups. A "group" is the set of identifiers in a single LE order — for
/// the homelab this is `["*.vallee.casa", "vallee.casa"]`, both covered by
/// the same cert. The first identifier is the canonical key into the
/// `tls_certs` table.
#[derive(Debug, Clone)]
pub struct WildcardGroup {
    pub identifiers: Vec<String>,
}

impl WildcardGroup {
    pub fn primary(&self) -> &str {
        &self.identifiers[0]
    }
}

/// Parse the comma-separated `ISENGARD_ACME_DOMAINS` env var into one or
/// more wildcard groups. Each group is a single LE order.
///
/// Grouping rule: a wildcard `*.foo` and its apex `foo` go in the same group
/// (they're typically requested together so the apex name is also covered);
/// any other domain is its own group. This matches the homelab use case
/// without forcing the operator to learn a more elaborate config syntax.
pub fn parse_acme_domains(raw: &str) -> Vec<WildcardGroup> {
    let entries: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut groups: Vec<WildcardGroup> = Vec::new();
    let mut consumed: Vec<bool> = vec![false; entries.len()];

    for (i, e) in entries.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        let is_wildcard = e.starts_with("*.");
        let apex = if is_wildcard { &e[2..] } else { e.as_str() };

        let mut group = vec![e.clone()];
        consumed[i] = true;
        // Pair wildcard + apex if the apex is also listed.
        for (j, other) in entries.iter().enumerate() {
            if i == j || consumed[j] {
                continue;
            }
            if (is_wildcard && other.as_str() == apex)
                || (!is_wildcard && other.as_str() == format!("*.{apex}"))
            {
                group.push(other.clone());
                consumed[j] = true;
            }
        }
        // Always keep the wildcard first when both are present so the
        // primary key is stable across config edits.
        group.sort_by(|a, b| {
            let aw = a.starts_with("*.");
            let bw = b.starts_with("*.");
            bw.cmp(&aw)
        });
        groups.push(WildcardGroup { identifiers: group });
    }
    groups
}

/// Spawn the periodic renewal loop. Cancels with the runtime; no explicit
/// shutdown handle is needed because the `Arc<>` graph drops on
/// `run_controller` exit.
pub fn spawn<P: DnsProvider + 'static>(
    inventory: Arc<Inventory>,
    cert_store: Arc<WildcardCertStore>,
    acme: Arc<AcmeDns01Client<P>>,
    domains: Vec<WildcardGroup>,
) {
    if domains.is_empty() {
        return;
    }
    tokio::spawn(async move {
        // First tick immediately on boot: the operator's expectation is "I
        // configured ACME, so the cert appears on first start", not "wait 6h".
        loop {
            if let Err(e) = tick(&inventory, &cert_store, &acme, &domains).await {
                warn!(error = %e, "acme: scheduler tick failed");
            }
            sleep(TICK_INTERVAL).await;
        }
    });
}

/// One pass: for each domain group, decide whether to (re)issue and act on it.
/// Pulled out of the loop body so the unit tests can call it directly.
pub async fn tick<P: DnsProvider>(
    inventory: &Arc<Inventory>,
    cert_store: &Arc<WildcardCertStore>,
    acme: &Arc<AcmeDns01Client<P>>,
    groups: &[WildcardGroup],
) -> Result<()> {
    let now = Utc::now();
    for group in groups {
        let primary = group.primary();
        let meta = inventory
            .get_tls_cert_meta(primary)
            .await
            .with_context(|| format!("get_tls_cert_meta for {primary}"))?;

        if !needs_issuance(meta.as_ref(), now) {
            continue;
        }
        if let Some(ref m) = meta {
            if !should_retry(m, now) {
                tracing::debug!(
                    primary = %primary,
                    attempts = m.attempt_count,
                    "acme: backoff window not elapsed; skipping",
                );
                continue;
            }
        }

        match acme.order_wildcard(&group.identifiers).await {
            Ok(cert) => {
                if let Err(e) = handle_issued(inventory, cert_store, &cert).await {
                    warn!(primary = %primary, "acme: post-issuance handling failed: {e:#}");
                    let _ = inventory
                        .record_tls_attempt(primary, false, Some(format!("post-issue: {e:#}")))
                        .await;
                } else {
                    info!(primary = %primary, "acme: wildcard cert issued/renewed");
                }
            }
            Err(e) => {
                warn!(primary = %primary, "acme: order_wildcard failed: {e:#}");
                let _ = inventory
                    .record_tls_attempt(primary, false, Some(format!("{e:#}")))
                    .await;
            }
        }
    }
    Ok(())
}

/// Returns true when there is no current cert for the primary identifier
/// or when its `next_renewal_at` has elapsed.
fn needs_issuance(meta: Option<&TlsCertMeta>, now: DateTime<Utc>) -> bool {
    match meta {
        None => true,
        Some(m) => now >= m.next_renewal_at,
    }
}

/// Same shape as the agent's exponential backoff. Public for unit tests.
pub fn should_retry(meta: &TlsCertMeta, now: DateTime<Utc>) -> bool {
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
    now >= last + ChronoDuration::hours(backoff_hours)
}

async fn handle_issued(
    inventory: &Arc<Inventory>,
    cert_store: &Arc<WildcardCertStore>,
    cert: &IssuedCert,
) -> Result<()> {
    let primary = &cert.identifiers[0];

    // Parse the cert to extract not_before / not_after for the metadata row.
    let (not_before, not_after, serial) = parse_cert_validity(&cert.cert_pem)?;
    let next_renewal = not_after - ChronoDuration::days(RENEW_DAYS_BEFORE_EXPIRY);

    inventory
        .upsert_tls_cert_meta(UpsertTlsCertMeta {
            public_hostname: primary.clone(),
            host_id: wildcard_host_id(),
            issuer: "letsencrypt".into(),
            not_before,
            not_after,
            next_renewal_at: next_renewal,
            serial: Some(serial),
        })
        .await?;
    inventory.record_tls_attempt(primary, true, None).await?;

    cert_store
        .install(&cert.identifiers, &cert.cert_pem, &cert.key_pem)
        .await?;

    Ok(())
}

/// Pull `notBefore`, `notAfter`, and the serial off the leaf cert PEM.
pub fn parse_cert_validity(cert_pem: &str) -> Result<(DateTime<Utc>, DateTime<Utc>, String)> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("parse cert PEM: {e}"))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| anyhow::anyhow!("parse X509: {e}"))?;

    let nb = chrono::DateTime::<Utc>::from_timestamp(cert.validity().not_before.timestamp(), 0)
        .ok_or_else(|| anyhow::anyhow!("not_before out of range"))?;
    let na = chrono::DateTime::<Utc>::from_timestamp(cert.validity().not_after.timestamp(), 0)
        .ok_or_else(|| anyhow::anyhow!("not_after out of range"))?;
    let serial = cert.serial.to_str_radix(16);
    Ok((nb, na, serial))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(attempts: u32, last: Option<DateTime<Utc>>) -> TlsCertMeta {
        TlsCertMeta {
            public_hostname: "*.vallee.casa".into(),
            host_id: wildcard_host_id(),
            issuer: "letsencrypt".into(),
            not_before: Utc::now(),
            not_after: Utc::now() + ChronoDuration::days(90),
            last_renewed_at: None,
            next_renewal_at: Utc::now() + ChronoDuration::days(60),
            serial: None,
            last_attempt_at: last,
            last_error: None,
            attempt_count: attempts,
        }
    }

    #[test]
    fn parse_acme_domains_pairs_wildcard_with_apex() {
        let groups = parse_acme_domains("*.vallee.casa,vallee.casa");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].identifiers,
            vec!["*.vallee.casa".to_string(), "vallee.casa".to_string()],
        );
    }

    #[test]
    fn parse_acme_domains_apex_first_still_pairs() {
        let groups = parse_acme_domains("vallee.casa,*.vallee.casa");
        assert_eq!(groups.len(), 1);
        // Wildcard always sorted first as the canonical primary.
        assert_eq!(groups[0].primary(), "*.vallee.casa");
    }

    #[test]
    fn parse_acme_domains_unrelated_split_into_groups() {
        let groups = parse_acme_domains("*.foo.com,bar.com");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].primary(), "*.foo.com");
        assert_eq!(groups[1].primary(), "bar.com");
    }

    #[test]
    fn parse_acme_domains_handles_whitespace_and_empties() {
        let groups = parse_acme_domains("  ,*.foo,  ,foo  ,");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].identifiers,
            vec!["*.foo".to_string(), "foo".to_string()]
        );
    }

    #[test]
    fn parse_acme_domains_empty_input_returns_empty() {
        assert!(parse_acme_domains("").is_empty());
        assert!(parse_acme_domains(" , , ").is_empty());
    }

    #[test]
    fn needs_issuance_no_meta_yes() {
        assert!(needs_issuance(None, Utc::now()));
    }

    #[test]
    fn needs_issuance_future_renewal_no() {
        let m = meta(0, None);
        assert!(!needs_issuance(Some(&m), Utc::now()));
    }

    #[test]
    fn needs_issuance_past_renewal_yes() {
        let mut m = meta(0, None);
        m.next_renewal_at = Utc::now() - ChronoDuration::hours(1);
        assert!(needs_issuance(Some(&m), Utc::now()));
    }

    #[test]
    fn should_retry_first_attempt_immediately() {
        // attempt_count=0 with no last_attempt_at means "fresh"; should retry.
        let m = meta(0, None);
        assert!(should_retry(&m, Utc::now()));
    }

    #[test]
    fn should_retry_one_attempt_one_hour_window() {
        let m = meta(1, Some(Utc::now() - ChronoDuration::minutes(30)));
        assert!(!should_retry(&m, Utc::now()));
        let m = meta(1, Some(Utc::now() - ChronoDuration::hours(2)));
        assert!(should_retry(&m, Utc::now()));
    }

    #[test]
    fn should_retry_caps_at_24h() {
        let m = meta(99, Some(Utc::now() - ChronoDuration::hours(23)));
        assert!(!should_retry(&m, Utc::now()));
        let m = meta(99, Some(Utc::now() - ChronoDuration::hours(25)));
        assert!(should_retry(&m, Utc::now()));
    }

    #[test]
    fn parse_cert_validity_rejects_garbage() {
        let err = parse_cert_validity("not a cert").unwrap_err();
        assert!(err.to_string().contains("parse"));
    }

    #[test]
    fn parse_cert_validity_extracts_dates() {
        // Generate a self-signed cert via rcgen so we have valid PEM to test
        // the parser without external fixtures.
        let mut params = rcgen::CertificateParams::new(vec!["test.example".to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test.example");
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();

        let (nb, na, _serial) = parse_cert_validity(&pem).unwrap();
        assert!(na > nb, "not_after must be > not_before");
    }
}
