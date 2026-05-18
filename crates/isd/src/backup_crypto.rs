//! Passphrase-based encryption for backups using `age`.
//!
//! Streams plaintext through age's `Encryptor::with_user_passphrase` and
//! ciphertext through `Decryptor::decrypt(...)`. Works on any `Read` /
//! `Write` pair; backup pipelines bridge `tokio::io` to `std::io` via
//! `tokio_util::io::SyncIoBridge` in a `spawn_blocking` task.
//!
//! The format is a real age file (rage / age CLI can decrypt it given the
//! passphrase), which keeps "lose isd, keep the bytes" recovery on the table.

#![allow(dead_code)]

use std::io::{Read, Write};

use age::secrecy::SecretString;
use anyhow::{Context, Result, anyhow};

/// Encrypt `reader` into `writer` using the given passphrase. Streams,
/// no buffering of the full plaintext. Returns total plaintext bytes
/// processed.
pub fn encrypt_stream(passphrase: &str, mut reader: impl Read, writer: impl Write) -> Result<u64> {
    let encryptor =
        age::Encryptor::with_user_passphrase(SecretString::from(passphrase.to_string()));
    let mut output = encryptor
        .wrap_output(writer)
        .context("starting age encryption stream")?;
    let bytes = std::io::copy(&mut reader, &mut output).context("streaming plaintext to age")?;
    output.finish().context("finalizing age ciphertext")?;
    Ok(bytes)
}

/// Decrypt `reader` into `writer`. Returns bytes written. Wrong passphrase
/// surfaces as a clear error.
pub fn decrypt_stream(passphrase: &str, reader: impl Read, mut writer: impl Write) -> Result<u64> {
    let decryptor = age::Decryptor::new(reader).context("reading age header")?;
    let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_string()));
    let mut output = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| anyhow!("decrypting (wrong passphrase?): {e}"))?;
    let bytes = std::io::copy(&mut output, &mut writer).context("streaming plaintext from age")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let plaintext = b"hello world".repeat(1000);
        let mut ciphertext = Vec::new();
        encrypt_stream("hunter2", &plaintext[..], &mut ciphertext).unwrap();
        assert_ne!(plaintext, ciphertext);
        assert!(!ciphertext.is_empty());

        let mut recovered = Vec::new();
        decrypt_stream("hunter2", &ciphertext[..], &mut recovered).unwrap();
        assert_eq!(plaintext, recovered);
    }

    #[test]
    fn wrong_passphrase_fails_with_clear_error() {
        let plaintext = b"secret".to_vec();
        let mut ciphertext = Vec::new();
        encrypt_stream("right", &plaintext[..], &mut ciphertext).unwrap();

        let mut recovered = Vec::new();
        let err = decrypt_stream("wrong", &ciphertext[..], &mut recovered).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("wrong passphrase") || msg.contains("decrypt") || msg.contains("scrypt"),
            "expected wrong-passphrase signal, got: {msg}"
        );
    }

    #[test]
    fn truncated_ciphertext_errors() {
        let plaintext = b"hello world".repeat(200);
        let mut ciphertext = Vec::new();
        encrypt_stream("hunter2", &plaintext[..], &mut ciphertext).unwrap();
        ciphertext.truncate(ciphertext.len() / 2);

        let mut recovered = Vec::new();
        let result = decrypt_stream("hunter2", &ciphertext[..], &mut recovered);
        assert!(result.is_err(), "truncated ciphertext must error");
    }
}
