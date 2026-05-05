//! Internal Certificate Authority for issuing per-agent mTLS certs.
//!
//! See spec docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md.
//!
//! The CA is a single self-signed ECDSA P-256 root persisted in the
//! controller's SQLite (`ca` table, single row). On controller boot,
//! [`Authority::load_or_init`] either loads the persisted root or generates
//! and persists a new one. Per-agent leaf certs are minted by
//! [`Authority::sign_agent_leaf`] (ClientAuth-only, used by the enrollment
//! service for agent leaves) and the controller's own server cert is
//! minted by [`Authority::sign_server_leaf`] (ClientAuth + ServerAuth).

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

/// Root CA validity. Picked long enough that we don't have to think about
/// rotating the root for the foreseeable lifetime of an Isengard install.
const CA_TTL_DAYS: i64 = 365 * 10;

/// In-memory handle to the controller's internal CA. Holds the parsed root
/// cert + signing key, plus the PEM blobs (kept around so callers asking for
/// the trust anchor don't have to re-serialize on every request).
pub struct Authority {
    cert_pem: String,
    key_pem: String,
    cert: Certificate,
    key_pair: KeyPair,
}

/// Output of [`Authority::sign_agent_leaf`]. The serial is the 16-byte
/// random value the controller assigned (mirrored into the
/// `agent_certs.serial` PK so revocation can match).
pub struct IssuedLeaf {
    pub cert_pem: String,
    pub key_pem: String,
    pub serial: Vec<u8>,
    pub expires_at: DateTime<Utc>,
}

impl Authority {
    /// Load the CA from inventory, or generate + persist a new one if the
    /// `ca` table is empty. Idempotent: calling twice returns an Authority
    /// backed by the same persisted PEM blobs.
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

    /// PEM-encoded root certificate. This is what agents pin as their trust
    /// anchor; also returned in the enroll response.
    pub fn root_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// PEM-encoded root private key. Used by the controller's own
    /// server-cert generation path; never sent over the wire.
    pub fn root_key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Maximum length of an agent-supplied hostname. DNS labels are bounded
    /// to 253 characters total; we apply the same cap as a basic sanity
    /// guard against absurd inputs (Imp-1 bonus). Anything longer is
    /// rejected up-front so we never feed a 4-MB SAN to rcgen.
    const MAX_HOSTNAME_LEN: usize = 253;

    /// Mint a per-agent leaf cert (Imp-1: ClientAuth EKU only).
    ///
    /// CN = host UUID, single SAN (DNS) = hostname, EKU = ClientAuth.
    /// Random 16-byte serial. Pre-Imp-1 the EKU also included ServerAuth,
    /// which combined with agent-controlled hostnames let an agent get a
    /// cert other peers would trust as the controller (e.g. by enrolling
    /// with `hostname: "controller.local"`). Restricting agent leaves to
    /// ClientAuth closes that path. Controller-side server certs are
    /// minted via [`sign_server_leaf`] instead.
    pub fn sign_agent_leaf(
        &self,
        host_id: HostId,
        hostname: &str,
        ttl: Duration,
    ) -> Result<IssuedLeaf> {
        self.sign_leaf_inner(
            host_id,
            hostname,
            ttl,
            &[ExtendedKeyUsagePurpose::ClientAuth],
        )
    }

    /// Mint the controller's own server cert. Same shape as
    /// [`sign_agent_leaf`] but with both ClientAuth (so the controller can
    /// also dial out via mTLS in future) and ServerAuth (required for the
    /// gRPC server's TLS handshake).
    pub fn sign_server_leaf(
        &self,
        host_id: HostId,
        hostname: &str,
        ttl: Duration,
    ) -> Result<IssuedLeaf> {
        self.sign_leaf_inner(
            host_id,
            hostname,
            ttl,
            &[
                ExtendedKeyUsagePurpose::ClientAuth,
                ExtendedKeyUsagePurpose::ServerAuth,
            ],
        )
    }

    fn sign_leaf_inner(
        &self,
        host_id: HostId,
        hostname: &str,
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

/// Convert a `chrono::DateTime<Utc>` to the `time::OffsetDateTime` that
/// rcgen's `not_before` / `not_after` fields require. rcgen has no chrono
/// interop. Lossless within nanosecond precision; both wrap the same UNIX
/// instant. Panics only on values that can't fit in i64 seconds, which
/// can't happen for `Utc::now()`-derived times.
fn chrono_to_offset(dt: DateTime<Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp())
        .expect("chrono Utc::now()-derived timestamp always fits in i64 seconds")
}
