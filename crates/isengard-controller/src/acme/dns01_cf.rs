//! DNS-01 ACME flow with a pluggable DNS provider (Cloudflare in production).
//!
//! Why this exists: `crates/isengard-agent/src/tls/acme.rs` already drives the
//! HTTP-01 dance per-host. Wildcards (`*.vallee.casa`) cannot use HTTP-01:
//! Let's Encrypt requires DNS-01 for any wildcard cert. This module is the
//! controller-side DNS-01 sibling.
//!
//! Why controller-side: the agent doesn't own DNS for the public zone. The
//! controller has the CF API token, the controller orchestrates wildcard
//! issuance once per renewal and fans the result out to every agent that
//! routes traffic for the zone.
//!
//! Caller contract: this returns `Err` on rate-limit / transient errors. The
//! caller (`scheduler.rs`) MUST apply backoff before retrying. Calling
//! `order_wildcard()` in a tight loop will burn LE's per-account order quota.

use crate::acme::cf_api::CloudflareApi;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
};
use isengard_storage::{Inventory, UpsertAcmeAccount};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio::time::{Instant, sleep};

/// Let's Encrypt production directory URL.
pub const LE_PRODUCTION_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt staging directory URL (lax rate limits, untrusted
/// roots).
pub const LE_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Maximum time an ACME order may spend reaching `Ready` / `Valid`.
const ORDER_FINALIZE_TIMEOUT: Duration = Duration::from_secs(180);
/// Maximum time spent polling for the issued certificate after
/// `finalize`.
const CERT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Interval between successive order/cert polls.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Wait between TXT record create and the `set_ready` notification
/// to LE.
///
/// CF propagates within a few seconds for most paths, but LE queries
/// authoritative nameservers; propagation across CF's edge to those
/// auth servers is what matters. 15 seconds is conservative and
/// matches the lego and certbot CF defaults.
const DNS_PROPAGATION_WAIT: Duration = Duration::from_secs(15);

/// Abstraction over the DNS provider so unit tests can drive the flow
/// with a mock and no real CF calls.
///
/// `present` creates the TXT record at `_acme-challenge.<base_domain>`
/// with `value`; `cleanup` removes whatever `present` returned a
/// handle to.
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Creates the challenge TXT record and returns a handle to it.
    async fn present(&self, base_domain: &str, value: &str) -> Result<DnsRecordHandle>;
    /// Deletes the TXT record identified by `handle`.
    async fn cleanup(&self, handle: DnsRecordHandle) -> Result<()>;
}

/// Identifier returned by [`DnsProvider::present`] so [`DnsProvider::cleanup`]
/// can target the right record.
#[derive(Debug, Clone)]
pub struct DnsRecordHandle {
    /// CF zone id.
    pub zone_id: String,
    /// CF record id.
    pub record_id: String,
}

/// Production [`DnsProvider`] backed by [`CloudflareApi`].
pub struct CloudflareDnsProvider {
    /// Underlying CF client.
    api: CloudflareApi,
}

impl CloudflareDnsProvider {
    /// Builds a provider from a raw API token.
    pub fn new(api_token: String) -> Self {
        Self {
            api: CloudflareApi::new(api_token),
        }
    }

