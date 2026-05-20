#![doc = include_str!("../docs/join-token.md")]

use sha2::{Digest, Sha256};

/// Token prefix.
///
/// Makes leaked tokens greppable (same role as `sk_`, `ghp_`).
pub const PREFIX: &str = "TK";

/// Separator between the random bytes portion and the CA fingerprint.
pub const SEP: char = '.';

/// Base32 alphabet without padding: A-Z2-7. RFC 4648 unpadded.
const ALPHABET: data_encoding::Encoding = data_encoding::BASE32_NOPAD;

/// Pack a 32-byte token and a CA PEM into the operator-visible string.
///
/// The fingerprint is `sha256(ca_pem)` so the agent can independently verify
/// the CA it fetches matches what the controller minted against.
///
/// # Examples
///
/// ```
/// use isengard_core::join_token::{pack, parse};
///
/// let bytes = [0xABu8; 32];
/// let ca_pem = b"-----BEGIN CERTIFICATE-----\nFIXTURE\n-----END CERTIFICATE-----\n";
/// let packed = pack(&bytes, ca_pem);
/// assert!(packed.starts_with("TK"));
/// let parsed = parse(&packed).unwrap();
/// assert_eq!(parsed.bytes, bytes);
/// ```
pub fn pack(token_bytes: &[u8; 32], ca_pem: &[u8]) -> String {
    let fingerprint = Sha256::digest(ca_pem);
    let bytes_b32 = ALPHABET.encode(token_bytes);
    let fp_b32 = ALPHABET.encode(&fingerprint);
    format!("{PREFIX}{bytes_b32}{SEP}{fp_b32}")
}

/// Parsed parts of a join token.
///
/// The agent uses both halves; the controller only uses `bytes` for token
/// validation (the fingerprint stays opaque to it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToken {
    /// The 32-byte shared secret half.
    pub bytes: [u8; 32],
    /// `sha256(ca_pem)` the controller minted the token against.
    pub fingerprint: [u8; 32],
}

/// Errors returned by [`parse`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The token did not start with the required `TK` prefix.
    #[error("token missing required `TK` prefix")]
    MissingPrefix,
    /// The token did not contain the `.` separator between the bytes and
    /// fingerprint halves.
    #[error("token missing required `.` separator between bytes and fingerprint")]
    MissingSeparator,
    /// The bytes half failed base32 decoding.
    #[error("token bytes portion failed base32 decode: {0}")]
    BadBytesEncoding(data_encoding::DecodeError),
    /// The bytes half decoded to a slice of the wrong length (expected 32).
    #[error("token bytes portion is {0} bytes, expected 32")]
    WrongBytesLength(usize),
    /// The fingerprint half failed base32 decoding.
    #[error("token fingerprint portion failed base32 decode: {0}")]
    BadFingerprintEncoding(data_encoding::DecodeError),
    /// The fingerprint half decoded to a slice of the wrong length
    /// (expected 32).
    #[error("token fingerprint portion is {0} bytes, expected 32")]
    WrongFingerprintLength(usize),
}

/// Parse a packed join-token string into its `(bytes, fingerprint)` parts.
///
/// # Errors
///
/// Returns [`ParseError`] when the prefix, separator, base32 alphabet, or
/// either half's length is wrong.
pub fn parse(s: &str) -> Result<ParsedToken, ParseError> {
    let body = s.strip_prefix(PREFIX).ok_or(ParseError::MissingPrefix)?;
    let (bytes_part, fp_part) = body.split_once(SEP).ok_or(ParseError::MissingSeparator)?;

    let bytes_vec = ALPHABET
        .decode(bytes_part.as_bytes())
        .map_err(ParseError::BadBytesEncoding)?;
    let bytes: [u8; 32] = bytes_vec
        .as_slice()
        .try_into()
        .map_err(|_| ParseError::WrongBytesLength(bytes_vec.len()))?;

    let fp_vec = ALPHABET
        .decode(fp_part.as_bytes())
        .map_err(ParseError::BadFingerprintEncoding)?;
    let fingerprint: [u8; 32] = fp_vec
        .as_slice()
        .try_into()
        .map_err(|_| ParseError::WrongFingerprintLength(fp_vec.len()))?;

    Ok(ParsedToken { bytes, fingerprint })
}

/// Compute the SHA-256 of an arbitrary PEM blob.
///
/// Returns the raw 32-byte digest. Used on the agent side to compare
/// against [`ParsedToken::fingerprint`].
pub fn fingerprint(ca_pem: &[u8]) -> [u8; 32] {
    Sha256::digest(ca_pem).into()
}

/// Re-encode a 32-byte token back to the bare base32 string used by the
/// controller's storage hash.
///
/// Only useful inside the controller's `EnrollmentService::redeem` to keep
/// storage hash format unchanged across the format bump. Not for general
/// use; if you need a base32 encoder, use `data-encoding` directly.
pub fn encode_bytes(token_bytes: &[u8; 32]) -> String {
    ALPHABET.encode(token_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nFIXTURE\n-----END CERTIFICATE-----\n";

    #[test]
    fn pack_roundtrip_recovers_bytes_and_fingerprint() {
        let bytes = [0xABu8; 32];
        let packed = pack(&bytes, FIXTURE_PEM);
        let parsed = parse(&packed).unwrap();
        assert_eq!(parsed.bytes, bytes);
        assert_eq!(parsed.fingerprint, fingerprint(FIXTURE_PEM));
    }

    #[test]
    fn pack_uses_tk_prefix_and_dot_separator() {
        let bytes = [0xFFu8; 32];
        let packed = pack(&bytes, FIXTURE_PEM);
        assert!(packed.starts_with("TK"));
        assert_eq!(packed.matches('.').count(), 1);
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        let err = parse("AAAAAAAA.BBBBBBBB").unwrap_err();
        assert!(matches!(err, ParseError::MissingPrefix));
    }

    #[test]
    fn parse_rejects_missing_separator() {
        let err = parse("TKabcdef").unwrap_err();
        assert!(matches!(err, ParseError::MissingSeparator));
    }

    #[test]
    fn parse_rejects_bad_base32() {
        let err = parse("TK!!!.AAA").unwrap_err();
        assert!(matches!(err, ParseError::BadBytesEncoding(_)));
    }

    #[test]
    fn parse_rejects_wrong_length_bytes() {
        let short = ALPHABET.encode(&[0u8; 16]);
        let fp = ALPHABET.encode(&[0u8; 32]);
        let err = parse(&format!("TK{short}.{fp}")).unwrap_err();
        assert!(matches!(err, ParseError::WrongBytesLength(16)));
    }
}
