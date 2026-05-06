//! HMAC-SHA256 signing for outbound webhook bodies.
//!
//! Outgoing requests carry `X-Isengard-Signature: sha256=<hex>` where `<hex>`
//! is the lowercase hex of `HMAC-SHA256(secret_bytes, body_bytes)`. Receivers
//! verify by computing the same and comparing in constant time.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Header name used for the signature.
pub const SIGNATURE_HEADER: &str = "X-Isengard-Signature";

/// Header value prefix carried before the hex digest.
pub const SIGNATURE_PREFIX: &str = "sha256=";

/// Compute the `sha256=<hex>` header value for `body` with `secret`.
pub fn compute_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!("{SIGNATURE_PREFIX}{}", hex::encode(tag))
}

/// Verify a `sha256=<hex>` signature header in constant time. Returns true
/// iff the header parses and the digest matches.
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex_part) = header.strip_prefix(SIGNATURE_PREFIX) else {
        return false;
    };
    let Ok(provided) = hex::decode(hex_part) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_deterministic_lowercase_hex() {
        let sig = compute_signature(b"key", b"body");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig, sig.to_lowercase());
        // Same inputs -> same digest.
        assert_eq!(sig, compute_signature(b"key", b"body"));
    }

    #[test]
    fn signature_changes_when_body_changes() {
        let a = compute_signature(b"secret", b"hello");
        let b = compute_signature(b"secret", b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn verify_round_trip() {
        let body = b"{\"kind\":\"update.success\"}";
        let sig = compute_signature(b"sekret", body);
        assert!(verify_signature(b"sekret", body, &sig));
        assert!(!verify_signature(b"WRONG", body, &sig));
        assert!(!verify_signature(b"sekret", b"tampered", &sig));
    }

    #[test]
    fn verify_rejects_malformed_header() {
        assert!(!verify_signature(b"k", b"body", "no-prefix"));
        assert!(!verify_signature(b"k", b"body", "sha256=not-hex!!"));
    }
}
