//! HMAC-SHA256 signing for outbound webhook bodies.
//!
//! Every outbound POST carries `X-Isengard-Signature: sha256=<hex>`
//! where `<hex>` is the lowercase hex of
//! `HMAC-SHA256(secret_bytes, body_bytes)`. Receivers verify by
//! computing the same MAC and comparing in constant time.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// HMAC-SHA256 type alias.
type HmacSha256 = Hmac<Sha256>;

/// HTTP header carrying the signature.
pub const SIGNATURE_HEADER: &str = "X-Isengard-Signature";

/// Required prefix on the header value.
pub const SIGNATURE_PREFIX: &str = "sha256=";

/// Computes the `sha256=<hex>` header value for `body` signed with
/// `secret`.
///
/// Output is deterministic and lowercase. The HMAC construction
/// accepts any key length, including an empty key (used by lifecycle
/// hooks that don't carry a per-container secret).
pub fn compute_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!("{SIGNATURE_PREFIX}{}", hex::encode(tag))
}

/// Verifies a `sha256=<hex>` signature header against `body` and
/// `secret`.
///
/// Returns `true` only when the header carries the
/// [`SIGNATURE_PREFIX`], the hex decodes, and the resulting MAC
/// matches in constant time.
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
