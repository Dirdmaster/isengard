//! Phase 14 Task 12: agent-side cert renewal task.
//!
//! Periodically inspects the on-disk cert bundle's TTL. When the cert is past
//! 50% of its validity window, calls `RenewCert` against the controller and
//! atomically swaps the new bundle into place. The mTLS channel held by the
//! sync loop is rebuilt from the same `Endpoint` on every reconnect, so the
//! freshly written cert is picked up the next time the stream cycles.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use isengard_proto::pb::RenewCertRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use tokio::sync::RwLock;
use tonic::transport::Channel;
use tracing::{info, warn};

use crate::cert_store::{self, CertBundle};
use isengard_storage::host::HostId;

/// True when `now` is at or past the midpoint of `[issued_at, expires_at]`.
/// Used by the renewal loop to decide whether to call `RenewCert`.
pub fn should_renew(issued_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> bool {
    let total = expires_at - issued_at;
    let half = total / 2;
    Utc::now() >= issued_at + half
}

/// Long-running task. On each poll: check current cert's TTL; if past 50%, call
/// RenewCert and atomically swap the cert bundle on disk. The mTLS channel held
/// by the sync loop will rebuild on its next reconnect (see sync.rs).
pub async fn run_renewal_loop(
    state_dir: PathBuf,
    host_id: HostId,
    channel_holder: Arc<RwLock<Channel>>,
    poll_interval: Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(poll_interval).await;
        if let Err(e) = maybe_renew(&state_dir, host_id, channel_holder.clone()).await {
            warn!(error=%e, "cert renewal check failed");
        }
    }
}

async fn maybe_renew(
    state_dir: &Path,
    host_id: HostId,
    channel_holder: Arc<RwLock<Channel>>,
) -> Result<()> {
    let bundle = cert_store::load(state_dir)?;
    let (issued_at, expires_at) = parse_validity(&bundle.cert_pem)?;
    if !should_renew(issued_at, expires_at) {
        return Ok(());
    }

    info!(
        host_id = %host_id,
        expires_at = %expires_at,
        "cert past 50% TTL, renewing",
    );
    let channel = channel_holder.read().await.clone();
    let mut client = ControllerClient::new(channel);
    // Bl-1 fix: RenewCertRequest no longer carries host_id; the controller
    // reads it authoritatively from the client cert CN.
    let resp = client
        .renew_cert(RenewCertRequest {})
        .await
        .context("renew_cert RPC")?
        .into_inner();

    let new_bundle = CertBundle {
        ca_pem: bundle.ca_pem,
        cert_pem: resp.agent_cert_pem,
        key_pem: resp.agent_key_pem,
    };
    cert_store::save(state_dir, &new_bundle)?;

    info!("cert renewed; channel will swap on next sync reconnect");
    Ok(())
}

fn parse_validity(cert_pem: &str) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    use x509_parser::pem::parse_x509_pem;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes())?;
    let cert = pem.parse_x509()?;
    let nb = cert.tbs_certificate.validity.not_before.timestamp();
    let na = cert.tbs_certificate.validity.not_after.timestamp();
    Ok((
        DateTime::<Utc>::from_timestamp(nb, 0).context("not_before timestamp")?,
        DateTime::<Utc>::from_timestamp(na, 0).context("not_after timestamp")?,
    ))
}
