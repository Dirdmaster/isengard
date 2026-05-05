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

/// Imp-3 fix: revoke EVERY currently-active cert for `host_id` (not just
/// the most-recent). Pre-fix the controller called
/// [`Inventory::active_cert_for_host`] which is `LIMIT 1 ORDER BY issued_at
/// DESC`, so an older still-active cert (e.g. the one the agent was
/// presenting before a renewal) survived a single revoke call. The fix
/// moves to a single SQL `UPDATE ... WHERE host_id = ? AND revoked_at IS
/// NULL` via [`Inventory::revoke_all_active_for_host`].
///
/// Returns an error only on storage failure. A host with zero active
/// certs is treated as a no-op success — callers should use this when
/// they want "kick this host off, however many certs it has".
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
