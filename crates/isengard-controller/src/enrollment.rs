//! Enrollment service: mint short-lived bootstrap tokens, then redeem them
//! into a per-agent leaf cert + bundle.
//!
//! See spec docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md.
//!
//! Mint flow: 32 random bytes from OsRng → unpadded uppercase RFC4648 base32 →
//! plaintext returned to the operator (shown once). Only the SHA-256 hash is
//! persisted. Tokens carry a TTL; expired tokens are filtered out by storage.
//!
//! Redeem flow:
//!   1. Look up active token by hash (storage filters expired/consumed).
//!   2. Enroll the host (storage assigns a fresh HostId).
//!   3. Sign a leaf cert via [`Authority::sign_agent_leaf`].
//!   4. Persist the cert.
//!   5. Mark the token consumed *last*. If anything before this fails the
//!      token stays usable. Race window: two concurrent redeems may both
//!      observe the token as active and both sign leafs; only one will win
//!      the conditional UPDATE in `consume_enrollment_token`. The loser's
//!      cert becomes dangling — acceptable trade-off for an internal CA.
//!
//! On unknown / expired / already-consumed token in step 1 the error message
//! contains the literal word "token" so callers can match on it (Task 6 maps
//! this to a gRPC status; the unit tests assert the substring).

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

/// Per-agent leaf cert validity. Renewed well before expiry by the controller's
/// renewal task (Task 5). 30 days matches the spec's "renew at <7d remaining"
/// policy with a comfortable safety margin.
const LEAF_TTL_DAYS: i64 = 30;

/// Heartbeat cadence the agent should adopt after enrollment. Returned in the
/// enroll bundle so the controller is the single source of truth.
const HEARTBEAT_INTERVAL_SECS: u32 = 10;

/// Minimal host descriptor the agent presents at redeem time. The agent supplies
/// what it knows locally; storage fills in everything else (HostId, enrolled_at).
/// Other host fields (arch, docker_version, fingerprint) default to
/// placeholders here and are refined later via heartbeat / re-enrollment — the
/// enrollment exchange is intentionally minimal so a freshly-installed agent
/// can come online without first running a full system probe.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
}

/// Bundle returned to the agent on successful redeem. The agent persists the
/// cert + key locally (mTLS material), pins `ca_root_pem` as its trust anchor
/// for the controller, and uses `heartbeat_interval_secs` to drive its
/// heartbeat loop.
#[derive(Debug, Clone)]
pub struct EnrollResponse {
    pub host_id: HostId,
    pub agent_cert_pem: String,
    pub agent_key_pem: String,
    pub ca_root_pem: String,
    pub heartbeat_interval_secs: u32,
}

/// Bundle returned on a successful renew. Same shape as the cert half of
/// [`EnrollResponse`], minus the bootstrap-only fields (host_id and CA root
/// are already known to the caller).
#[derive(Debug, Clone)]
pub struct RenewedCert {
    pub agent_cert_pem: String,
    pub agent_key_pem: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

pub struct EnrollmentService {
    inventory: Arc<Inventory>,
    ca: Arc<Authority>,
}

impl EnrollmentService {
    pub fn new(inventory: Arc<Inventory>, ca: Arc<Authority>) -> Self {
        Self { inventory, ca }
    }

    /// Mint a fresh enrollment token. Returns the *plaintext* token (shown
    /// once to the operator); only the SHA-256 hash is persisted.
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

    /// Redeem a token: validate, sign leaf, enroll host, persist cert,
    /// consume token. See module-level docs for the failure / race semantics.
    ///
    /// Step order: we mint the leaf cert before inserting the host row so we
    /// can use the leaf's SHA-256 fingerprint as the `hosts.fingerprint`
    /// column (which carries a UNIQUE constraint). Pre-fix the controller
    /// passed an empty string, so the second enrollment on any controller
    /// would collide on UNIQUE and fail with `enroll host`. The leaf cert is
    /// signed under the controller's CA with a 16-byte random serial, so its
    /// SHA-256 is unique by construction per enrollment.
    ///
    /// The CN-in-the-cert is the `HostId`, but the host row also needs that
    /// id at insert time. We work around the chicken-and-egg by pre-minting
    /// a `HostId` and passing it to both `sign_agent_leaf` and `enroll_host`.
    pub async fn redeem(&self, token: &str, host_info: HostInfo) -> Result<EnrollResponse> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
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

    /// Sign a fresh leaf cert for an already-enrolled host. The previous cert
    /// row stays in storage with `revoked_at = NULL`: the agent_certs table is
    /// an append-only audit trail, and the old cert remains valid until either
    /// it expires naturally or it's revoked explicitly. The agent simply
    /// switches to presenting the new key/cert pair.
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

/// Compute the SHA-256 fingerprint of a PEM-encoded certificate's DER bytes
/// and return it as a lowercase hex string. Used as the stable, unique
/// `hosts.fingerprint` for enrolled agents: the leaf cert is freshly minted
/// per redeem with a 16-byte random serial, so its hash is collision-free
/// in practice.
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
