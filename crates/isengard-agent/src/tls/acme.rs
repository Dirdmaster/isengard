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
use tokio::time::sleep;

pub const LE_PRODUCTION_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";
pub const LE_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

pub struct AcmeClient {
    inventory: Arc<Inventory>,
    challenges: Arc<ChallengeState>,
    contact_email: String,
    directory_url: String,
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
        }
    }

    /// Get or create the LE account, persisted in storage so we don't
    /// re-register on restarts.
    async fn account(&self) -> Result<Account> {
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

        for authz in &authorizations {
            let challenge = authz
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01)
                .ok_or_else(|| anyhow!("no HTTP-01 challenge for {hostname}"))?;
            let key_auth = order.key_authorization(challenge);
            self.challenges
                .install(&challenge.token, key_auth.as_str())
                .await;
            order
                .set_challenge_ready(&challenge.url)
                .await
                .context("ack challenge ready")?;
        }

        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > 30 {
                return Err(anyhow!("ACME order did not finalize after 30 polls"));
            }
            sleep(Duration::from_secs(2)).await;
            let state = order.refresh().await.context("refresh order")?;
            match state.status {
                OrderStatus::Ready | OrderStatus::Valid => break,
                OrderStatus::Invalid => {
                    return Err(anyhow!("ACME order invalid: {:?}", state));
                }
                OrderStatus::Pending | OrderStatus::Processing => continue,
            }
        }

        // Cleanup challenge tokens.
        for authz in &authorizations {
            for c in &authz.challenges {
                self.challenges.remove(&c.token).await;
            }
        }

        // Generate CSR + finalize.
        let mut params = rcgen::CertificateParams::new(vec![hostname.to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, hostname);
        let key_pair = rcgen::KeyPair::generate()?;
        let csr = params.serialize_request(&key_pair)?;

        order.finalize(csr.der()).await.context("finalize order")?;

        // Download cert chain.
        let cert_pem = loop {
            attempts += 1;
            if attempts > 60 {
                return Err(anyhow!("ACME cert download did not arrive"));
            }
            sleep(Duration::from_secs(2)).await;
            if let Some(pem) = order.certificate().await.context("get certificate")? {
                break pem;
            }
        };

        Ok(IssuedCert {
            cert_pem,
            key_pem: key_pair.serialize_pem(),
        })
    }
}