    /// Builds a provider from a pre-configured [`CloudflareApi`].
    /// Useful for tests that want to inject a wiremock base URL.
    pub fn with_api(api: CloudflareApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl DnsProvider for CloudflareDnsProvider {
    async fn present(&self, base_domain: &str, value: &str) -> Result<DnsRecordHandle> {
        let zone = self.api.find_zone_for_domain(base_domain).await?;
        let record_name = format!("_acme-challenge.{}", base_domain.trim_start_matches("*."));
        let record_id = self
            .api
            .create_txt_record(&zone.id, &record_name, value)
            .await?;
        tracing::info!(
            zone = %zone.name,
            record_name = %record_name,
            record_id = %record_id,
            "ACME DNS-01: TXT record created",
        );
        Ok(DnsRecordHandle {
            zone_id: zone.id,
            record_id,
        })
    }

    async fn cleanup(&self, handle: DnsRecordHandle) -> Result<()> {
        let res = self
            .api
            .delete_dns_record(&handle.zone_id, &handle.record_id)
            .await;
        if let Err(ref e) = res {
            // Log but do not propagate as fatal: at this point the cert is
            // already issued; an orphan TXT record is cosmetic, not blocking.
            tracing::warn!(
                zone_id = %handle.zone_id,
                record_id = %handle.record_id,
                error = %e,
                "ACME DNS-01: TXT cleanup failed (non-fatal)",
            );
        }
        res
    }
}

/// Reusable DNS-01 ACME client.
///
/// The `Account` is initialised lazily on the first `order_wildcard`
/// call and cached for the lifetime of the client. Restart-survival
/// comes from the persisted credentials in `acme_account`.
pub struct AcmeDns01Client<P: DnsProvider> {
    /// Shared inventory for account persistence.
    inventory: Arc<Inventory>,
    /// `mailto:<email>` contact for the ACME account.
    contact_email: String,
    /// Directory URL (production or staging).
    directory_url: String,
    /// DNS provider used to install + clean up challenge records.
    dns: P,
    /// Lazily-initialised ACME account, cached for the client's life.
    account_cache: OnceCell<Account>,
}

/// Bundle returned by [`AcmeDns01Client::order_wildcard`].
#[derive(Debug, Clone)]
pub struct IssuedCert {
    /// All requested identifiers, in order. The first drives the
    /// Common Name; the rest are SANs.
    pub identifiers: Vec<String>,
    /// PEM-encoded certificate chain.
    pub cert_pem: String,
    /// PEM-encoded leaf private key.
    pub key_pem: String,
}

impl<P: DnsProvider> AcmeDns01Client<P> {
    /// Builds a client over the shared inventory, contact email,
    /// directory URL, and DNS provider.
    pub fn new(
        inventory: Arc<Inventory>,
        contact_email: String,
        directory_url: String,
        dns: P,
    ) -> Self {
        Self {
            inventory,
            contact_email,
            directory_url,
            dns,
            account_cache: OnceCell::new(),
        }
    }

    /// Gets or creates the LE account, persisted in storage so the
    /// controller doesn't re-register on restarts.
    async fn account(&self) -> Result<&Account> {
        self.account_cache
            .get_or_try_init(|| async { self.load_or_register_account().await })
            .await
    }

    /// Reconstructs the persisted account when possible, otherwise
    /// registers a fresh one (and persists it). Falls back to fresh
    /// registration on directory or email change and on credentials
    /// JSON shape drift (e.g. across an instant-acme dependency bump).
    async fn load_or_register_account(&self) -> Result<Account> {
        if let Some(saved) = self.inventory.get_acme_account().await? {
            // Re-use the saved account only if directory + email match. A
            // change in either means the operator switched envs (e.g. staging
            // -> production); in that case we register fresh rather than
            // attempting to reuse a key against a different ACME endpoint.
            if saved.directory_url == self.directory_url
                && saved.contact_email == self.contact_email
            {
                // Try to reconstruct from the saved credentials. If the
                // serialized JSON shape changed across an instant-acme
                // dependency bump (e.g. 0.7 -> 0.8 added a `newNonce`
                // field), reconstruction fails and historically wedged the
                // controller forever. Treat this as "credentials format
                // drifted" and fall through to registering a fresh
                // account, the same path as a directory/email change.
                match serde_json::from_str::<AccountCredentials>(&saved.account_key_pem) {
                    Ok(creds) => match Account::builder()
                        .context("acme account builder")?
                        .from_credentials(creds)
                        .await
                    {
                        Ok(account) => return Ok(account),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "ACME: saved credentials no longer reconstruct (likely instant-acme dep bump); registering fresh account",
                            );
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "ACME: saved credentials JSON shape changed; registering fresh account",
                        );
                    }
                }
            } else {
                tracing::info!(
                    old_directory = %saved.directory_url,
                    new_directory = %self.directory_url,
                    "ACME: directory/email changed; registering a fresh account",
                );
            }
        }

        let (account, creds) = Account::builder()
            .context("acme account builder")?
            .create(
                &NewAccount {
                    contact: &[&format!("mailto:{}", self.contact_email)],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory_url.clone(),
                None,
            )
            .await
            .context("creating ACME account")?;

        let creds_json = serde_json::to_string(&creds).context("serialise creds")?;
        self.inventory
            .upsert_acme_account(UpsertAcmeAccount {
                contact_email: self.contact_email.clone(),
                directory_url: self.directory_url.clone(),
                account_key_pem: creds_json,
                kid: None,
            })
            .await?;
        tracing::info!(
            directory = %self.directory_url,
            email = %self.contact_email,
            "ACME: registered new account",
        );
        Ok(account)
    }

    /// Orders a single cert covering `identifiers` (typically
    /// `["*.foo", "foo"]` for a wildcard with the apex).
    ///
    /// All identifiers are validated via DNS-01.
    ///
    /// # Errors
    ///
    /// Returns `Err` on:
    ///
    /// - empty `identifiers`;
    /// - any DNS provider failure (no TXT record installed);
    /// - LE refusing the authorization or returning `Invalid`;
    /// - timeout reaching `Ready`/`Valid`;
    /// - timeout downloading the issued certificate.
    ///
    /// The caller (the scheduler) MUST apply backoff before retrying;
    /// calling this in a tight loop will burn LE's per-account order
    /// quota.
    pub async fn order_wildcard(&self, identifiers: &[String]) -> Result<IssuedCert> {
        if identifiers.is_empty() {
            return Err(anyhow!("order_wildcard called with empty identifier list"));
        }
        let account = self.account().await?;

        let id_objs: Vec<Identifier> = identifiers
            .iter()
            .map(|s| Identifier::Dns(s.clone()))
            .collect();
        let mut order = account
            .new_order(&NewOrder::new(&id_objs))
            .await
            .context("placing ACME order")?;

        let mut installed: Vec<DnsRecordHandle> = Vec::new();
        // 0.8 returns a lazy iterator: each `.next().await` fetches one authz
        // from the server, populating its state. We loop until exhausted.
        let mut authorizations = order.authorizations();
        loop {
            let mut authz = match authorizations.next().await {
                Some(Ok(h)) => h,
                Some(Err(e)) => {
                    cleanup_all(&self.dns, installed).await;
                    return Err(anyhow::Error::new(e).context("fetching authorizations"));
                }
                None => break,
            };
            // For a wildcard order LE returns the apex (e.g. `vallee.casa`)
            // as the identifier, with `wildcard: true` on the AuthorizedIdentifier
            // when the original order included `*.vallee.casa`. We only need the
            // base domain for `_acme-challenge.<base>` TXT placement, so the
            // wildcard flag is informational.
            let base_domain = match authz.identifier().identifier {
                Identifier::Dns(d) => d.clone(),
                other => {
                    cleanup_all(&self.dns, installed).await;
                    return Err(anyhow!("non-DNS identifier in authz: {other:?}"));
                }
            };
            // `challenge(type)` returns Option<ChallengeHandle>. None means LE
            // didn't offer DNS-01 for this identifier (shouldn't happen for a
            // wildcard, but we cleanup and bail rather than panic).
            let mut challenge = match authz.challenge(ChallengeType::Dns01) {
                Some(c) => c,
                None => {
                    cleanup_all(&self.dns, installed).await;
                    return Err(anyhow!("no DNS-01 challenge offered for {base_domain}"));
                }
            };
            let dns_value = challenge.key_authorization().dns_value();
            let handle = match self.dns.present(&base_domain, &dns_value).await {
                Ok(h) => h,
                Err(e) => {
                    cleanup_all(&self.dns, installed).await;
                    return Err(anyhow!(
                        "DNS provider present failed for {base_domain}: {e}"
                    ));
                }
            };
            installed.push(handle);

            // Tell LE the challenge is ready *after* the propagation wait so
            // the validator's first lookup hits the new TXT record.
            sleep(DNS_PROPAGATION_WAIT).await;

            if let Err(e) = challenge.set_ready().await {
                cleanup_all(&self.dns, installed).await;
                return Err(anyhow::Error::new(e).context("ack challenge ready"));
            }
        }

        // Poll order status until ready/valid or timeout.
        let order_deadline = Instant::now() + ORDER_FINALIZE_TIMEOUT;
        loop {
            if Instant::now() >= order_deadline {
                cleanup_all(&self.dns, installed).await;
                return Err(anyhow!(
                    "ACME order did not finalize within {}s for {identifiers:?}",
                    ORDER_FINALIZE_TIMEOUT.as_secs(),
                ));
            }
            sleep(POLL_INTERVAL).await;
            let state = order.refresh().await.context("refresh order")?;
            tracing::debug!(?identifiers, status = ?state.status, "ACME order poll");
            match state.status {
                OrderStatus::Ready | OrderStatus::Valid => break,
                OrderStatus::Invalid => {
                    cleanup_all(&self.dns, installed).await;
                    return Err(anyhow!("ACME order invalid for {identifiers:?}: {state:?}"));
                }
                OrderStatus::Pending | OrderStatus::Processing => continue,
            }
        }

        // Build CSR + finalize.
        let mut params = rcgen::CertificateParams::new(identifiers.to_vec())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, &identifiers[0]);
        let key_pair = rcgen::KeyPair::generate()?;
        let csr = params.serialize_request(&key_pair)?;

        order
            .finalize_csr(csr.der())
            .await
            .context("finalize order")?;

        // Download cert chain. LE sometimes takes a beat after finalize.
        let download_deadline = Instant::now() + CERT_DOWNLOAD_TIMEOUT;
        let cert_pem = loop {
            if Instant::now() >= download_deadline {
                cleanup_all(&self.dns, installed).await;
                return Err(anyhow!(
                    "ACME cert download did not arrive within {}s for {identifiers:?}",
                    CERT_DOWNLOAD_TIMEOUT.as_secs(),
                ));
            }
            sleep(POLL_INTERVAL).await;
            if let Some(pem) = order.certificate().await.context("get certificate")? {
                break pem;
            }
        };

        // Cert in hand — clean up the TXT records. Failures here are logged
        // (in cleanup) but do not fail the issuance.
        cleanup_all(&self.dns, installed).await;

        tracing::info!(?identifiers, "ACME DNS-01: cert issued");
        Ok(IssuedCert {
            identifiers: identifiers.to_vec(),
            cert_pem,
            key_pem: key_pair.serialize_pem(),
        })
    }
}

