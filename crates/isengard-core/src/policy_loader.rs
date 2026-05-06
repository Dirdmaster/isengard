//! `PolicyLoader`: cross-crate seam for the updater plugin to fetch policy
//! rows without taking a direct dependency on `isengard-storage` semantics.
//!
//! Mirrors the [`crate::UpdateDispatcher`] pattern: a small trait lives here
//! in `isengard-core`, the agent runtime constructs an implementation backed
//! by its `Inventory`, and threads it through [`crate::PluginContext`] to
//! every plugin via `with_policy_loader`.
//!
//! The loaded snapshot is owned (not borrowed) so the resolver caller can
//! move it across `.await` boundaries. The resolver itself takes a borrowed
//! slice; the cycle simply re-borrows the snapshot per candidate.

use async_trait::async_trait;

use crate::HostId;
use crate::policy::{Policy, PolicyScopeType};

/// One row's worth of policy data, owned. Mirrors
/// `isengard_storage::PolicyRow` but excludes id + timestamps; the resolver
/// only needs `(scope_type, scope_key, body)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPolicy {
    pub scope_type: PolicyScopeType,
    pub scope_key: String,
    pub body: Policy,
}

/// Async loader returning the full set of policy rows.
///
/// Implementations live in higher crates: the production one in
/// `isengard-storage` wraps an `Arc<Inventory>`; tests hand the updater a
/// fixture loader to seed deterministic policy state.
#[async_trait]
pub trait PolicyLoader: Send + Sync + 'static {
    /// Load every policy row in scope-rank order (the order the resolver
    /// expects). Implementations may return rows in any order; the resolver
    /// re-sorts internally.
    async fn list(&self) -> Result<Vec<LoadedPolicy>, PolicyLoaderError>;

    /// Look up the fleet name for a host, if known. Used by the updater
    /// plugin to seed `PolicyContext::fleet` once at init (cached for the
    /// life of the plugin). Returns `Ok(None)` when no host row matches
    /// (mirrors `Inventory::get_host` returning `None`).
    ///
    /// Default impl returns `Ok(None)` so test fixtures don't have to
    /// stub it.
    async fn fleet_for(&self, _host_id: HostId) -> Result<Option<String>, PolicyLoaderError> {
        Ok(None)
    }
}

/// Opaque error wrapper. The updater treats every loader error the same:
/// log and skip the policy step for this cycle (fail-safe: behave as if
/// no policies exist).
#[derive(Debug)]
pub struct PolicyLoaderError(pub String);

impl std::fmt::Display for PolicyLoaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "policy loader error: {}", self.0)
    }
}

impl std::error::Error for PolicyLoaderError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use std::sync::Arc;

    struct FixedLoader(Vec<LoadedPolicy>);

    #[async_trait]
    impl PolicyLoader for FixedLoader {
        async fn list(&self) -> Result<Vec<LoadedPolicy>, PolicyLoaderError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn fixed_loader_round_trips_a_row() {
        let row = LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: Policy::default(),
        };
        let loader: Arc<dyn PolicyLoader> = Arc::new(FixedLoader(vec![row.clone()]));
        let out = loader.list().await.expect("load");
        assert_eq!(out, vec![row]);
    }
}
