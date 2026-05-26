//! Internal Certificate Authority that issues every Isengard mTLS leaf.
//!
//! Controller certificate authority for enrollment and agent identity.
//!
//! The CA is one self-signed ECDSA P-256 root persisted in the
//! controller's SQLite (`ca` table, single row).
//! [`Authority::load_or_init`] loads the persisted root or generates and
//! persists a new one on first boot. Two leaf-mint entry points sit on
//! top:
//!
//! - [`Authority::sign_agent_leaf`] mints agent leaves with the
//!   `ClientAuth` EKU only. The enrollment service calls this.
//! - [`Authority::sign_server_leaf_with_sans`] mints the controller's
//!   own server cert with `ClientAuth + ServerAuth`. The boot path
//!   calls this once during gRPC server setup.
//!
//! The EKU split is the Bl-1 fix: pre-fix an agent could enroll with
//! hostname `controller.local` and present its leaf as a server cert
//! that other peers trust as the controller.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SerialNumber,
};

use isengard_storage::Inventory;
use isengard_storage::ca::CaRow;
use isengard_storage::host::HostId;

/// Root CA validity. Ten years, long enough that root rotation is not
/// something the operator has to think about for the lifetime of an
/// Isengard install.
const CA_TTL_DAYS: i64 = 365 * 10;

/// In-memory handle to the controller's internal CA.
///
/// Holds the parsed root cert and signing key plus the PEM blobs (kept
/// around so callers asking for the trust anchor don't have to
/// re-serialize on every request).
pub struct Authority {
    /// Root cert in PEM form. Served from `GetCaPem` and pinned by
    /// agents.
    cert_pem: String,
    /// Root private key in PEM form. Used by the rcgen `signed_by`
    /// path; never sent over the wire.
    key_pem: String,
    /// Parsed root certificate.
    cert: Certificate,
    /// Parsed root signing key.
    key_pair: KeyPair,
}

/// Output of every leaf mint call.
///
/// The serial is the 16-byte random value the controller assigned,
/// mirrored into `agent_certs.serial` so revocation lookups match.
pub struct IssuedLeaf {
    /// PEM-encoded leaf certificate.
    pub cert_pem: String,
    /// PEM-encoded leaf private key.
    pub key_pem: String,
    /// 16-byte random serial. Matches the serial embedded in
    /// `cert_pem`.
    pub serial: Vec<u8>,
    /// Wall-clock expiry. Mirrors `not_after`.
    pub expires_at: DateTime<Utc>,
}

