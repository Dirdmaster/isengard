//! v0.3.6 Isengard-managed secrets store.
//!
//! Operators put secrets via `isd secret put` (or the dashboard later);
//! agents fetch them on container start over mTLS and mount them as
//! tmpfs at `/run/secrets/<name>` inside the workload container.
//!
//! Architecture:
//! - Master key derived from `ISENGARD_SECRETS_PASSPHRASE` on controller
//!   boot via age scrypt. Never stored on disk; never re-derived from
//!   anywhere but the env var.
//! - SQLite holds `(name, age-encrypted ciphertext, timestamps)`. Plaintext
//!   touches the controller process briefly and is wiped from the heap
//!   when the response goes out the door.
//! - Agent fetch path is gated by mTLS: the per-RPC interceptor already
//!   guarantees a valid agent cert on the connection. The
//!   `FetchSecret` handler additionally verifies the cert is alive
//!   (revocation set) and returns the plaintext bytes.
//!
//! Plaintext NEVER touches disk inside the controller. Logs refer to
//! secrets by `name` only.

use std::sync::Arc;

use age::secrecy::SecretString;
use age::{Decryptor, Encryptor, scrypt};
use isengard_storage::Inventory;
use std::io::{Read, Write};
use thiserror::Error;
use tracing::warn;

/// Env var the operator sets to unlock the secrets store on controller
/// boot. Same shape as the backup plugin's `ISENGARD_BACKUP_PASSPHRASE`.
pub const PASSPHRASE_ENV: &str = "ISENGARD_SECRETS_PASSPHRASE";

