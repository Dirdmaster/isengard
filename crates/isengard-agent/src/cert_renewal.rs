//! Agent-side cert renewal task.
//!
//! Periodically inspects the on-disk cert bundle's TTL. When the cert is past
//! 50% of its validity window, calls `RenewCert` against the controller and
//! atomically swaps the new bundle into place. The mTLS Endpoint shared with
//! the sync loop (via `Arc<RwLock<Endpoint>>`) is rebuilt from the freshly
//! written bundle so the next reconnect picks up the new identity: no agent
//! restart needed (Imp-2 fix; pre-fix the live Endpoint kept the old cert
//! bytes baked in until the process restarted).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use isengard_proto::pb::RenewCertRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use tokio::sync::RwLock;
use tonic::transport::Endpoint;
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

/// Long-running task. On each poll: check current cert's TTL; if past 50%,
/// call RenewCert, atomically swap the cert bundle on disk, and rebuild the
/// shared Endpoint so the sync loop's next reconnect uses the new cert.
///
/// `controller_url` and `endpoint_builder` together let us regenerate the
/// Endpoint from a freshly-loaded bundle. The builder takes the same shape as
/// `lib.rs::build_mtls_endpoint`: passed in instead of imported to keep this
/// module from pulling in the agent-options/types graph.
pub async fn run_renewal_loop(
    state_dir: PathBuf,
    host_id: HostId,
    endpoint_holder: Arc<RwLock<Endpoint>>,
    controller_url: String,
    endpoint_builder: EndpointBuilder,
    poll_interval: Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(poll_interval).await;
        if let Err(e) = maybe_renew(
            &state_dir,
            host_id,
            endpoint_holder.clone(),
            &controller_url,
            &endpoint_builder,
        )
        .await
        {
            warn!(error=%e, "cert renewal check failed");
        }
    }
}

/// Builder closure: takes (controller_url, bundle) and returns an Endpoint.
/// Indirected so the renewal module doesn't need to know how to construct
/// the mTLS config (lives in `lib.rs::build_mtls_endpoint`).
pub type EndpointBuilder = Arc<dyn Fn(&str, &CertBundle) -> Result<Endpoint> + Send + Sync>;

/// Internal helper: maybe renew.
async fn maybe_renew(
    state_dir: &Path,
    host_id: HostId,
    endpoint_holder: Arc<RwLock<Endpoint>>,
    controller_url: &str,
    endpoint_builder: &EndpointBuilder,
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
    // Snapshot the current endpoint, dial it, call RenewCert. We cap how
    // long we hold the read lock so a slow connect can't block a future
    // swap.
    let endpoint_snapshot = endpoint_holder.read().await.clone();
    let channel = endpoint_snapshot
        .connect()
        .await
        .context("dial controller for renew_cert")?;
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

    // Imp-2: rebuild the Endpoint with the freshly-written bundle and swap
    // it in. Sync's reconnect loop snapshots the inner Endpoint on every
    // attempt, so the new cert takes effect on the next stream cycle :
    // no agent restart required.
    let new_endpoint = endpoint_builder(controller_url, &new_bundle)
        .context("rebuild mTLS endpoint with renewed cert")?;
    *endpoint_holder.write().await = new_endpoint;

    info!("cert renewed; endpoint swapped, sync loop will adopt on next reconnect");
    Ok(())
}

/// Internal helper: parse validity.
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
