//! Phase 11a: age passphrase encryption.
//!
//! v1 ships passphrase-only (PBKDF2 via `age::scrypt`). X25519 recipients are
//! deferred until SaaS escrow (later phase). The passphrase is provided to
//! the controller process via the `ISENGARD_BACKUP_PASSPHRASE` env var; the
//! DB persists only a 12-hex-char SHA-256 fingerprint so the UI can confirm
//! the running controller's passphrase matches what the operator stored
//! during setup.
//!
//! On decryption the work factor is bumped via `Identity::with_max_work_factor`
//! to keep tests reasonable; production decrypts inherit the on-disk header's
//! work factor.

use std::io::{Read, Write};

use age::secrecy::SecretString;
use age::{Decryptor, Encryptor, scrypt};
use sha2::{Digest, Sha256};

/// Errors raised by the encryption module.
#[derive(Debug, thiserror::Error)]
pub enum EncryptError {
    #[error("age encrypt: {0}")]
    AgeEncrypt(#[from] age::EncryptError),

    #[error("age decrypt: {0}")]
    AgeDecrypt(#[from] age::DecryptError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("passphrase is empty")]
    EmptyPassphrase,
}

/// Encrypt `plaintext` with a passphrase. Output is the binary age format
/// (no armor); store as `<name>.db.age` on the destination.
pub fn encrypt_with_passphrase(
    plaintext: &[u8],
    passphrase: &str,
) -> Result<Vec<u8>, EncryptError> {
    if passphrase.is_empty() {
        return Err(EncryptError::EmptyPassphrase);
    }
    let recipient = scrypt::Recipient::new(SecretString::from(passphrase.to_string()));
    let encryptor = Encryptor::with_user_passphrase(SecretString::from(passphrase.to_string()));
    // Discard the unused recipient binding; future-proof against the API
    // exposing passphrase encryption purely through `with_user_passphrase`.
    let _ = recipient;

    let mut out = Vec::new();
    let mut writer = encryptor.wrap_output(&mut out)?;
    writer.write_all(plaintext)?;
    writer.finish()?;
    Ok(out)
}

/// Decrypt an age blob with a passphrase. Used by the future restore flow
/// and by integration tests.
pub fn decrypt_with_passphrase(
    ciphertext: &[u8],
    passphrase: &str,
) -> Result<Vec<u8>, EncryptError> {
    if passphrase.is_empty() {
        return Err(EncryptError::EmptyPassphrase);
    }
    let decryptor = Decryptor::new(ciphertext)?;

    let identity = scrypt::Identity::new(SecretString::from(passphrase.to_string()));
    let mut reader = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?;
    let mut plaintext = Vec::new();
    reader.read_to_end(&mut plaintext)?;
    Ok(plaintext)
}

/// Compute a stable 12-hex-char fingerprint for a passphrase (SHA-256 prefix).
/// Empty input returns an empty string. The fingerprint is shown in the UI so
/// operators can verify the env-var-supplied passphrase matches what they set
/// during setup, without ever revealing the secret.
pub fn passphrase_fingerprint(passphrase: &str) -> String {
    if passphrase.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(passphrase.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..6]) // 12 hex chars
}
