//! Phase 14: agent → controller enrollment.
//!
//! On first boot the agent has no CA cert to validate against. Trust for the
//! bootstrap `Enroll` channel is resolved in this order:
//!
//!   1. `ISENGARD_CONTROLLER_CA_PEM_PATH` env var (or `--controller-ca-pem-path`
//!      CLI arg) — read the CA root cert PEM from disk and pin it.
//!   2. `ISENGARD_CONTROLLER_CA_PEM` env var — inline PEM string, pin directly.
//!   3. Fallback: trust the platform's native root store. Works when the
//!      controller serves a publicly-signed cert (e.g. Let's Encrypt) but
//!      FAILS for the default self-signed internal CA — operators running
//!      that setup MUST pass a pinned CA via 1 or 2.
//!
//! Every RPC after Enroll runs over an mTLS channel rooted at the CA returned
//! in `EnrollResponse.ca_root_pem` (which the agent persists and reuses).

#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, anyhow};

use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_storage::host::HostId;
use tonic::transport::{Certificate, ClientTlsConfig};

use crate::cert_store::CertBundle;

/// Env var pointing at a PEM file containing the controller's CA root cert.
/// Read once at enrollment to pin the bootstrap channel.
pub const CONTROLLER_CA_PEM_PATH_ENV: &str = "ISENGARD_CONTROLLER_CA_PEM_PATH";
/// Env var carrying the controller's CA root cert PEM inline. Used when a
/// file path isn't convenient (e.g. CI secrets).
pub const CONTROLLER_CA_PEM_ENV: &str = "ISENGARD_CONTROLLER_CA_PEM";

/// Optional pinned CA material for the bootstrap channel. Resolution order
/// inside [`enroll`] is path env > inline env > caller-supplied > native roots.
#[derive(Debug, Clone, Default)]
pub struct BootstrapTrust {
    /// Path to a PEM file holding the controller CA. Equivalent to setting
    /// `ISENGARD_CONTROLLER_CA_PEM_PATH` but plumbed through `AgentOptions`.
    pub ca_pem_path: Option<std::path::PathBuf>,
    /// Inline PEM bytes. Equivalent to `ISENGARD_CONTROLLER_CA_PEM`.
    pub ca_pem: Option<String>,
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
/// resolved per [`BootstrapTrust`] / env-var precedence (see module docs).
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

/// Resolve a [`ClientTlsConfig`] for the bootstrap channel. Precedence:
/// path env > inline env > caller-provided path > caller-provided inline >
/// native roots fallback. The first source that yields a non-empty PEM wins.
fn build_bootstrap_tls(trust: &BootstrapTrust) -> Result<ClientTlsConfig> {
    if let Ok(path) = std::env::var(CONTROLLER_CA_PEM_PATH_ENV) {
        if !path.is_empty() {
            let pem = std::fs::read_to_string(&path).with_context(|| {
                format!("reading {CONTROLLER_CA_PEM_PATH_ENV}={path:?} for bootstrap CA")
            })?;
            return Ok(pin_ca(&pem));
        }
    }
    if let Ok(pem) = std::env::var(CONTROLLER_CA_PEM_ENV) {
        if !pem.is_empty() {
            return Ok(pin_ca(&pem));
        }
    }
    if let Some(path) = trust.ca_pem_path.as_ref() {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading bootstrap CA from {path:?}"))?;
        return Ok(pin_ca(&pem));
    }
    if let Some(pem) = trust.ca_pem.as_ref() {
        if !pem.is_empty() {
            return Ok(pin_ca(pem));
        }
    }
    Ok(ClientTlsConfig::new().with_native_roots())
}

fn pin_ca(pem: &str) -> ClientTlsConfig {
    ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem.as_bytes()))
}

#[cfg(test)]
mod bootstrap_tls_tests {
    //! Resolution-order checks for [`build_bootstrap_tls`]. We can't easily
    //! assert the resulting `ClientTlsConfig` (no public accessors), so these
    //! exercise the I/O side: missing files surface, present files are read.

    use super::{BootstrapTrust, build_bootstrap_tls};
    use std::io::Write;

    #[test]
    fn caller_provided_path_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n")
            .unwrap();
        let trust = BootstrapTrust {
            ca_pem_path: Some(path),
            ca_pem: None,
        };
        // Just asserting the function doesn't error on a real file. The CA is
        // not parsed here — tonic does that lazily when the channel handshakes.
        build_bootstrap_tls(&trust).unwrap();
    }

    #[test]
    fn missing_caller_path_surfaces_error() {
        let trust = BootstrapTrust {
            ca_pem_path: Some("/nonexistent/path/ca.pem".into()),
            ca_pem: None,
        };
        let err = build_bootstrap_tls(&trust).unwrap_err();
        assert!(format!("{err:#}").contains("bootstrap CA"));
    }
}
