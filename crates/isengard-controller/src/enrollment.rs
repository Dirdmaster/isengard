//! Enrollment service: mint short-lived bootstrap tokens and redeem
//! them for a per-agent leaf cert and bundle.
//!
//! Enrollment token issuance, verification, and agent identity state.
//!
//! # Mint flow
//!
//! 32 random bytes from `OsRng`, base32-encoded (RFC 4648, unpadded,
//! uppercase). The plaintext is returned to the operator once; only the
//! SHA-256 hash is persisted. Tokens carry a TTL; storage filters out
//! expired tokens on lookup.
//!
//! # Redeem flow
//!
//! 1. Look up an active token by hash (storage filters expired and
//!    already-consumed rows).
//! 2. Pre-mint the [`HostId`] so the leaf CN can carry it.
//! 3. Sign a leaf cert via [`Authority::sign_agent_leaf`].
//! 4. Insert the host row, using the leaf's SHA-256 as the unique
//!    `hosts.fingerprint`.
//! 5. Persist the cert row.
//! 6. Mark the token consumed last. Two concurrent redeems may both
//!    observe the token as active and both sign leaves; only one wins
//!    the conditional UPDATE in `consume_enrollment_token`. The loser's
//!    cert becomes dangling, an acceptable trade-off for an internal
//!    CA.
//!
//! On unknown / expired / already-consumed token, the error message
//! contains the literal word "token" so the gRPC handler can match on
//! it.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use base32::Alphabet;
use chrono::Duration;
use rand::RngCore;
use sha2::{Digest, Sha256};

use isengard_storage::Inventory;
use isengard_storage::agent_cert::AgentCert;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::{EnrollHost, HostId};

use crate::ca::Authority;

/// Per-agent leaf cert validity, in days.
///
/// The controller's renewal task signs a fresh leaf well before expiry.
/// 30 days matches the spec's "renew at <7d remaining" policy with a
/// comfortable safety margin.
const LEAF_TTL_DAYS: i64 = 30;

/// Heartbeat cadence the agent adopts after enrollment, in seconds.
///
/// Returned in the enroll bundle so the controller stays the single
/// source of truth for the cadence.
const HEARTBEAT_INTERVAL_SECS: u32 = 10;

/// Minimal host descriptor the agent presents at redeem time.
///
/// The agent supplies what it knows locally; storage fills in
/// everything else (`HostId`, `enrolled_at`). Other host fields (arch,
/// docker_version, fingerprint) default to placeholders here and are
/// refined later via heartbeat or re-enrollment. The exchange is
/// intentionally minimal so a freshly-installed agent can come online
/// without first running a full system probe.
#[derive(Debug, Clone)]
pub struct HostInfo {
    /// Agent-reported hostname. Goes into the leaf's DNS SAN.
    pub hostname: String,
    /// Agent-reported OS string (e.g. `linux`, `darwin`).
    pub os: String,
    /// Agent binary version.
    pub version: String,
}

/// Bundle returned to the agent on a successful redeem.
///
/// The agent persists `agent_cert_pem` and `agent_key_pem` locally as
/// its mTLS material, pins `ca_root_pem` as its trust anchor for the
/// controller, and uses `heartbeat_interval_secs` to drive its
/// heartbeat loop.
#[derive(Debug, Clone)]
pub struct EnrollResponse {
    /// Stable host identifier assigned by the controller.
    pub host_id: HostId,
    /// PEM-encoded leaf cert (signed by the controller CA).
    pub agent_cert_pem: String,
    /// PEM-encoded leaf private key.
    pub agent_key_pem: String,
    /// PEM-encoded CA root, pinned as the controller's trust anchor.
    pub ca_root_pem: String,
    /// Heartbeat cadence the agent should adopt.
    pub heartbeat_interval_secs: u32,
}

