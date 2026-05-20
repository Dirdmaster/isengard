//! v0.3.6 Isengard-managed secrets store.
//!
//! Operators bootstrap secrets at install time (the installer pipes each
//! value through `isengard secret bootstrap <name>`); agents fetch them
//! on container start over mTLS and mount them as tmpfs at
//! `/run/secrets/<name>` inside the workload container.
//!
//! Architecture (matches Docker Swarm):
//! - The installer generates a 32-byte random master key and writes it
//!   to `/etc/isengard/master.key` mode 0600 root. The operator never
//!   sees or types it.
//! - The controller container bind-mounts that key at
//!   `/run/secrets/master.key` (configurable via
//!   `ISENGARD_MASTER_KEY_FILE`) and reads the raw 32 bytes once on boot.
//!   No env var ever holds secret material.
//! - Each row in the SQLite `secrets` table holds a ChaCha20-Poly1305
//!   ciphertext: 12-byte nonce prepended to the AEAD output, AAD = the
//!   secret name. Plaintext touches the controller process briefly and
//!   is wiped from the heap when the response goes out the door.
//! - Agent fetch path is gated by mTLS: the per-RPC interceptor already
//!   guarantees a valid agent cert on the connection. The
//!   `FetchSecret` handler additionally verifies the cert is alive
//!   (revocation set) and returns the plaintext bytes.
//!
//! Plaintext NEVER touches disk inside the controller. Logs refer to
//! secrets by `name` only. The master key is held in a [`SecretBox`] so
//! it isn't accidentally `Debug`-logged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use isengard_storage::Inventory;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox};
use thiserror::Error;
use tracing::warn;

/// Env var pointing to the raw 32-byte master key file. Defaults to
/// `/run/secrets/master.key`, which is where `install/compose.yaml`
/// bind-mounts `/etc/isengard/master.key` inside the controller
/// container.
pub const MASTER_KEY_FILE_ENV: &str = "ISENGARD_MASTER_KEY_FILE";

/// Default location of the master key file inside the controller
/// container. Operators install it on the host at
/// `/etc/isengard/master.key`.
pub const DEFAULT_MASTER_KEY_FILE: &str = "/run/secrets/master.key";

/// Length of the symmetric key in bytes.
const KEY_LEN: usize = 32;

/// Length of the ChaCha20-Poly1305 nonce in bytes.
const NONCE_LEN: usize = 12;

/// Errors raised by the secrets store.
///
/// Distinct from sqlx errors so the HTTP layer can map cleanly to 4xx
/// versus 5xx and so the gRPC handler can pick the right `Status`.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// Master key file is missing or not readable. Includes the
    /// underlying io error.
    #[error(
        "master key file not readable (set ISENGARD_MASTER_KEY_FILE or write /run/secrets/master.key): {0}"
    )]
    MasterKeyUnreadable(#[source] std::io::Error),

    /// Master key file is the wrong size. The controller fails to
    /// start rather than guess at padding.
    #[error("master key file is {actual} bytes; expected 32")]
    MasterKeyWrongSize {
        /// Actual file size in bytes.
        actual: usize,
    },

    /// Store is locked. The controller refused to boot, or a test
    /// constructed a locked store explicitly.
    #[error("secrets store has no master key loaded; controller cannot encrypt or decrypt")]
    MasterKeyMissing,

    /// No row with that secret name.
    #[error("secret {0:?} not found")]
    NotFound(String),

    /// Underlying storage error (sqlx, migration, etc).
    #[error("storage: {0}")]
    Storage(#[from] isengard_storage::Error),

    /// AEAD encrypt step failed.
    #[error("encrypt: {0}")]
    Encrypt(chacha20poly1305::Error),

    /// AEAD decrypt step failed (corrupt ciphertext, swapped row,
    /// wrong key).
    #[error("decrypt: {0}")]
    Decrypt(chacha20poly1305::Error),

    /// Ciphertext shorter than the nonce; can't even attempt
    /// decryption.
    #[error("ciphertext shorter than nonce ({actual} < 12)")]
    CiphertextTruncated {
        /// Actual ciphertext length in bytes.
        actual: usize,
    },

    /// Generic IO error (not bound to a specific code path).
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
}

/// Wrapper around the raw 32-byte master key.
///
/// [`SecretBox`] ensures the bytes aren't accidentally `Debug`-logged
/// or copied into traces.
type MasterKey = SecretBox<[u8; KEY_LEN]>;

/// Controller-side handle for the secrets store.
///
/// Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct SecretsStore {
    /// Shared inventory pool. The `secrets` table lives there.
    inv: Arc<Inventory>,
    /// Master key, or `None` for a locked store.
    ///
    /// `None` only in the rare unlocked-fresh-install case where the
    /// controller booted before any master key was provisioned. Every
    /// encrypt and decrypt path returns
    /// [`SecretsError::MasterKeyMissing`] in that state.
    key: Option<Arc<MasterKey>>,
}

