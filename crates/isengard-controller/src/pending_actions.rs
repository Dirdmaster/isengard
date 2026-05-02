//! Collect pending host_actions for a host and convert them to wire-format
//! PendingActions for inclusion in the HeartbeatAck. Each action is marked
//! delivered as it's collected — we don't block the reply on agent acknowledgement.

use isengard_proto::pb::PendingAction as ProtoPendingAction;
use isengard_storage::{HostId, Inventory, Result};

pub async fn collect_pending_actions(
    inv: &Inventory,
    host_id: HostId,
) -> Result<Vec<ProtoPendingAction>> {
    let pending = inv.pending_actions(host_id).await?;
    let mut out = Vec::with_capacity(pending.len());
    for action in pending {
        out.push(ProtoPendingAction {
            id: action.id.0 as u64,
            kind: action.kind.kind_str().to_string(),
            payload_json: action.kind.payload_json(),
        });
        inv.mark_action_delivered(action.id, "dispatched").await?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::{EnrollHost, HostActionKind};

    #[tokio::test]
    async fn collect_attaches_and_marks_delivered() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
            fleet: "default".into(),
            })
            .await
            .unwrap();

        let action_id = inv
            .queue_action(host_id, HostActionKind::ForceUpdate { stack_name: None })
            .await
            .unwrap();

        let collected = collect_pending_actions(&inv, host_id).await.unwrap();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].id, action_id.0 as u64);
        assert_eq!(collected[0].kind, "force_update");

        // Should be empty on second call (already marked delivered).
        let still_pending = inv.pending_actions(host_id).await.unwrap();
        assert!(still_pending.is_empty());

        let collected_again = collect_pending_actions(&inv, host_id).await.unwrap();
        assert!(collected_again.is_empty());
    }
}
