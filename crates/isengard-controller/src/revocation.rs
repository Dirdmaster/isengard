//! In-memory revocation set and persistent revoke helper.
//!
//! [`crate::auth::CertAuthInterceptor`] checks every incoming RPC's
//! client-cert serial against the [`RevocationSet`] on the hot path. The
//! set is hydrated from the `agent_certs` table at controller startup
//! via [`RevocationSet::load_from_inventory`] and mutated through either
//! [`revoke_agent`] (writes the DB row and the set in one call) or
//! [`RevocationSet::revoke`] (set-only, used after callers have already
//! persisted).
//!
//! Lock choice: `std::sync::RwLock` keeps `contains` and `revoke`
//! synchronous so the tonic interceptor's sync `call` hook can use them
//! directly without blocking the async runtime.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};

use isengard_storage::Inventory;
use isengard_storage::host::HostId;

/// Set of revoked X.509 serials (raw bytes).
///
/// Cheap to clone: the underlying `HashSet` is shared through
/// `Arc<RwLock<_>>`.
#[derive(Clone, Default)]
pub struct RevocationSet {
    /// Shared serial set. Reads on every RPC; writes on every revoke.
    inner: Arc<RwLock<HashSet<Vec<u8>>>>,
}

impl RevocationSet {
    /// Builds a fresh set seeded with every revoked serial in the DB.
    ///
    /// Called once at controller boot from [`crate::run_controller`].
    ///
    /// # Errors
    ///
    /// Returns `Err` when the inventory read fails.
    pub async fn load_from_inventory(inventory: &Inventory) -> Result<Self> {
        let serials = inventory.revoked_serials().await?;
        Ok(Self {
            inner: Arc::new(RwLock::new(serials.into_iter().collect())),
        })
    }

    /// Returns `true` when `serial` is in the revoked set.
    ///
    /// Synchronous and cheap: read lock plus hash lookup. Fails closed
    /// on a poisoned lock (treats the cert as revoked) so a panic in
    /// the write path doesn't open up a revocation bypass.
    pub fn contains(&self, serial: &[u8]) -> bool {
        match self.inner.read() {
            Ok(g) => g.contains(serial),
            Err(_) => true,
        }
    }

    /// Inserts `serial` into the in-memory set.
    ///
    /// Does not persist. Callers that want durable revocation must use
    /// [`revoke_agent`] instead. Silently no-ops on a poisoned lock.
    pub fn revoke(&self, serial: Vec<u8>) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(serial);
        }
    }
}

/// Revokes every currently-active cert for `host_id`, persisted and
/// in-memory in one call.
///
/// This is the Imp-3 fix: pre-fix the controller called
/// `Inventory::active_cert_for_host` (`LIMIT 1 ORDER BY issued_at
/// DESC`), so an older still-active cert (e.g. the one the agent was
/// presenting before a renewal) survived a single revoke call. The fix
/// moves to a single SQL `UPDATE ... WHERE host_id = ? AND revoked_at
/// IS NULL` via `Inventory::revoke_all_active_for_host` and mirrors
/// every returned serial into the in-memory set.
///
/// Use this when the goal is "kick this host off, however many certs
/// it has".
///
/// # Errors
///
/// Returns `Err` on storage failure or when the host has zero active
/// certs (so the caller knows the operation was a no-op).
pub async fn revoke_agent(
    inventory: &Inventory,
    revocation: &RevocationSet,
    host_id: HostId,
    reason: &str,
) -> Result<()> {
    let serials = inventory
        .revoke_all_active_for_host(host_id, reason)
        .await?;
    if serials.is_empty() {
        return Err(anyhow!("no active cert for host {host_id}"));
    }
    for serial in serials {
        revocation.revoke(serial);
    }
    Ok(())
}
