//! Age passphrase encryption.
//!
//! v1 ships passphrase-only (PBKDF2 via `age::scrypt`). X25519
//! recipients are deferred until SaaS escrow.
//!
//! The passphrase reaches the controller via the
//! `ISENGARD_BACKUP_PASSPHRASE` env var; the DB persists only a
//! 12-hex-char SHA-256 fingerprint via [`passphrase_fingerprint`] so
//! the UI can confirm the running controller's passphrase matches
//! what the operator stored during setup.

use std::io::{Read, Write};

use age::secrecy::SecretString;
use age::{Decryptor, Encryptor, scrypt};
use sha2::{Digest, Sha256};

/// Errors raised by the encryption module.
#[derive(Debug, thiserror::Error)]
pub enum EncryptError {
    /// age library encryption failure.
    #[error("age encrypt: {0}")]
    AgeEncrypt(#[from] age::EncryptError),

    /// age library decryption failure.
    #[error("age decrypt: {0}")]
    AgeDecrypt(#[from] age::DecryptError),

    /// IO failure while writing to or reading from the in-memory
    /// buffer.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Empty passphrase rejected up front.
    #[error("passphrase is empty")]
    EmptyPassphrase,
}

/// Encrypts `plaintext` with `passphrase`.
///
/// Output is the binary age format (no armor); store on the
/// destination as `<name>.db.age`.
///
/// # Errors
///
/// Returns [`EncryptError::EmptyPassphrase`] when `passphrase` is
/// empty. Bubbles any age or IO error.
pub fn encrypt_with_passphrase(
    plaintext: &[u8],
    passphrase: &str,
) -> Result<Vec<u8>, EncryptError> {
    if passphrase.is_empty() {
        return Err(EncryptError::EmptyPassphrase);
    }
    let recipient = scrypt::Recipient::new(SecretString::from(passphrase.to_string()));
    let encryptor = Encryptor::with_user_passphrase(SecretString::from(passphrase.to_string()));
    // Future-proof: keeps a reference to the recipient binding alive
    // in case the age API ever exposes passphrase encryption purely
    // through `Recipient`. Discarded today.
    let _ = recipient;

    let mut out = Vec::new();
    let mut writer = encryptor.wrap_output(&mut out)?;
    writer.write_all(plaintext)?;
    writer.finish()?;
    Ok(out)
}

/// Decrypts an age blob with `passphrase`.
///
/// Used by the restore flow and by integration tests. The work
/// factor on decryption is inherited from the on-disk header.
///
/// # Errors
///
/// Returns [`EncryptError::EmptyPassphrase`] when `passphrase` is
/// empty. Bubbles any age or IO error (a wrong passphrase surfaces
/// as an age decrypt error).
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

/// Returns a stable 12-hex-char fingerprint for `passphrase` (the
/// first 6 bytes of its SHA-256).
///
/// Empty input returns an empty string. The fingerprint is shown in
/// the UI so operators can verify the env-var-supplied passphrase
/// matches what they set during setup, without ever revealing the
/// secret.
pub fn passphrase_fingerprint(passphrase: &str) -> String {
    if passphrase.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(passphrase.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..6])
}
