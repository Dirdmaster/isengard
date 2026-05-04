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
/// Other host fields (arch, docker_version, fingerprint, fleet) default to
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

    /// Redeem a token: validate → enroll host → sign leaf → persist cert →
    /// consume token. See module-level docs for the failure / race semantics.
    pub async fn redeem(&self, token: &str, host_info: HostInfo) -> Result<EnrollResponse> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        let _record = self
            .inventory
            .find_active_token(&hash)
            .await
            .context("token lookup")?
            .ok_or_else(|| anyhow!("enrollment token unknown, expired, or already consumed"))?;

        // Storage assigns the HostId. We pass the agent-supplied descriptor and
        // sane placeholders for the fields agents don't carry at bootstrap;
        // these get refined by subsequent agent reports.
        let host_id = self
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: String::new(),
                hostname: host_info.hostname.clone(),
                os: host_info.os.clone(),
                arch: String::new(),
                agent_version: host_info.version.clone(),
                docker_version: String::new(),
                fleet: "default".to_string(),
            })
            .await
            .context("enroll host")?;

        let leaf = self
            .ca
            .sign_agent_leaf(host_id, &host_info.hostname, Duration::days(LEAF_TTL_DAYS))
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
}