impl Authority {
    /// Loads the CA from inventory, or generates and persists a new
    /// one when the `ca` table is empty.
    ///
    /// Idempotent: calling twice on the same inventory returns an
    /// `Authority` backed by the same persisted PEM blobs.
    ///
    /// # Errors
    ///
    /// Returns `Err` on inventory read failures, on key/cert parse
    /// failures (corrupted row), or on `rcgen` self-sign failures.
    pub async fn load_or_init(inventory: &Inventory) -> Result<Self> {
        if let Some(row) = inventory.get_ca().await.context("ca lookup")? {
            let key_pair = KeyPair::from_pem(&row.root_key_pem).context("parse ca key")?;
            let params =
                CertificateParams::from_ca_cert_pem(&row.root_cert_pem).context("parse ca cert")?;
            let cert = params.self_signed(&key_pair).context("rebuild ca cert")?;
            return Ok(Authority {
                cert_pem: row.root_cert_pem,
                key_pem: row.root_key_pem,
                cert,
                key_pair,
            });
        }

        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generate ca key")?;
        let mut params = CertificateParams::new(vec![]).context("ca params")?;
        params
            .distinguished_name
            .push(DnType::CommonName, "Isengard Internal CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = Utc::now();
        params.not_before = chrono_to_offset(now);
        params.not_after = chrono_to_offset(now + Duration::days(CA_TTL_DAYS));

        let cert = params.self_signed(&key_pair).context("self-sign ca")?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        inventory
            .set_ca(CaRow {
                root_cert_pem: cert_pem.clone(),
                root_key_pem: key_pem.clone(),
            })
            .await
            .context("persist ca")?;

        Ok(Authority {
            cert_pem,
            key_pem,
            cert,
            key_pair,
        })
    }

    /// Returns the PEM-encoded root certificate.
    ///
    /// This is what agents pin as their trust anchor. Also returned in
    /// the `Enroll` response and from the public `GetCaPem` RPC.
    pub fn root_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Returns the PEM-encoded root private key.
    ///
    /// Used by the controller's own server-cert generation path; never
    /// sent over the wire.
    pub fn root_key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Maximum length of an agent-supplied hostname.
    ///
    /// DNS labels are bounded to 253 characters total; the same cap
    /// applies here as a sanity guard. Anything longer is rejected up
    /// front so an absurd input never reaches rcgen as a 4-MB SAN.
    const MAX_HOSTNAME_LEN: usize = 253;

    /// Mints a per-agent leaf cert.
    ///
    /// CN is set to `host_id.to_string()`, the single DNS SAN is
    /// `hostname`, the EKU is `ClientAuth` only, the serial is 16 random
    /// bytes. Pre-Bl-1 the EKU also included `ServerAuth`, which
    /// combined with agent-controlled hostnames let an agent get a cert
    /// other peers would trust as the controller (e.g. by enrolling
    /// with `hostname: "controller.local"`). Restricting agent leaves
    /// to `ClientAuth` closes that path. Controller-side server certs
    /// go through [`Authority::sign_server_leaf_with_sans`] instead.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `hostname` exceeds
    /// `Authority::MAX_HOSTNAME_LEN` or rcgen fails to sign.
    pub fn sign_agent_leaf(
        &self,
        host_id: HostId,
        hostname: &str,
        ttl: Duration,
    ) -> Result<IssuedLeaf> {
        self.sign_leaf_inner(
            host_id,
            hostname,
            &[],
            ttl,
            &[ExtendedKeyUsagePurpose::ClientAuth],
        )
    }

    /// Mints the controller's own server cert with one DNS SAN.
    ///
    /// Same shape as [`Authority::sign_agent_leaf`] but with both
    /// `ClientAuth` (so the controller can also dial out via mTLS in
    /// future) and `ServerAuth` (required for the gRPC server's TLS
    /// handshake). Thin wrapper around
    /// [`Authority::sign_server_leaf_with_sans`] with no extra SANs.
    ///
    /// # Errors
    ///
    /// Same as [`Authority::sign_server_leaf_with_sans`].
    pub fn sign_server_leaf(
        &self,
        host_id: HostId,
        hostname: &str,
        ttl: Duration,
    ) -> Result<IssuedLeaf> {
        self.sign_server_leaf_with_sans(host_id, hostname, &[], ttl)
    }

    /// Mints the controller's server cert with extra Subject Alternative
    /// Names.
    ///
    /// Each entry in `extra_sans` is parsed as an IP address (v4 or v6)
    /// when it lexes as one and as a DNS name otherwise. Empty and
    /// blank entries are silently skipped so the caller can pass an
    /// unfiltered comma-split.
    ///
    /// `isengard init` uses this to thread `localhost`, `127.0.0.1`,
    /// the auto-detected host IP, and any operator-supplied hostnames
    /// through to the controller's first-boot server-cert generation.
    /// Without these SANs the local agent could not reach
    /// `https://localhost:9417` and remote `isengard join` hosts could
    /// not pin the controller by IP.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `hostname` or any `extra_sans` entry exceeds
    /// the IA5 charset, when `hostname` is longer than
    /// `Authority::MAX_HOSTNAME_LEN`, or when rcgen fails to sign.
    pub fn sign_server_leaf_with_sans(
        &self,
        host_id: HostId,
        hostname: &str,
        extra_sans: &[String],
        ttl: Duration,
    ) -> Result<IssuedLeaf> {
        self.sign_leaf_inner(
            host_id,
            hostname,
            extra_sans,
            ttl,
            &[
                ExtendedKeyUsagePurpose::ClientAuth,
                ExtendedKeyUsagePurpose::ServerAuth,
            ],
        )
    }

    /// Shared mint path behind every leaf sign call. Validates the
    /// hostname length, generates the leaf key, builds SANs, sets the
    /// EKUs, mints a 16-byte random serial, and signs with the root.
    fn sign_leaf_inner(
        &self,
        host_id: HostId,
        hostname: &str,
        extra_sans: &[String],
        ttl: Duration,
        ekus: &[ExtendedKeyUsagePurpose],
    ) -> Result<IssuedLeaf> {
        if hostname.len() > Self::MAX_HOSTNAME_LEN {
            return Err(anyhow::anyhow!(
                "hostname too long ({} > {})",
                hostname.len(),
                Self::MAX_HOSTNAME_LEN,
            ));
        }
        let leaf_key =
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generate leaf key")?;
        let mut params = CertificateParams::new(vec![]).context("leaf params")?;
        params
            .distinguished_name
            .push(DnType::CommonName, host_id.to_string());
        params.subject_alt_names.push(SanType::DnsName(
            hostname.try_into().context("hostname not ia5")?,
        ));
        for raw in extra_sans {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed == hostname {
                continue;
            }
            if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
                params.subject_alt_names.push(SanType::IpAddress(ip));
            } else {
                params.subject_alt_names.push(SanType::DnsName(
                    trimmed
                        .try_into()
                        .with_context(|| format!("extra SAN {trimmed:?} not ia5"))?,
                ));
            }
        }
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = ekus.to_vec();
        let now = Utc::now();
        let expires_at = now + ttl;
        params.not_before = chrono_to_offset(now);
        params.not_after = chrono_to_offset(expires_at);

        let mut serial_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut serial_bytes);
        params.serial_number = Some(SerialNumber::from_slice(&serial_bytes));

        let leaf_cert = params
            .signed_by(&leaf_key, &self.cert, &self.key_pair)
            .context("sign leaf")?;

        Ok(IssuedLeaf {
            cert_pem: leaf_cert.pem(),
            key_pem: leaf_key.serialize_pem(),
            serial: serial_bytes.to_vec(),
            expires_at,
        })
    }
}

/// Converts a `chrono::DateTime<Utc>` to the `time::OffsetDateTime`
/// shape rcgen's `not_before` / `not_after` fields require.
///
/// rcgen has no chrono interop. Lossless within nanosecond precision;
/// both wrap the same UNIX instant.
///
/// # Panics
///
/// Panics only on values that don't fit in `i64` seconds. That can't
/// happen for `Utc::now()`-derived inputs, which is the only call site.
fn chrono_to_offset(dt: DateTime<Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp())
        .expect("chrono Utc::now()-derived timestamp always fits in i64 seconds")
}