/// Bundle returned by [`EnrollmentService::renew`].
///
/// Same shape as the cert half of [`EnrollResponse`] minus the
/// bootstrap-only fields (`host_id` and the CA root are already known
/// to the caller).
#[derive(Debug, Clone)]
pub struct RenewedCert {
    /// PEM-encoded fresh leaf cert.
    pub agent_cert_pem: String,
    /// PEM-encoded fresh leaf private key.
    pub agent_key_pem: String,
    /// Wall-clock expiry of the new cert.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Owns the mint and redeem flows.
///
/// Cheap to share via `Arc`. Holds the shared inventory and CA handles
/// so it can write tokens, host rows, and cert rows in one place.
pub struct EnrollmentService {
    /// Shared inventory handle for token, host, and cert rows.
    inventory: Arc<Inventory>,
    /// Shared CA handle for signing leaves.
    ca: Arc<Authority>,
}

impl EnrollmentService {
    /// Builds a service over the shared inventory and CA.
    pub fn new(inventory: Arc<Inventory>, ca: Arc<Authority>) -> Self {
        Self { inventory, ca }
    }

    /// Mints a fresh enrollment token.
    ///
    /// Returns the plaintext token (shown once to the operator); only
    /// the SHA-256 hash is persisted.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the inventory write fails.
    pub async fn mint(&self, role: TokenRole, ttl: Duration) -> Result<String> {
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token = base32::encode(Alphabet::Rfc4648 { padding: false }, &raw);
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        let expires_at = chrono::Utc::now() + ttl;

        self.inventory
            .insert_enrollment_token(hash, role, expires_at)
            .await
            .context("persist enrollment token")?;
        Ok(token)
    }

    /// Redeems a token: validate, sign leaf, enroll host, persist cert,
    /// consume token.
    ///
    /// See the module-level docs for the full failure and race
    /// semantics. The leaf cert is minted before the host row is
    /// inserted so the leaf's SHA-256 fingerprint can fill the
    /// `hosts.fingerprint` column (which carries a UNIQUE constraint).
    /// Pre-fix the controller passed an empty string, so the second
    /// enrollment on any controller would collide on UNIQUE and fail
    /// with `enroll host`.
    ///
    /// The leaf CN is the `HostId`, but the host row also needs that
    /// id at insert time. The chicken-and-egg is resolved by
    /// pre-minting a [`HostId`] and passing it to both
    /// [`Authority::sign_agent_leaf`] and `enroll_host_with_id`.
    ///
    /// # Errors
    ///
    /// Returns `Err` on a malformed packed token, on unknown / expired
    /// / consumed tokens, on signing failures, and on storage failures.
    pub async fn redeem(&self, token: &str, host_info: HostInfo) -> Result<EnrollResponse> {
        // Incoming token MUST be packed (TK<bytes>.<fingerprint>).
        // Storage keys on `sha256(bare_b32_string)`, so the packed token
        // gets decomposed back to its bare-bytes base32 form for the
        // lookup; the fingerprint half stays opaque to the controller.
        // Legacy bare-base32 tokens are rejected with a clear error.
        let parsed = isengard_core::join_token::parse(token)
            .map_err(|e| anyhow!("invalid enrollment token: {e}"))?;
        let lookup_token = isengard_core::join_token::encode_bytes(&parsed.bytes);
        let hash = Sha256::digest(lookup_token.as_bytes()).to_vec();
        let _record = self
            .inventory
            .find_active_token(&hash)
            .await
            .context("token lookup")?
            .ok_or_else(|| anyhow!("enrollment token unknown, expired, or already consumed"))?;

        // Pre-mint the HostId so we can sign the leaf cert (CN = host_id)
        // before inserting the hosts row (which needs the fingerprint).
        let host_id = HostId::new();

        let leaf = self
            .ca
            .sign_agent_leaf(host_id, &host_info.hostname, Duration::days(LEAF_TTL_DAYS))
            .context("sign leaf cert")?;

        // Derive the agent's stable fingerprint from the SHA-256 of the leaf
        // cert DER bytes. Hex-encoded lowercase to match the rest of the
        // codebase's hex conventions (e.g. `hex::encode` in the updater /
        // dashboard signing paths).
        let fingerprint = cert_fingerprint(&leaf.cert_pem).context("compute cert fingerprint")?;

        // Storage uses the pre-minted HostId. We pass the agent-supplied
        // descriptor and sane placeholders for the fields agents don't carry
        // at bootstrap; these get refined by subsequent agent reports.
        self.inventory
            .enroll_host_with_id(
                host_id,
                EnrollHost {
                    fingerprint,
                    hostname: host_info.hostname.clone(),
                    os: host_info.os.clone(),
                    arch: String::new(),
                    agent_version: host_info.version.clone(),
                    docker_version: String::new(),
                },
            )
            .await
            .context("enroll host")?;

        self.inventory
            .insert_agent_cert(AgentCert {
                serial: leaf.serial.clone(),
                host_id,
                cert_pem: leaf.cert_pem.clone(),
                issued_at: chrono::Utc::now(),
                expires_at: leaf.expires_at,
                revoked_at: None,
                revoke_reason: None,
            })
            .await
            .context("persist cert")?;

        // Mark the token consumed last. If two redeems race, both may pass
        // find_active_token but only one wins the conditional UPDATE here;
        // the loser's already-signed cert becomes dangling (acceptable).
        self.inventory
            .consume_enrollment_token(&hash, host_id)
            .await
            .context("consume enrollment token (race?)")?;

        Ok(EnrollResponse {
            host_id,
            agent_cert_pem: leaf.cert_pem,
            agent_key_pem: leaf.key_pem,
            ca_root_pem: self.ca.root_cert_pem().to_string(),
            heartbeat_interval_secs: HEARTBEAT_INTERVAL_SECS,
        })
    }