impl SecretsStore {
    /// Builds a store from the inventory and a raw 32-byte master
    /// key.
    ///
    /// Used directly by the bootstrap subcommand and by tests.
    pub fn new(inv: Arc<Inventory>, key: [u8; KEY_LEN]) -> Self {
        Self {
            inv,
            key: Some(Arc::new(SecretBox::new(Box::new(key)))),
        }
    }

    /// Builds a store with no master key.
    ///
    /// Encrypt and decrypt calls return
    /// [`SecretsError::MasterKeyMissing`]. Useful for tests of the
    /// locked-store path.
    pub fn new_locked(inv: Arc<Inventory>) -> Self {
        Self { inv, key: None }
    }

    /// Reads the master key from the path in
    /// [`MASTER_KEY_FILE_ENV`] (default
    /// [`DEFAULT_MASTER_KEY_FILE`]) and builds a store.
    ///
    /// Use this from controller boot.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the file is missing, unreadable, or the
    /// wrong size. Bubble up so the controller fails loud at startup.
    pub fn from_env(inv: Arc<Inventory>) -> Result<Self, SecretsError> {
        let path = master_key_path_from_env();
        let key = read_master_key(&path)?;
        Ok(Self::new(inv, key))
    }

    /// Returns `true` when the controller has the master key in
    /// memory.
    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    /// Inserts a brand-new secret.
    ///
    /// # Errors
    ///
    /// Returns `Err` on duplicate name (use [`SecretsStore::put`] for
    /// upsert) or any encryption or storage failure.
    pub async fn create(
        &self,
        name: &str,
        plaintext: &[u8],
        created_by: Option<&str>,
    ) -> Result<(), SecretsError> {
        let cipher = self.encrypt(name, plaintext)?;
        self.inv
            .insert_secret_strict(name, &cipher, created_by)
            .await?;
        // Log by name only; never the value.
        tracing::info!(name = %name, "secret created");
        Ok(())
    }

    /// Upserts a secret: replaces if present, inserts if missing.
    ///
    /// Returns `true` when a new row was inserted, `false` when an
    /// existing row was replaced.
    ///
    /// # Errors
    ///
    /// Returns `Err` on encryption or storage failure.
    pub async fn put(
        &self,
        name: &str,
        plaintext: &[u8],
        created_by: Option<&str>,
    ) -> Result<bool, SecretsError> {
        let cipher = self.encrypt(name, plaintext)?;
        let inserted = self.inv.upsert_secret(name, &cipher, created_by).await?;
        tracing::info!(name = %name, inserted, "secret put");
        Ok(inserted)
    }

    /// Fetches and decrypts a secret.
    ///
    /// The agent-facing `FetchSecret` handler calls this; the auth
    /// layer has already gated the call behind a valid client cert.
    ///
    /// # Errors
    ///
    /// Returns `Err` on missing row, decrypt failure, or storage
    /// failure.
    pub async fn fetch(&self, name: &str) -> Result<Vec<u8>, SecretsError> {
        let cipher = self
            .inv
            .get_secret_ciphertext(name)
            .await?
            .ok_or_else(|| SecretsError::NotFound(name.to_string()))?;
        self.decrypt(name, &cipher)
    }

    /// Returns public-safe metadata for one secret. Never includes
    /// the value or the ciphertext.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure.
    pub async fn meta(
        &self,
        name: &str,
    ) -> Result<Option<isengard_storage::SecretMeta>, SecretsError> {
        Ok(self.inv.get_secret_meta(name).await?)
    }

    /// Lists every secret's metadata. Never includes values or
    /// ciphertexts.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure.
    pub async fn list(&self) -> Result<Vec<isengard_storage::SecretMeta>, SecretsError> {
        Ok(self.inv.list_secrets().await?)
    }

