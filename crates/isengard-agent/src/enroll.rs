//! Phase 14: agent → controller enrollment.
//!
//! On first boot the agent has no CA cert to validate against, so the bootstrap
//! channel for the `Enroll` RPC trusts whatever cert the controller presents
//! (subject to the platform's native root store). This is the documented
//! bootstrap-trust limitation; out-of-band CA pinning / TOFU is a follow-up.
//! Every RPC after this one runs over an mTLS channel rooted at the CA the
//! controller returned in the EnrollResponse.

#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, anyhow};

use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_storage::host::HostId;

use crate::cert_store::CertBundle;

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

/// Bootstrap-trust enrollment: agent has no CA yet, so the channel for this
/// one RPC trusts whatever cert the controller presents (validated against the
/// platform's native root store). All subsequent RPCs run over an mTLS channel
/// rooted at `bundle.ca_pem`.
pub async fn enroll(
    controller_url: &str,
    enroll_token: &str,
    host_info: HostInfo,
) -> Result<EnrollOutcome> {
    let bootstrap_tls = tonic::transport::ClientTlsConfig::new().with_native_roots();

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