    /// Signs a fresh leaf cert for an already-enrolled host.
    ///
    /// The previous cert row stays in storage with `revoked_at = NULL`:
    /// the `agent_certs` table is an append-only audit trail, and the
    /// old cert remains valid until either it expires naturally or it's
    /// revoked explicitly. The agent switches to presenting the new
    /// key/cert pair.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the host is not enrolled, when signing
    /// fails, or when the cert insert fails.
    pub async fn renew(&self, host_id: HostId) -> Result<RenewedCert> {
        let host = self
            .inventory
            .get_host(host_id)
            .await
            .context("host lookup")?
            .ok_or_else(|| anyhow!("unknown host"))?;

        let leaf = self
            .ca
            .sign_agent_leaf(host_id, &host.hostname, Duration::days(LEAF_TTL_DAYS))
            .context("sign leaf cert")?;

        self.inventory
            .insert_agent_cert(AgentCert {
                serial: leaf.serial.clone(),
                host_id,
                cert_pem: leaf.cert_pem.clone(),
                issued_at: chrono::Utc::now(),
                expires_at: leaf.expires_at,
                revoked_at: None,
                revoke_reason: None,
            })
            .await
            .context("persist renewed cert")?;

        Ok(RenewedCert {
            agent_cert_pem: leaf.cert_pem,
            agent_key_pem: leaf.key_pem,
            expires_at: leaf.expires_at,
        })
    }
}

/// Computes the SHA-256 of a PEM-encoded certificate's DER bytes as
/// lowercase hex.
///
/// Used as the unique `hosts.fingerprint` for enrolled agents. The leaf
/// cert is freshly minted per redeem with a 16-byte random serial, so
/// its hash is collision-free in practice.
///
/// # Errors
///
/// Returns `Err` when the input is not a valid PEM certificate.
fn cert_fingerprint(cert_pem: &str) -> Result<String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow!("parse leaf cert PEM: {e}"))?;
    Ok(hex::encode(Sha256::digest(&pem.contents)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_fingerprint_is_64_hex_chars_and_deterministic() {
        // Two arbitrary self-signed certs from rcgen so we exercise the
        // PEM-decode path with realistic input. Using the same PEM twice
        // must produce the same digest; different PEMs must differ.
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let params = rcgen::CertificateParams::new(vec!["fp-test".into()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();

        let fp_a = cert_fingerprint(&pem).unwrap();
        let fp_b = cert_fingerprint(&pem).unwrap();
        assert_eq!(fp_a, fp_b, "fingerprint must be deterministic for same PEM");
        assert_eq!(fp_a.len(), 64, "sha256 hex is 64 chars");
        assert!(
            fp_a.chars().all(|c| c.is_ascii_hexdigit()),
            "fp must be lowercase hex: {fp_a}"
        );

        let key2 = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let params2 = rcgen::CertificateParams::new(vec!["other".into()]).unwrap();
        let cert2 = params2.self_signed(&key2).unwrap();
        let fp_other = cert_fingerprint(&cert2.pem()).unwrap();
        assert_ne!(fp_a, fp_other, "different certs must produce different fps");
    }

    #[test]
    fn cert_fingerprint_rejects_garbage() {
        let err = cert_fingerprint("not a pem at all").unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("parse"));
    }
}