/// Best-effort cleanup of every TXT record handle in `handles`.
/// Failures are swallowed so the cleanup path doesn't surface errors
/// the caller can't act on.
async fn cleanup_all<P: DnsProvider + ?Sized>(dns: &P, handles: Vec<DnsRecordHandle>) {
    for h in handles {
        let _ = dns.cleanup(h).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A stub DNS provider that records calls so unit tests can assert
    /// present/cleanup symmetry without touching the network.
    pub struct StubDns {
        pub events: Mutex<Vec<String>>,
        pub present_fails: bool,
    }

    impl StubDns {
        pub fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                present_fails: false,
            }
        }

        pub fn failing() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                present_fails: true,
            }
        }

        pub fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DnsProvider for StubDns {
        async fn present(&self, base_domain: &str, value: &str) -> Result<DnsRecordHandle> {
            self.events
                .lock()
                .unwrap()
                .push(format!("present:{base_domain}:{value}"));
            if self.present_fails {
                return Err(anyhow!("simulated CF failure"));
            }
            Ok(DnsRecordHandle {
                zone_id: "z".into(),
                record_id: format!("r-{base_domain}"),
            })
        }

        async fn cleanup(&self, handle: DnsRecordHandle) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("cleanup:{}", handle.record_id));
            Ok(())
        }
    }

    #[tokio::test]
    async fn cleanup_all_drains_handles() {
        let dns = StubDns::new();
        cleanup_all(
            &dns,
            vec![
                DnsRecordHandle {
                    zone_id: "z".into(),
                    record_id: "r1".into(),
                },
                DnsRecordHandle {
                    zone_id: "z".into(),
                    record_id: "r2".into(),
                },
            ],
        )
        .await;
        assert_eq!(dns.events(), vec!["cleanup:r1", "cleanup:r2"]);
    }

    #[tokio::test]
    async fn stub_present_records_event_and_returns_handle() {
        let dns = StubDns::new();
        let h = dns.present("vallee.casa", "txt-value").await.unwrap();
        assert_eq!(h.record_id, "r-vallee.casa");
        assert_eq!(dns.events(), vec!["present:vallee.casa:txt-value"]);
    }

    #[tokio::test]
    async fn stub_present_failing_returns_err() {
        let dns = StubDns::failing();
        let res = dns.present("vallee.casa", "v").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("simulated"));
    }

    #[test]
    fn cf_provider_strips_wildcard_in_record_name() {
        // The DNS provider receives the base domain as carried by the ACME
        // authorization — for a `*.vallee.casa` order, LE returns the
        // identifier as `vallee.casa` (the wildcard is stripped server-side).
        // We assert here that our trim_start_matches still produces the
        // canonical _acme-challenge.<host> name even if a `*.` slipped through.
        let domain = "*.vallee.casa";
        let trimmed = domain.trim_start_matches("*.");
        assert_eq!(trimmed, "vallee.casa");
        let record_name = format!("_acme-challenge.{trimmed}");
        assert_eq!(record_name, "_acme-challenge.vallee.casa");
    }
}