    /// Deletes a secret by name. Returns `true` when a row was
    /// removed.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure.
    pub async fn delete(&self, name: &str) -> Result<bool, SecretsError> {
        let removed = self.inv.delete_secret(name).await?;
        if removed {
            tracing::info!(name = %name, "secret deleted");
        }
        Ok(removed)
    }

    /// Boot-time guard: confirms the store is unlocked.
    ///
    /// Called once at controller startup. When it returns an error
    /// the controller refuses to start.
    ///
    /// # Errors
    ///
    /// Returns [`SecretsError::MasterKeyMissing`] when the store is
    /// locked.
    pub async fn boot_check(&self) -> Result<(), SecretsError> {
        if !self.is_unlocked() {
            warn!(
                "controller boot: master key not loaded (set {} or write {})",
                MASTER_KEY_FILE_ENV, DEFAULT_MASTER_KEY_FILE,
            );
            return Err(SecretsError::MasterKeyMissing);
        }
        Ok(())
    }

    /// Encrypts `plaintext` for `name` with a fresh 12-byte nonce.
    /// Wire format is `nonce || aead-output`. AAD is `name.as_bytes()`
    /// to defend against row swaps.
    fn encrypt(&self, name: &str, plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let key_box = self
            .key
            .as_ref()
            .ok_or(SecretsError::MasterKeyMissing)?
            .clone();
        let key = Key::from_slice(key_box.expose_secret().as_slice());
        let cipher = ChaCha20Poly1305::new(key);

        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let aad = name.as_bytes();
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(SecretsError::Encrypt)?;

        // Wire format: nonce || ciphertext-with-tag.
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Reverse of [`SecretsStore::encrypt`]. Splits the wire blob into
    /// nonce and ciphertext, runs AEAD decrypt with `name.as_bytes()`
    /// as AAD.
    fn decrypt(&self, name: &str, blob: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let key_box = self
            .key
            .as_ref()
            .ok_or(SecretsError::MasterKeyMissing)?
            .clone();
        if blob.len() < NONCE_LEN {
            return Err(SecretsError::CiphertextTruncated { actual: blob.len() });
        }
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        let key = Key::from_slice(key_box.expose_secret().as_slice());
        let cipher = ChaCha20Poly1305::new(key);
        let nonce = Nonce::from_slice(nonce_bytes);
        let aad = name.as_bytes();
        cipher
            .decrypt(nonce, Payload { msg: ct, aad })
            .map_err(SecretsError::Decrypt)
    }
}

/// Resolves the master key file path from the environment, falling
/// back to [`DEFAULT_MASTER_KEY_FILE`].
pub fn master_key_path_from_env() -> PathBuf {
    std::env::var(MASTER_KEY_FILE_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MASTER_KEY_FILE))
}

