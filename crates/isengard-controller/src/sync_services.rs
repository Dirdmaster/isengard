//! Persist service info from agent heartbeats: upsert reported services and
//! prune any that are no longer reported for that host.

use std::collections::HashSet;

use isengard_proto::pb::ServiceInfo as ProtoServiceInfo;
use isengard_storage::{HostId, InsertService, Inventory, Result, ServiceState};

/// Apply a heartbeat's reported services to the inventory:
/// - Resolve stack_id from the per-host stacks table for each service that
///   declares a stack name.
/// - Upsert each reported service (idempotent per `(host_id, name)`).
/// - Delete any existing services for this host that the heartbeat no longer mentions.
pub async fn process_heartbeat_services(
    inv: &Inventory,
    host_id: HostId,
    services: &[ProtoServiceInfo],
) -> Result<()> {
    let mut current_names: HashSet<String> = HashSet::new();

    // Pre-fetch the host's stacks once so we don't requery for every service.
    let host_stacks = inv.list_stacks(Some(host_id)).await?;

    for s in services {
        let stack_id = s.stack.as_deref().and_then(|name| {
            host_stacks
                .iter()
                .find(|st| st.name == name)
                .map(|st| st.id)
        });

        inv.insert_service(InsertService {
            host_id,
            stack_id,
            name: s.name.clone(),
            image: s.image.clone(),
            state: ServiceState::from_str(&s.state),
        })
        .await?;
        current_names.insert(s.name.clone());
    }

    // Prune services for this host that are no longer reported.
    let existing = inv.list_services(None).await?;
    for sv in existing.into_iter().filter(|sv| sv.host_id == host_id) {
        if !current_names.contains(&sv.name) {
            inv.delete_service(sv.id).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::{EnrollHost, InsertStack, StackSource};

    #[tokio::test]
    async fn services_persisted_and_pruned() {
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

        // Pre-create a stack so the service can resolve its stack_id.
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        // First heartbeat: two services
        let services_v1 = vec![
            ProtoServiceInfo {
                name: "web".into(),
                image: "nginx".into(),
                state: "running".into(),
                stack: Some("blog".into()),
            },
            ProtoServiceInfo {
                name: "db".into(),
                image: "postgres".into(),
                state: "running".into(),
                stack: Some("blog".into()),
            },
        ];
        process_heartbeat_services(&inv, host_id, &services_v1)
            .await
            .unwrap();

        let stored = inv.list_services(None).await.unwrap();
        assert_eq!(stored.len(), 2);
        // Both services resolved their stack_id correctly.
        assert!(stored.iter().all(|sv| sv.stack_id == Some(stack_id)));

        // Second heartbeat: only one service — db should be pruned
        let services_v2 = vec![ProtoServiceInfo {
            name: "web".into(),
            image: "nginx:1.25-alpine".into(), // image change should propagate via upsert
            state: "running".into(),
            stack: Some("blog".into()),
        }];
        process_heartbeat_services(&inv, host_id, &services_v2)
            .await
            .unwrap();

        let stored = inv.list_services(None).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "web");
        assert_eq!(stored[0].image, "nginx:1.25-alpine");
    }
}
