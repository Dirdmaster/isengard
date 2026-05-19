//! Agent → controller enrollment.
//!
//! On first boot the agent has no CA cert to validate against. The join
//! token carries the SHA-256 of the controller's CA:
//! [`fetch_and_verify_ca`] fetches the controller's CA over skip-verify
//! TLS and confirms its digest matches the fingerprint embedded in the
//! packed token before the real Enroll RPC runs over an mTLS channel
//! rooted at the verified CA.
//!
//! The fingerprint-verified PEM is the only trust anchor: there is no
//! env-var or path fallback.

#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, anyhow};

use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_storage::host::HostId;
use tonic::transport::{Certificate, ClientTlsConfig};

use crate::cert_store::CertBundle;

/// Trust anchor for the bootstrap channel. The only path
/// is the fingerprint flow: [`fetch_and_verify_ca`] populates
/// `verified_ca_pem` after confirming the CA fetched over skip-verify
/// TLS matches the SHA-256 embedded in the packed join token.
#[derive(Debug, Clone, Default)]
pub struct BootstrapTrust {
    /// CA PEM that has already been fingerprint-verified against
    /// the join token's embedded SHA-256.
    pub verified_ca_pem: Option<Vec<u8>>,
}

/// Resolved host metadata included in the EnrollRequest.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
}

impl HostInfo {
    /// Best-effort host detection. `hostname` falls back to `"unknown"` if the
    /// OS call fails or returns non-UTF-8 bytes.
    pub fn detect() -> Self {
        Self {
            hostname: hostname::get()
                .ok()
                .and_then(|s| s.into_string().ok())
                .unwrap_or_else(|| "unknown".into()),
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Successful enrollment outcome. Caller persists `bundle` via
/// [`crate::cert_store::save`] and `host_id` + `heartbeat_interval_secs` via
/// [`crate::agent_state::save`] before any subsequent RPC.
#[derive(Debug)]
pub struct EnrollOutcome {
    pub host_id: HostId,
    pub bundle: CertBundle,
    pub heartbeat_interval_secs: u32,
}

/// Bootstrap-trust enrollment. The bootstrap channel's trust anchor is
/// the fingerprint-verified CA PEM from [`BootstrapTrust::verified_ca_pem`].
/// All subsequent RPCs run over an mTLS channel rooted at the CA returned
/// in `EnrollResponse`.
pub async fn enroll(
    controller_url: &str,
    enroll_token: &str,
    host_info: HostInfo,
    trust: BootstrapTrust,
) -> Result<EnrollOutcome> {
    let bootstrap_tls = build_bootstrap_tls(&trust)?;

    let channel = tonic::transport::Channel::from_shared(controller_url.to_string())
        .with_context(|| format!("invalid controller url {controller_url:?}"))?
        .tls_config(bootstrap_tls)
        .context("install bootstrap tls config")?
        .connect()
        .await
        .with_context(|| format!("connect bootstrap channel to {controller_url}"))?;

    let mut client = ControllerClient::new(channel);
    let resp = client
        .enroll(EnrollRequest {
            token: enroll_token.to_string(),
            hostname: host_info.hostname,
            os: host_info.os,
            version: host_info.version,
        })
        .await
        .context("Enroll RPC failed")?
        .into_inner();

    let host_id = HostId::from_db_bytes(resp.host_id)
        .map_err(|e| anyhow!("invalid host_id from controller: {e}"))?;
    let bundle = CertBundle {
        ca_pem: resp.ca_root_pem,
        cert_pem: resp.agent_cert_pem,
        key_pem: resp.agent_key_pem,
    };

    Ok(EnrollOutcome {
        host_id,
        bundle,
        heartbeat_interval_secs: resp.heartbeat_interval_secs,
    })
}

/// Build the bootstrap channel TLS config. Only the fingerprint-verified
/// PEM is accepted; a `None` trust value means the caller did not run the
/// pre-enroll verify, which is a hard error pointing the operator at
/// minting a fresh packed token.
fn build_bootstrap_tls(trust: &BootstrapTrust) -> Result<ClientTlsConfig> {
    let pem = trust
        .verified_ca_pem
        .as_ref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "token is missing the controller CA fingerprint. \
                 Mint a fresh token with `isd join-token` and re-run `isd join`."
            )
        })?;
    Ok(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem)))
}

/// Pre-enroll fingerprint verify.
///
/// Fetches the controller's CA PEM over skip-verify TLS, compares its
/// SHA-256 against the fingerprint embedded in the packed token, and
/// returns the verified PEM bytes on match. Caller uses the returned
/// PEM to build a properly-validating reqwest::Client (or tonic channel)
/// for the actual Enroll RPC.
///
/// Threat model: the MITM window is one HTTP request long. Successfully
/// spoofing the CA requires preimage resistance on SHA-256, which is
/// computationally infeasible. Same mechanic as `docker swarm join`.
pub async fn fetch_and_verify_ca(
    controller_url: &str,
    packed_token: &str,
) -> anyhow::Result<Vec<u8>> {
    let parsed = isengard_core::join_token::parse(packed_token)
        .map_err(|e| anyhow!("invalid token: {e}"))?;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow!("building skip-verify client: {e}"))?;

    let url = format!("{}/api/v1/ca/pem", controller_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("fetching CA from controller: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("controller returned {status} for GET {url}"));
    }
    let pem = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("reading CA response body: {e}"))?
        .to_vec();

    let actual = isengard_core::join_token::fingerprint(&pem);
    if actual != parsed.fingerprint {
        return Err(anyhow!(
            "controller CA fingerprint mismatch: token says {} but controller served CA with fingerprint {}. \
             Token was either (a) minted against a different controller, (b) intercepted, or (c) the controller's CA was rotated since the token was minted",
            hex_full(&parsed.fingerprint),
            hex_full(&actual)
        ));
    }

    Ok(pem)
}

fn hex_full(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod bootstrap_tls_tests {
    //! `build_bootstrap_tls` only accepts the fingerprint-verified PEM.
    //! Missing / empty PEM hard-fails with an operator-actionable error
    //! pointing at `isd join-token`.

    use super::{BootstrapTrust, build_bootstrap_tls};

    #[test]
    fn verified_pem_is_accepted() {
        let pem = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".to_vec();
        let trust = BootstrapTrust {
            verified_ca_pem: Some(pem),
        };
        // Just asserting the function doesn't error on a real PEM. The CA is
        // not parsed here: tonic does that lazily when the channel handshakes.
        build_bootstrap_tls(&trust).unwrap();
    }

    #[test]
    fn missing_verified_pem_surfaces_operator_pointer() {
        let trust = BootstrapTrust {
            verified_ca_pem: None,
        };
        let err = build_bootstrap_tls(&trust).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("missing the controller CA fingerprint"),
            "{rendered}"
        );
        assert!(rendered.contains("isd join-token"), "{rendered}");
    }

    #[test]
    fn empty_verified_pem_surfaces_operator_pointer() {
        let trust = BootstrapTrust {
            verified_ca_pem: Some(Vec::new()),
        };
        let err = build_bootstrap_tls(&trust).unwrap_err();
        assert!(format!("{err:#}").contains("missing the controller CA fingerprint"));
    }
}
