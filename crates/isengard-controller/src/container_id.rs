//! Derive the operator-visible container id from
//! `sha256(host_id || "|" || runtime_container_id)[..8]` (16 hex chars).
//!
//! 64 bits of entropy. Collision-resistant per cluster. Stable: the same
//! host + runtime id always hashes to the same id, so reconnects and
//! agent restarts don't churn the row. Globally unique without
//! depending on host-local id spaces (so `isd container inspect <id>`
//! does not need a `--host` flag).

use sha2::{Digest, Sha256};

/// Compute the 16-char lowercase hex digest of
/// `sha256(host_id_display || "|" || runtime_id)[..8]`.
///
/// `host_id` is the [`isengard_storage::host::HostId`] in its display
/// form (Crockford ULID). Passing the display string (rather than the
/// raw 16 bytes) keeps the derivation reproducible from anywhere the
/// operator can read the ULID: dashboard URL, CLI flag, JSON dump.
pub fn derive_container_id(host_id: &str, runtime_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(host_id.as_bytes());
    hasher.update(b"|");
    hasher.update(runtime_id.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_stable_for_same_inputs() {
        let a = derive_container_id("01HXABCDEFGHJKMNPQRSTVWXYZ", "rt-abc-123");
        let b = derive_container_id("01HXABCDEFGHJKMNPQRSTVWXYZ", "rt-abc-123");
        assert_eq!(a, b);
    }

    #[test]
    fn derive_emits_16_lowercase_hex_chars() {
        let id = derive_container_id("01HXABCDEFGHJKMNPQRSTVWXYZ", "rt-abc-123");
        assert_eq!(id.len(), 16);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "id should be lowercase hex: {id}"
        );
    }

    #[test]
    fn derive_differs_for_different_runtime_ids() {
        let a = derive_container_id("01HXABCDEFGHJKMNPQRSTVWXYZ", "rt-abc-123");
        let b = derive_container_id("01HXABCDEFGHJKMNPQRSTVWXYZ", "rt-def-456");
        assert_ne!(a, b);
    }

    #[test]
    fn derive_differs_for_different_hosts() {
        let host_a = "01HXAAAAAAAAAAAAAAAAAAAAAA";
        let host_b = "01HXBBBBBBBBBBBBBBBBBBBBBB";
        let id_a = derive_container_id(host_a, "rt-shared");
        let id_b = derive_container_id(host_b, "rt-shared");
        assert_ne!(
            id_a, id_b,
            "different hosts with same runtime id must produce different operator ids",
        );
    }

    /// The `|` separator prevents trivial collisions between
    /// `(host=A, rt=BC)` and `(host=AB, rt=C)`. Without it both inputs
    /// would hash the same string.
    #[test]
    fn derive_uses_separator_to_avoid_concat_collision() {
        let with_split_a = derive_container_id("A", "BC");
        let with_split_b = derive_container_id("AB", "C");
        assert_ne!(with_split_a, with_split_b);
    }
}