/// Reads a 32-byte master key from `path`.
///
/// # Errors
///
/// Returns [`SecretsError::MasterKeyUnreadable`] when the file is
/// missing or unreadable, and [`SecretsError::MasterKeyWrongSize`]
/// when the file is the wrong length.
pub fn read_master_key(path: &Path) -> Result<[u8; KEY_LEN], SecretsError> {
    let bytes = std::fs::read(path).map_err(SecretsError::MasterKeyUnreadable)?;
    if bytes.len() != KEY_LEN {
        return Err(SecretsError::MasterKeyWrongSize {
            actual: bytes.len(),
        });
    }
    let mut arr = [0u8; KEY_LEN];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

/// Generates a fresh 32-byte master key.
///
/// Used by the optional controller-side init helper; the installer
/// normally generates the key via `openssl rand 32` so it never
/// round-trips through the controller binary.
pub fn generate_master_key() -> [u8; KEY_LEN] {
    let mut k = [0u8; KEY_LEN];
    rand::thread_rng().fill_bytes(&mut k);
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> [u8; KEY_LEN] {
        let mut k = [0u8; KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    async fn store_with(key: [u8; KEY_LEN]) -> SecretsStore {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        SecretsStore::new(inv, key)
    }

    #[tokio::test]
    async fn put_then_fetch_round_trips() {
        let store = store_with(fixed_key()).await;
        store
            .put("api_token", b"super-secret-bytes", Some("operator"))
            .await
            .unwrap();
        let got = store.fetch("api_token").await.unwrap();
        assert_eq!(got, b"super-secret-bytes");
    }

    #[tokio::test]
    async fn nonce_changes_per_encrypt_so_ciphertexts_differ() {
        // Same key, same plaintext, two encrypts: the random nonce
        // must produce distinct ciphertexts. Otherwise we leak that
        // two stored secrets had the same value.
        let store = store_with(fixed_key()).await;
        let c1 = store.encrypt("k", b"same-value").unwrap();
        let c2 = store.encrypt("k", b"same-value").unwrap();
        assert_ne!(c1, c2);
    }

    #[tokio::test]
    async fn fetch_missing_yields_not_found() {
        let store = store_with(fixed_key()).await;
        let err = store.fetch("nope").await.unwrap_err();
        assert!(matches!(err, SecretsError::NotFound(ref n) if n == "nope"));
    }

    #[tokio::test]
    async fn put_without_key_errors() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let store = SecretsStore::new_locked(inv);
        let err = store.put("k", b"v", None).await.unwrap_err();
        assert!(matches!(err, SecretsError::MasterKeyMissing));
    }

    #[tokio::test]
    async fn decrypt_with_different_key_fails() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let s1 = SecretsStore::new(inv.clone(), fixed_key());
        s1.put("k", b"value", None).await.unwrap();

        let mut other = fixed_key();
        other[0] ^= 0xff;
        let s2 = SecretsStore::new(inv, other);
        let err = s2.fetch("k").await.unwrap_err();
        assert!(
            matches!(err, SecretsError::Decrypt(_)),
            "expected decrypt error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn decrypt_with_swapped_aad_fails() {
        // AAD is the secret name. Renaming the row in storage without
        // re-encrypting must fail decrypt: catches a class of attacks
        // where ciphertexts get shuffled across rows.
        let store = store_with(fixed_key()).await;
        let blob = store.encrypt("real_name", b"value").unwrap();
        let err = store.decrypt("not_real_name", &blob).unwrap_err();
        assert!(matches!(err, SecretsError::Decrypt(_)));
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let store = store_with(fixed_key()).await;
        store.create("dup", b"a", None).await.unwrap();
        let err = store.create("dup", b"b", None).await.unwrap_err();
        assert!(matches!(err, SecretsError::Storage(_)));
    }

    #[tokio::test]
    async fn list_omits_ciphertext_and_plaintext() {
        let store = store_with(fixed_key()).await;
        store.put("a", b"plain-value", None).await.unwrap();
        let list = store.list().await.unwrap();
        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("plain-value"));
        assert!(!json.contains("ciphertext"));
    }

    #[tokio::test]
    async fn boot_check_passes_when_unlocked() {
        let store = store_with(fixed_key()).await;
        store.boot_check().await.unwrap();
    }

    #[tokio::test]
    async fn boot_check_fails_when_locked() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let store = SecretsStore::new_locked(inv);
        let err = store.boot_check().await.unwrap_err();
        assert!(matches!(err, SecretsError::MasterKeyMissing));
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let store = store_with(fixed_key()).await;
        store.put("k", b"v", None).await.unwrap();
        assert!(store.delete("k").await.unwrap());
        assert!(!store.delete("k").await.unwrap());
    }

    #[test]
    fn read_master_key_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.key");
        let err = read_master_key(&path).unwrap_err();
        assert!(matches!(err, SecretsError::MasterKeyUnreadable(_)));
    }

    #[test]
    fn read_master_key_rejects_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.key");
        std::fs::write(&path, b"too-short").unwrap();
        let err = read_master_key(&path).unwrap_err();
        assert!(
            matches!(err, SecretsError::MasterKeyWrongSize { actual } if actual == 9),
            "got {err:?}"
        );
    }

    #[test]
    fn read_master_key_accepts_exactly_32_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.key");
        std::fs::write(&path, [0xabu8; KEY_LEN]).unwrap();
        let key = read_master_key(&path).unwrap();
        assert_eq!(key, [0xabu8; KEY_LEN]);
    }

    #[test]
    fn read_master_key_rejects_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.key");
        std::fs::write(&path, vec![0u8; KEY_LEN + 1]).unwrap();
        let err = read_master_key(&path).unwrap_err();
        assert!(
            matches!(err, SecretsError::MasterKeyWrongSize { actual } if actual == KEY_LEN + 1)
        );
    }
}
