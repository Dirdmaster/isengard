//! Persist stack info from agent heartbeats: upsert reported stacks and
//! prune any that are no longer reported for that host.

use std::collections::HashSet;

use isengard_proto::pb::StackInfo as ProtoStackInfo;
use isengard_storage::{HostId, InsertStack, Inventory, Result, StackSource};

/// Apply a heartbeat's reported stacks to the inventory:
/// - Upsert each reported stack (idempotent per `(host_id, name)`).
/// - Delete any existing stacks for this host that the heartbeat no longer mentions.
pub async fn process_heartbeat_stacks(
    inv: &Inventory,
    host_id: HostId,
    stacks: &[ProtoStackInfo],
) -> Result<()> {
    let mut current_names: HashSet<String> = HashSet::new();
    for s in stacks {
        let source = StackSource::from_str(&s.source).unwrap_or(StackSource::Inferred);
        inv.insert_stack(InsertStack {
            host_id,
            name: s.name.clone(),
            source,
        })
        .await?;
        current_names.insert(s.name.clone());
    }

    let existing = inv.list_stacks(Some(host_id)).await?;
    for stack in existing {
        if !current_names.contains(&stack.name) {
            inv.delete_stack(stack.id).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::EnrollHost;

    #[tokio::test]
    async fn heartbeat_with_stacks_upserts_and_prunes() {
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

        // First heartbeat: two stacks
        let stacks_v1 = vec![
            ProtoStackInfo {
                name: "wordpress".into(),
                source: "compose".into(),
                services: vec!["web".into(), "db".into()],
            },
            ProtoStackInfo {
                name: "homer".into(),
                source: "inferred".into(),
                services: vec!["homer".into()],
            },
        ];
        process_heartbeat_stacks(&inv, host_id, &stacks_v1)
            .await
            .unwrap();

        let stored = inv.list_stacks(Some(host_id)).await.unwrap();
        assert_eq!(stored.len(), 2);

        // Second heartbeat: only one stack — homer should be pruned
        let stacks_v2 = vec![ProtoStackInfo {
            name: "wordpress".into(),
            source: "compose".into(),
            services: vec!["web".into(), "db".into()],
        }];
        process_heartbeat_stacks(&inv, host_id, &stacks_v2)
            .await
            .unwrap();

        let stored = inv.list_stacks(Some(host_id)).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "wordpress");
    }
}
