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
//! at the resolved-gate=Approval branch: `find_open_approval_for_proposed_digest`
//! for idempotence, `insert_pending_approval` for first-time persistence. The
//! dashboard's full `decide_pending_approval` / `list_pending_approvals` /
//! `expire_pending_approvals` surface stays on `Inventory` directly because
//! the dashboard already has a hard storage dependency.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HostId;

/// Owned mirror of the storage-side `UpdateApprovalBody`.
///
/// Lives in core so the updater can construct the body without taking a
/// direct storage dep. Field order + names match
/// `isengard_storage::host_action::UpdateApprovalBody` exactly so the storage
/// impl can `From`-convert in one shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PendingApprovalBody {
    /// Host the candidate container lives on.
    pub host_id: HostId,
    /// Compose project / stack name.
    pub stack: String,
    /// Service name within the stack.
    pub service: String,
    /// Docker container name.
    pub container_name: String,
    /// Image reference (with tag).
    pub image: String,
    /// Digest currently running.
    pub current_digest: String,
    /// Digest the updater wants to roll forward to.
    pub proposed_digest: String,
    /// Optional URL to a diff view of the change (e.g. GitHub compare URL).
    pub diff_url: Option<String>,
    /// Notifier channel id to route the approval prompt to.
    pub approver_channel: Option<String>,
}

/// Insert payload accepted by [`ApprovalStore::insert_pending_approval`].
#[derive(Debug, Clone)]
pub struct InsertPendingApproval {
    /// The body of the new row.
    pub body: PendingApprovalBody,
    /// When the row should auto-transition to `pending_expired` if no
    /// decision arrives.
    pub expires_at: DateTime<Utc>,
    /// Notifier channel id; redundant with `body.approver_channel` but kept
    /// at the insert level so the storage impl can index it without
    /// re-decoding the body.
    pub approver_channel: Option<String>,
}

/// Slim record returned by the trait.
///
/// Only `action_id` is needed at the updater seam (it goes into the emitted
/// `update.pending_approval` event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalRecord {
    /// Stable id of the persisted approval row (ULID).
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
    /// `(host_id, stack, service, proposed_digest)` tuple?
    ///
    /// Returns the most recent open row's record if any.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalStoreError`] wrapping whatever the underlying
    /// storage failed with. The updater treats every error the same:
    /// log and skip.
    async fn find_open_approval_for_proposed_digest(
        &self,
        host_id: HostId,
        stack: &str,
        service: &str,
        proposed_digest: &str,
    ) -> Result<Option<PendingApprovalRecord>, ApprovalStoreError>;

    /// Insert a brand-new pending-approval row in state `pending_open`.
    ///
    /// Returns the record so the caller can emit `update.pending_approval`.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalStoreError`] when the insert fails (uniqueness
    /// violation, DB unreachable, ...).
    async fn insert_pending_approval(
        &self,
        ins: InsertPendingApproval,
    ) -> Result<PendingApprovalRecord, ApprovalStoreError>;
}

/// Opaque error wrapper.
///
/// The updater treats every store error the same: log and skip the persist
/// step for this candidate (fail-safe: notifier won't fire, but the cycle
/// continues).
#[derive(Debug)]
pub struct ApprovalStoreError(
    /// Free-form error message captured from the underlying store.
    pub String,
);

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
