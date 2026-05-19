//! Age passphrase encryption tests.

use isengard_plugin_backup::encrypt::{
    EncryptError, decrypt_with_passphrase, encrypt_with_passphrase, passphrase_fingerprint,
};

#[test]
fn round_trip_passphrase() {
    let plaintext = b"Hello, Isengard backup!".to_vec();
    let pass = "correct horse battery staple";

    let cipher = encrypt_with_passphrase(&plaintext, pass).unwrap();
    assert_ne!(cipher, plaintext, "ciphertext must differ from plaintext");

    let recovered = decrypt_with_passphrase(&cipher, pass).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trip_handles_binary_payload() {
    // Simulate a SQLite header + random binary middle.
    let mut payload = b"SQLite format 3\0".to_vec();
    payload.extend_from_slice(&(0..200u8).collect::<Vec<_>>());

    let pass = "another-pass-1234";
    let cipher = encrypt_with_passphrase(&payload, pass).unwrap();
    let recovered = decrypt_with_passphrase(&cipher, pass).unwrap();
    assert_eq!(recovered, payload);
}

#[test]
fn decrypt_with_wrong_passphrase_fails() {
    let plaintext = b"secret stuff".to_vec();
    let cipher = encrypt_with_passphrase(&plaintext, "right-pass").unwrap();
    let err = decrypt_with_passphrase(&cipher, "wrong-pass").unwrap_err();
    assert!(matches!(err, EncryptError::AgeDecrypt(_)));
}

#[test]
fn empty_passphrase_is_rejected_on_encrypt() {
    let err = encrypt_with_passphrase(b"data", "").unwrap_err();
    assert!(matches!(err, EncryptError::EmptyPassphrase));
}

#[test]
fn empty_passphrase_is_rejected_on_decrypt() {
    let cipher = encrypt_with_passphrase(b"data", "p").unwrap();
    let err = decrypt_with_passphrase(&cipher, "").unwrap_err();
    assert!(matches!(err, EncryptError::EmptyPassphrase));
}

#[test]
fn fingerprint_is_deterministic_and_short() {
    let fp_a = passphrase_fingerprint("hello");
    let fp_b = passphrase_fingerprint("hello");
    assert_eq!(fp_a, fp_b);
    assert_eq!(fp_a.len(), 12);
    assert!(fp_a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn fingerprint_changes_with_passphrase() {
    assert_ne!(
        passphrase_fingerprint("hello"),
        passphrase_fingerprint("hello!")
    );
}

#[test]
fn fingerprint_of_empty_is_empty() {
    assert_eq!(passphrase_fingerprint(""), "");
}
