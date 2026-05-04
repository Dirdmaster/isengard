//! Thin wrapper around `instant-acme`. Account registration + persistence,
//! HTTP-01 order orchestration, finalization, cert download.

use crate::tls::ChallengeState;
use anyhow::{Context, Result, anyhow};
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
};
use isengard_storage::{Inventory, UpsertAcmeAccount};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio::time::{Instant, sleep};

pub const LE_PRODUCTION_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";
pub const LE_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Wall-clock budgets for the order lifecycle.
const ORDER_FINALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const CERT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Thin wrapper around `instant-acme`. The `Account` is initialised lazily on
/// the first `order()` call and cached for the lifetime of the client (next
/// `order()` reuses it; restart-survival comes from the persisted credentials
/// in `acme_account`).
///
/// **Caller contract:** the function returns `Err` on rate-limit and other
/// transient ACME errors. The caller MUST apply backoff before retrying —
/// the renewal scheduler in `tls/renewal.rs` is the production caller and
/// implements this. Calling `order()` in a tight loop on failure will burn
/// LE's per-domain order quota.
pub struct AcmeClient {
    inventory: Arc<Inventory>,
    challenges: Arc<ChallengeState>,
    contact_email: String,
    directory_url: String,
    /// Cached `Account` so we don't hit the storage layer (and possibly LE
    /// for kid validation) on every `order()` call. Initialised on first use.
    account_cache: OnceCell<Account>,
}

pub struct IssuedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

impl AcmeClient {
    pub fn new(
        inventory: Arc<Inventory>,
        challenges: Arc<ChallengeState>,
        contact_email: String,
        directory_url: String,
    ) -> Self {
        Self {
            inventory,
            challenges,
            contact_email,
            directory_url,
            account_cache: OnceCell::new(),
        }
    }

    /// Get or create the LE account, persisted in storage so we don't
    /// re-register on restarts. Cached after first call so concurrent
    /// `order()` invocations share one `Account` instance.
    async fn account(&self) -> Result<&Account> {
        self.account_cache
            .get_or_try_init(|| async { self.load_or_register_account().await })
            .await
    }

    async fn load_or_register_account(&self) -> Result<Account> {
        if let Some(saved) = self.inventory.get_acme_account().await? {
            // We stored the AccountCredentials JSON in the account_key_pem
            // column (slight name mismatch — kept the column name from
            // Plan A's spec; the contents are JSON not PEM).
            let creds: AccountCredentials =
                serde_json::from_str(&saved.account_key_pem).context("decode acme creds JSON")?;
            return Account::from_credentials(creds)
                .await
                .context("reconstruct acme account");
        }

        let (account, creds) = Account::create(
            &NewAccount {
                contact: &[&format!("mailto:{}", self.contact_email)],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            &self.directory_url,
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
            "ACME: registered new account"
        );
        Ok(account)
    }

    /// Order a cert for `hostname` via HTTP-01. Returns the issued PEM pair.
    pub async fn order(&self, hostname: &str) -> Result<IssuedCert> {
        let account = self.account().await?;

        let identifier = Identifier::Dns(hostname.to_string());
        let mut order = account
            .new_order(&NewOrder {
                identifiers: &[identifier],
            })
            .await
            .context("placing ACME order")?;

        let authorizations = order
            .authorizations()
            .await
            .context("fetching authorizations")?;

        // Install all HTTP-01 challenges. Track installed tokens so a
        // mid-loop failure can clean them up — otherwise they'd leak in
        // the in-memory ChallengeState until process restart.
        let mut installed_tokens: Vec<String> = Vec::new();
        for authz in &authorizations {
            let challenge = match authz
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01)
            {
                Some(c) => c,
                None => {
                    cleanup_tokens(&self.challenges, &installed_tokens).await;
                    return Err(anyhow!("no HTTP-01 challenge for {hostname}"));
                }
            };
            let key_auth = order.key_authorization(challenge);
            self.challenges
                .install(&challenge.token, key_auth.as_str())
                .await;
            installed_tokens.push(challenge.token.clone());
            if let Err(e) = order
                .set_challenge_ready(&challenge.url)
                .await
                .context("ack challenge ready")
            {
                cleanup_tokens(&self.challenges, &installed_tokens).await;
                return Err(e);
            }
        }

        // Wall-clock deadline (not poll-count) so a long network round-trip
        // counts toward the budget consistently.
        let order_deadline = Instant::now() + ORDER_FINALIZE_TIMEOUT;
        loop {
            if Instant::now() >= order_deadline {
                cleanup_tokens(&self.challenges, &installed_tokens).await;
                return Err(anyhow!(
                    "ACME order did not finalize within {}s for {hostname}",
                    ORDER_FINALIZE_TIMEOUT.as_secs()
                ));
            }
            sleep(POLL_INTERVAL).await;
            let state = order.refresh().await.context("refresh order")?;
            tracing::debug!(hostname = %hostname, status = ?state.status, "ACME order poll");
            match state.status {
                OrderStatus::Ready | OrderStatus::Valid => break,
                OrderStatus::Invalid => {
                    cleanup_tokens(&self.challenges, &installed_tokens).await;
                    return Err(anyhow!("ACME order invalid for {hostname}: {state:?}"));
                }
                OrderStatus::Pending | OrderStatus::Processing => continue,
            }
        }

        // Cleanup challenge tokens — order is past the validation gate, so
        // the in-memory entries are no longer needed.
        cleanup_tokens(&self.challenges, &installed_tokens).await;

        // Generate CSR + finalize.
        let mut params = rcgen::CertificateParams::new(vec![hostname.to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, hostname);
        let key_pair = rcgen::KeyPair::generate()?;
        let csr = params.serialize_request(&key_pair)?;

        order.finalize(csr.der()).await.context("finalize order")?;

        // Download cert chain (separate deadline; LE sometimes takes a beat
        // after finalize before the cert chain is retrievable).
        let download_deadline = Instant::now() + CERT_DOWNLOAD_TIMEOUT;
        let cert_pem = loop {
            if Instant::now() >= download_deadline {
                return Err(anyhow!(
                    "ACME cert download did not arrive within {}s for {hostname}",
                    CERT_DOWNLOAD_TIMEOUT.as_secs()
                ));
            }
            sleep(POLL_INTERVAL).await;
            if let Some(pem) = order.certificate().await.context("get certificate")? {
                break pem;
            }
        };

        tracing::info!(hostname = %hostname, "ACME: cert issued");
        Ok(IssuedCert {
            cert_pem,
            key_pem: key_pair.serialize_pem(),
        })
    }
}

/// Best-effort cleanup of installed ChallengeState tokens. Called from every
/// error-return path and from the success path after the order is past
/// validation. Removing a missing token is a no-op.
async fn cleanup_tokens(challenges: &ChallengeState, tokens: &[String]) {
    for t in tokens {
        challenges.remove(t).await;
    }
}
