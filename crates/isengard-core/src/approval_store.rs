//! `ApprovalStore`: cross-crate seam for the updater plugin to persist + look
//! up pending approval rows without taking a direct dependency on
//! `isengard-storage` semantics.
//!
//! Mirrors the [`crate::PolicyLoader`] pattern: a small trait lives here in
//! `isengard-core`, the agent runtime constructs an implementation backed by
//! its `Inventory`, and threads it through [`crate::PluginContext`] to every
//! plugin via `with_approval_store`.
//!
//! The trait is deliberately narrow: only the two methods the updater needs
//! at the resolved-gate=Approval branch (`find_open_approval_for_proposed_digest`
//! for idempotence, `insert_pending_approval` for first-time persistence). The
//! dashboard's full `decide_pending_approval` / `list_pending_approvals` /
//! `expire_pending_approvals` surface stays on `Inventory` directly because
//! the dashboard already has a hard storage dependency.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HostId;

/// Owned mirror of the storage-side `UpdateApprovalBody`. Lives in core so the
/// updater can construct the body without taking a direct storage dep.
///
/// Field order + names match `isengard_storage::host_action::UpdateApprovalBody`
/// exactly so the storage impl can `From`-convert in one shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PendingApprovalBody {
    pub host_id: HostId,
    pub stack: String,
    pub service: String,
    pub container_name: String,
    pub image: String,
    pub current_digest: String,
    pub proposed_digest: String,
    pub diff_url: Option<String>,
    pub approver_channel: Option<String>,
}

/// Insert payload accepted by [`ApprovalStore::insert_pending_approval`].
#[derive(Debug, Clone)]
pub struct InsertPendingApproval {
    pub body: PendingApprovalBody,
    pub expires_at: DateTime<Utc>,
    pub approver_channel: Option<String>,
}

/// Slim record returned by the trait. Only the `action_id` is needed at the
/// updater seam (it goes into the emitted `update.pending_approval` event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalRecord {
    pub action_id: String,
}

/// Async store for pending-approval rows.
///
/// Implementations live in higher crates: the production one in
/// `isengard-storage` wraps an `Arc<Inventory>`; tests hand the updater a
/// fixture store to seed deterministic state.
#[async_trait]
pub trait ApprovalStore: Send + Sync + 'static {
    /// Idempotence helper: is there already an open approval row for this
    /// `(host_id, stack, service, proposed_digest)` tuple? Returns the most
    /// recent open row's record if any.
    async fn find_open_approval_for_proposed_digest(
        &self,
        host_id: HostId,
        stack: &str,
        service: &str,
        proposed_digest: &str,
    ) -> Result<Option<PendingApprovalRecord>, ApprovalStoreError>;

    /// Insert a brand-new pending-approval row in state `pending_open`.
    /// Returns the record so the caller can emit `update.pending_approval`.
    async fn insert_pending_approval(
        &self,
        ins: InsertPendingApproval,
    ) -> Result<PendingApprovalRecord, ApprovalStoreError>;
}

/// Opaque error wrapper. The updater treats every store error the same: log
/// and skip the persist step for this candidate (fail-safe: notifier won't
/// fire, but the cycle continues).
#[derive(Debug)]
pub struct ApprovalStoreError(pub String);

impl std::fmt::Display for ApprovalStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "approval store error: {}", self.0)
    }
}

impl std::error::Error for ApprovalStoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct FixedStore;

    #[async_trait]
    impl ApprovalStore for FixedStore {
        async fn find_open_approval_for_proposed_digest(
            &self,
            _host_id: HostId,
            _stack: &str,
            _service: &str,
            _proposed_digest: &str,
        ) -> Result<Option<PendingApprovalRecord>, ApprovalStoreError> {
            Ok(None)
        }

        async fn insert_pending_approval(
            &self,
            _ins: InsertPendingApproval,
        ) -> Result<PendingApprovalRecord, ApprovalStoreError> {
            Ok(PendingApprovalRecord {
                action_id: "01H0000000000000000000000".into(),
            })
        }
    }

    #[tokio::test]
    async fn fixed_store_round_trips_methods() {
        let store: Arc<dyn ApprovalStore> = Arc::new(FixedStore);
        let host_id = HostId::new();
        let r = store
            .find_open_approval_for_proposed_digest(host_id, "s", "svc", "sha256:x")
            .await
            .expect("call");
        assert!(r.is_none());

        let body = PendingApprovalBody {
            host_id,
            stack: "s".into(),
            service: "svc".into(),
            container_name: "s_svc_1".into(),
            image: "ghcr.io/foo/bar".into(),
            current_digest: "sha256:a".into(),
            proposed_digest: "sha256:b".into(),
            diff_url: None,
            approver_channel: None,
        };
        let rec = store
            .insert_pending_approval(InsertPendingApproval {
                body,
                expires_at: Utc::now(),
                approver_channel: None,
            })
            .await
            .expect("insert");
        assert!(!rec.action_id.is_empty());
    }
}
