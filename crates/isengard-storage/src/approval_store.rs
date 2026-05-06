//! Production [`isengard_core::ApprovalStore`] implementation backed by an
//! `Arc<Inventory>`. Mirrors the [`crate::policy::InventoryPolicyLoader`]
//! pattern: the agent runtime constructs one of these and threads it through
//! `PluginContext` so the updater plugin can persist + lookup approval rows
//! without a hard storage dep.
//!
//! Type-shape note: the core trait deals in `isengard_core::PendingApprovalBody`
//! / `InsertPendingApproval` / `PendingApprovalRecord`. The storage DAO uses
//! its own fully-typed [`crate::host_action::UpdateApprovalBody`] +
//! [`crate::host_action::InsertPendingApproval`]. This module is the
//! conversion seam: each method maps core types -> storage types, calls the
//! existing DAO, then projects the storage row back to a slim
//! `PendingApprovalRecord` (just the `action_id`).
//!
//! The storage-side body's `host_id` field is the storage `HostId` newtype
//! over `ulid::Ulid`. Core's body uses `isengard_core::HostId` (a re-export of
//! `ulid::Ulid`), so the conversion is a single `From` hop.

use async_trait::async_trait;
use std::sync::Arc;

use isengard_core::approval_store::{
    ApprovalStore, ApprovalStoreError, InsertPendingApproval as CoreInsertPendingApproval,
    PendingApprovalBody as CorePendingApprovalBody, PendingApprovalRecord,
};

use crate::host_action::{
    InsertPendingApproval as StorageInsertPendingApproval,
    UpdateApprovalBody as StorageUpdateApprovalBody,
};
use crate::inventory::Inventory;

/// Production [`ApprovalStore`] backed by an `Arc<Inventory>`. Constructed
/// once by the agent runtime and shared across plugins.
pub struct InventoryApprovalStore {
    inv: Arc<Inventory>,
}

impl InventoryApprovalStore {
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self { inv }
    }
}

fn body_core_to_storage(body: CorePendingApprovalBody) -> StorageUpdateApprovalBody {
    StorageUpdateApprovalBody {
        host_id: crate::host::HostId(body.host_id),
        stack: body.stack,
        service: body.service,
        container_name: body.container_name,
        image: body.image,
        current_digest: body.current_digest,
        proposed_digest: body.proposed_digest,
        diff_url: body.diff_url,
        approver_channel: body.approver_channel,
    }
}

#[async_trait]
impl ApprovalStore for InventoryApprovalStore {
    async fn find_open_approval_for_proposed_digest(
        &self,
        host_id: isengard_core::HostId,
        stack: &str,
        service: &str,
        proposed_digest: &str,
    ) -> std::result::Result<Option<PendingApprovalRecord>, ApprovalStoreError> {
        let storage_host = crate::host::HostId(host_id);
        let row = self
            .inv
            .find_open_approval_for_proposed_digest(storage_host, stack, service, proposed_digest)
            .await
            .map_err(|e| {
                ApprovalStoreError(format!("find_open_approval_for_proposed_digest: {e}"))
            })?;
        Ok(row.map(|r| PendingApprovalRecord {
            action_id: r.action_id,
        }))
    }

    async fn insert_pending_approval(
        &self,
        ins: CoreInsertPendingApproval,
    ) -> std::result::Result<PendingApprovalRecord, ApprovalStoreError> {
        let storage_ins = StorageInsertPendingApproval {
            body: body_core_to_storage(ins.body),
            expires_at: ins.expires_at,
            approver_channel: ins.approver_channel,
        };
        let row = self
            .inv
            .insert_pending_approval(storage_ins)
            .await
            .map_err(|e| ApprovalStoreError(format!("insert_pending_approval: {e}")))?;
        Ok(PendingApprovalRecord {
            action_id: row.action_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};

    async fn fresh_inv() -> Arc<Inventory> {
        Arc::new(Inventory::open_in_memory().await.expect("open"))
    }

    async fn enroll(inv: &Inventory) -> crate::host::HostId {
        inv.enroll_host(crate::host::EnrollHost {
            fingerprint: "fp1".into(),
            hostname: "h1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            fleet: "default".into(),
        })
        .await
        .expect("enroll")
    }

    fn body(host: crate::host::HostId, digest: &str) -> CorePendingApprovalBody {
        CorePendingApprovalBody {
            host_id: host.0,
            stack: "blog".into(),
            service: "web".into(),
            container_name: "blog_web_1".into(),
            image: "ghcr.io/x/y".into(),
            current_digest: "sha256:cur".into(),
            proposed_digest: digest.into(),
            diff_url: None,
            approver_channel: None,
        }
    }

    #[tokio::test]
    async fn store_finds_no_row_when_empty() {
        let inv = fresh_inv().await;
        let host = enroll(&inv).await;
        let store = InventoryApprovalStore::new(inv);
        let r = store
            .find_open_approval_for_proposed_digest(host.0, "blog", "web", "sha256:abc")
            .await
            .expect("call");
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn store_inserts_then_finds_row() {
        let inv = fresh_inv().await;
        let host = enroll(&inv).await;
        let store = InventoryApprovalStore::new(inv);

        let rec = store
            .insert_pending_approval(CoreInsertPendingApproval {
                body: body(host, "sha256:new"),
                expires_at: Utc::now() + ChronoDuration::hours(24),
                approver_channel: Some("ops".into()),
            })
            .await
            .expect("insert");
        assert_eq!(rec.action_id.len(), 26, "ULID is 26 chars");

        let found = store
            .find_open_approval_for_proposed_digest(host.0, "blog", "web", "sha256:new")
            .await
            .expect("find")
            .expect("present");
        assert_eq!(found.action_id, rec.action_id);
    }
}
