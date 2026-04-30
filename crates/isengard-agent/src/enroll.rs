//! Agent → controller enrollment.
//!
//! Builds a tonic Channel, attaches the bearer-token interceptor, calls the
//! `Enroll` RPC once, and returns the controller-assigned `agent_id`.

#![allow(clippy::result_large_err)]

use anyhow::Context;
use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::Result;

/// Resolved host metadata included in the EnrollRequest.
pub struct HostInfo {
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
}

impl HostInfo {
    /// Best-effort: hostname from the OS, OS/arch from rustc consts, agent
    /// version from cargo, docker version blank for now (Phase 3 queries
    /// the docker daemon).
    pub fn detect() -> Self {
        let hostname = hostname::get()
            .ok()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| "unknown-host".into());

        Self {
            fingerprint: hostname.clone(), // Phase 2d uses hostname as fingerprint; machine-id later
            hostname,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version: String::new(), // Phase 3
        }
    }
}

/// Issue an Enroll RPC against the configured controller. Returns the
/// controller-assigned `agent_id` as a string.
pub async fn enroll(controller_url: &str, token: &str, info: HostInfo) -> Result<String> {
    let channel = Channel::from_shared(controller_url.to_string())
        .with_context(|| format!("invalid controller url {controller_url:?}"))?
        .connect()
        .await
        .with_context(|| format!("connecting to controller at {controller_url}"))?;

    let bearer: MetadataValue<_> = format!("Bearer {token}")
        .parse()
        .context("token contains characters not legal in a Bearer header")?;

    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    let req = EnrollRequest {
        fingerprint: info.fingerprint,
        hostname: info.hostname,
        os: info.os,
        arch: info.arch,
        agent_version: info.agent_version,
        docker_version: info.docker_version,
    };

    let resp = client
        .enroll(req)
        .await
        .context("Enroll RPC failed")?
        .into_inner();

    if resp.agent_id.is_empty() {
        anyhow::bail!("controller returned empty agent_id");
    }

    Ok(resp.agent_id)
}