/// Errors raised by the secrets store. Distinct from sqlx errors so the
/// HTTP layer can map them cleanly to 4xx vs 5xx.
#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("ISENGARD_SECRETS_PASSPHRASE is not set on this controller")]
    NoPassphrase,

    #[error("secret {0:?} not found")]
    NotFound(String),

    #[error("storage: {0}")]
    Storage(#[from] isengard_storage::Error),

    #[error("encrypt: {0}")]
    Encrypt(#[from] age::EncryptError),

    #[error("decrypt: {0}")]
    Decrypt(#[from] age::DecryptError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Controller-side handle for the secrets store.
///
/// Cheap to clone (`Arc` inside). The passphrase is held in a
/// [`SecretString`] so it isn't accidentally `Debug`-logged. We
/// re-derive the age `scrypt::Recipient` / `scrypt::Identity` on each
/// encrypt/decrypt call: derivation cost is dominated by the per-message
/// work factor (intrinsic to age) and a passphrase clone here is fine.
#[derive(Clone)]
pub struct SecretsStore {
    inv: Arc<Inventory>,
    passphrase: Option<SecretString>,
}

impl SecretsStore {
    /// Build a store from the inventory + the env-derived passphrase.
    /// `passphrase = None` means the env var was unset; calls that need
    /// to encrypt or decrypt will return [`SecretsError::NoPassphrase`].
    pub fn new(inv: Arc<Inventory>, passphrase: Option<String>) -> Self {
        let passphrase = passphrase.map(SecretString::from);
        Self { inv, passphrase }
    }

    /// Read the env var once and build a store. Use this from controller
    /// boot. The caller is responsible for the boot-time fail-loud check
    /// when the DB has secrets but the env var is missing.
    pub fn from_env(inv: Arc<Inventory>) -> Self {
        let passphrase = std::env::var(PASSPHRASE_ENV).ok().filter(|s| !s.is_empty());
        Self::new(inv, passphrase)
    }

    /// True iff the controller has the master key in memory.
    pub fn is_unlocked(&self) -> bool {
        self.passphrase.is_some()
    }

    /// Insert a brand-new secret. Errors on duplicate name (use
    /// [`SecretsStore::put`] for upsert).
    pub async fn create(
        &self,
        name: &str,
        plaintext: &[u8],
        created_by: Option<&str>,
    ) -> Result<(), SecretsError> {
        let cipher = self.encrypt(plaintext)?;
        self.inv
            .insert_secret_strict(name, &cipher, created_by)
            .await?;
        // Log by name only; never the value.
        tracing::info!(name = %name, "secret created");
        Ok(())
    }

    /// Upsert: replace if present, insert if missing. Returns whether a
    /// new row was inserted.
    pub async fn put(
        &self,
        name: &str,
        plaintext: &[u8],
        created_by: Option<&str>,
    ) -> Result<bool, SecretsError> {
        let cipher = self.encrypt(plaintext)?;
        let inserted = self.inv.upsert_secret(name, &cipher, created_by).await?;
        tracing::info!(name = %name, inserted, "secret put");
        Ok(inserted)
    }

    /// Fetch + decrypt. Used by the agent-facing handler (mTLS-gated)
    /// and by the optional `isd secret get --reveal` flow.
    pub async fn fetch(&self, name: &str) -> Result<Vec<u8>, SecretsError> {
        let cipher = self
            .inv
            .get_secret_ciphertext(name)
            .await?
            .ok_or_else(|| SecretsError::NotFound(name.to_string()))?;
        self.decrypt(&cipher)
    }

    /// Public-safe metadata for one secret, no value.
    pub async fn meta(
        &self,
        name: &str,
    ) -> Result<Option<isengard_storage::SecretMeta>, SecretsError> {
        Ok(self.inv.get_secret_meta(name).await?)
    }

    /// List every secret's metadata.
    pub async fn list(&self) -> Result<Vec<isengard_storage::SecretMeta>, SecretsError> {
        Ok(self.inv.list_secrets().await?)
    }

    /// Delete by name.
    pub async fn delete(&self, name: &str) -> Result<bool, SecretsError> {
        let removed = self.inv.delete_secret(name).await?;
        if removed {
            tracing::info!(name = %name, "secret deleted");
        }
        Ok(removed)
    }

    /// Boot-time guard: if the DB has secrets and we have no passphrase,
    /// fail loud. Returns Ok(()) when either:
    ///   - the env var IS set (we can decrypt anything stored), OR
    ///   - the DB has zero secrets (a fresh install can defer the
    ///     passphrase until the first put).
    pub async fn boot_check(&self) -> Result<(), SecretsError> {
        if self.passphrase.is_some() {
            return Ok(());
        }
        if self.inv.has_any_secret().await? {
            warn!(
                "controller has stored secrets but {} is unset; refusing to start",
                PASSPHRASE_ENV
            );
            return Err(SecretsError::NoPassphrase);
        }
        Ok(())
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let pw = self.passphrase.as_ref().ok_or(SecretsError::NoPassphrase)?;
        let encryptor = Encryptor::with_user_passphrase(pw.clone());
        let mut out = Vec::with_capacity(plaintext.len() + 64);
        let mut writer = encryptor.wrap_output(&mut out)?;
        writer.write_all(plaintext)?;
        writer.finish()?;
        Ok(out)
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let pw = self.passphrase.as_ref().ok_or(SecretsError::NoPassphrase)?;
        let decryptor = Decryptor::new(ciphertext)?;
        let identity = scrypt::Identity::new(pw.clone());
        let mut reader = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?;
        let mut plain = Vec::new();
        reader.read_to_end(&mut plain)?;
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store_with(pass: Option<&str>) -> SecretsStore {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        SecretsStore::new(inv, pass.map(str::to_string))
    }

    #[tokio::test]
    async fn put_then_fetch_round_trips() {
        let store = store_with(Some("hunter2-correct-horse")).await;
        store
            .put("api_token", b"super-secret-bytes", Some("operator"))
            .await
            .unwrap();
        let got = store.fetch("api_token").await.unwrap();
        assert_eq!(got, b"super-secret-bytes");
    }

    #[tokio::test]
    async fn fetch_missing_yields_not_found() {
        let store = store_with(Some("p")).await;
        let err = store.fetch("nope").await.unwrap_err();
        assert!(matches!(err, SecretsError::NotFound(ref n) if n == "nope"));
    }

    #[tokio::test]
    async fn put_without_passphrase_errors() {
        let store = store_with(None).await;
        let err = store.put("k", b"v", None).await.unwrap_err();
        assert!(matches!(err, SecretsError::NoPassphrase));
    }

    #[tokio::test]
    async fn fetch_with_wrong_passphrase_fails_decrypt() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let s1 = SecretsStore::new(inv.clone(), Some("first-pass".into()));
        s1.put("k", b"value", None).await.unwrap();
        let s2 = SecretsStore::new(inv, Some("different".into()));
        let err = s2.fetch("k").await.unwrap_err();
        assert!(
            matches!(err, SecretsError::Decrypt(_)),
            "expected decrypt error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let store = store_with(Some("p")).await;
        store.create("dup", b"a", None).await.unwrap();
        let err = store.create("dup", b"b", None).await.unwrap_err();
        assert!(matches!(err, SecretsError::Storage(_)));
    }

    #[tokio::test]
    async fn list_omits_ciphertext_and_plaintext() {
        let store = store_with(Some("p")).await;
        store.put("a", b"plain-value", None).await.unwrap();
        let list = store.list().await.unwrap();
        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("plain-value"));
        assert!(!json.contains("ciphertext"));
    }

    #[tokio::test]
    async fn boot_check_passes_with_no_secrets_and_no_passphrase() {
        let store = store_with(None).await;
        store.boot_check().await.unwrap();
    }

    #[tokio::test]
    async fn boot_check_fails_when_db_has_secrets_but_passphrase_missing() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        // Stuff a row in directly (encrypted with some other key).
        inv.upsert_secret("k", &[0xff; 32], None).await.unwrap();
        let store = SecretsStore::new(inv, None);
        let err = store.boot_check().await.unwrap_err();
        assert!(matches!(err, SecretsError::NoPassphrase));
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let store = store_with(Some("p")).await;
        store.put("k", b"v", None).await.unwrap();
        assert!(store.delete("k").await.unwrap());
        assert!(!store.delete("k").await.unwrap());
    }
}
