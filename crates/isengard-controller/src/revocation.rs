//! In-memory revocation set + persistent revoke helper.
//!
//! The interceptor checks every incoming RPC's client-cert serial against
//! [`RevocationSet`] (sync, very hot path). The set is hydrated from the
//! `agent_certs` table at controller startup via
//! [`RevocationSet::load_from_inventory`], then mutated through
//! [`revoke_agent`] (which writes the DB row and updates the set) or
//! [`RevocationSet::revoke`] (set-only, used after callers have already
//! persisted).
//!
//! Lock choice: `std::sync::RwLock` keeps `contains`/`revoke` synchronous
//! so the tonic [`tonic::service::Interceptor::call`] sync hook can use
//! them directly without blocking the runtime.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};

use isengard_storage::Inventory;
use isengard_storage::host::HostId;

/// Set of revoked X.509 serials (binary). Cheap to clone — the underlying
/// `HashSet` is shared via `Arc<RwLock<_>>`.
#[derive(Clone, Default)]
pub struct RevocationSet {
    inner: Arc<RwLock<HashSet<Vec<u8>>>>,
}

impl RevocationSet {
    /// Build a fresh set seeded with every revoked serial currently in the DB.
    /// Called once at controller boot.
    pub async fn load_from_inventory(inventory: &Inventory) -> Result<Self> {
        let serials = inventory.revoked_serials().await?;
        Ok(Self {
            inner: Arc::new(RwLock::new(serials.into_iter().collect())),
        })
    }

    /// Returns `true` iff `serial` has been revoked. Sync, cheap (read-lock
    /// + hash lookup).
    pub fn contains(&self, serial: &[u8]) -> bool {
        // RwLock is only poisoned on panic-while-write. In that case we fail
        // closed and treat the cert as revoked, since we can't trust state.
        match self.inner.read() {
            Ok(g) => g.contains(serial),
            Err(_) => true,
        }
    }

    /// Insert `serial` into the in-memory set. Does NOT persist — callers
    /// that want durable revocation should use [`revoke_agent`] instead.
    pub fn revoke(&self, serial: Vec<u8>) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(serial);
        }
    }
}

/// Look up the active cert for `host_id`, persistently revoke it via
/// [`Inventory::revoke_cert`], and add the serial to the in-memory set so
/// the very next RPC from that agent is rejected.
///
/// Errors if the host has no active (non-revoked) cert.
pub async fn revoke_agent(
    inventory: &Inventory,
    revocation: &RevocationSet,
    host_id: HostId,
    reason: &str,
) -> Result<()> {
    let cert = inventory
        .active_cert_for_host(host_id)
        .await?
        .ok_or_else(|| anyhow!("no active cert for host {host_id}"))?;
    inventory.revoke_cert(&cert.serial, reason).await?;
    revocation.revoke(cert.serial);
    Ok(())
}
