//! Persist container info from agent heartbeats. Phase 0.18.
//!
//! For each `ContainerInfo` in the heartbeat we derive the operator-
//! visible id (see [`crate::container_id::derive_container_id`]) and
//! upsert a row into `containers`. After the loop, any rows that the
//! agent did NOT report (and that were previously alive for this host)
//! are marked removed at `server_now_seconds`.
//!
//! `last_seen_at` is clamped to `min(server_now_seconds,
//! observed_at_ms/1000)` so a wildly skewed agent clock can not push
//! the row's last-seen into the future.

use isengard_proto::pb::ContainerInfo as ProtoContainerInfo;
use isengard_storage::containers::{ContainerRow, mark_containers_removed, upsert_container};
use isengard_storage::host::HostId;
use sqlx::SqlitePool;

use crate::container_id::derive_container_id;

/// Apply a heartbeat's reported containers to the inventory:
/// 1. Upsert one row per `ContainerInfo`, deriving the operator id.
/// 2. Mark every row for this host that was NOT in the heartbeat as
///    removed at `server_now_seconds`.
///
/// Errors propagate as-is; callers log at the heartbeat boundary.
pub async fn process_heartbeat_containers(
    pool: &SqlitePool,
    host_id: HostId,
    containers: &[ProtoContainerInfo],
    server_now_seconds: i64,
) -> Result<(), isengard_storage::Error> {
    let host_display = host_id.to_string();
    let mut alive_ids: Vec<String> = Vec::with_capacity(containers.len());

    for info in containers {
        // Skip rows without a runtime id: the agent's derive_containers
        // already drops these, but defend against a future agent that
        // forgets the filter.
        if info.runtime_container_id.is_empty() {
            continue;
        }

        let id = derive_container_id(&host_display, &info.runtime_container_id);

        // Clamp last_seen_at to the smaller of server_now and the
        // agent's observed_at. Agents send observed_at_ms; convert to
        // seconds and floor at 0 in case the agent emitted a negative
        // value.
        let observed_seconds = (info.observed_at_ms / 1000).max(0);
        let last_seen_at = observed_seconds.min(server_now_seconds);

        // created_at: agent ships ms; convert to seconds, treat 0 as
        // unknown (NULL in storage).
        let created_at = if info.created_at_ms > 0 {
            Some(info.created_at_ms / 1000)
        } else {
            None
        };

        // Empty string fields collapse to None at the storage boundary
        // so the DB stays NULL rather than the empty-string sentinel.
        let row = ContainerRow {
            id: id.clone(),
            host_id,
            service_id: None,
            runtime_container_id: info.runtime_container_id.clone(),
            image: info.image.clone(),
            command: opt_string(&info.command),
            state: info.state.clone(),
            status_message: opt_string(&info.status_message),
            names: info.names.clone(),
            stack: opt_string(&info.stack),
            service: opt_string(&info.service),
            created_at,
            // first_seen_at: only used on INSERT. The DAO's upsert
            // preserves the existing value on UPDATE, so any sentinel
            // we pass here is fine on the conflict path. Use
            // server_now_seconds so newly inserted rows are stamped
            // with the controller's clock.
            first_seen_at: server_now_seconds,
            last_seen_at,
            removed_at: None,
        };

        upsert_container(pool, &row).await?;
        alive_ids.push(id);
    }

    mark_containers_removed(pool, host_id, &alive_ids, server_now_seconds).await?;
    Ok(())
}

fn opt_string(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::containers::{ContainerListFilter, get_container, list_containers};
    use isengard_storage::host::EnrollHost;
    use isengard_storage::inventory::Inventory;

    async fn setup() -> (Inventory, HostId) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-sync-containers".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
            })
            .await
            .unwrap();
        (inv, host_id)
    }

    fn sample(runtime_id: &str, state: &str) -> ProtoContainerInfo {
        ProtoContainerInfo {
            runtime_container_id: runtime_id.into(),
            image: "nginx:alpine".into(),
            command: "nginx -g 'daemon off;'".into(),
            state: state.into(),
            status_message: "Up 5m".into(),
            names: format!("{runtime_id}-name"),
            stack: "hello".into(),
            service: "web".into(),
            created_at_ms: 1_700_000_000_000,
            observed_at_ms: 1_700_000_300_000,
        }
    }

    /// Phase 0.18: first heartbeat upserts N containers; second
    /// heartbeat with M < N marks the missing (N - M) as removed.
    /// Survivors keep their first_seen_at across the two ingest calls.
    #[tokio::test]
    async fn ingest_two_then_one_marks_missing_removed() {
        let (inv, host_id) = setup().await;
        let pool = inv.pool();

        let containers_v1 = vec![sample("rt-a", "running"), sample("rt-b", "running")];
        process_heartbeat_containers(pool, host_id, &containers_v1, 1_700_000_300)
            .await
            .unwrap();

        let listed = list_containers(pool, ContainerListFilter::default())
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);

        // Second heartbeat: only rt-a remains.
        let containers_v2 = vec![sample("rt-a", "running")];
        process_heartbeat_containers(pool, host_id, &containers_v2, 1_700_000_600)
            .await
            .unwrap();

        // rt-a still alive; rt-b carries a removed_at stamp.
        let host_display = host_id.to_string();
        let id_a = derive_container_id(&host_display, "rt-a");
        let id_b = derive_container_id(&host_display, "rt-b");
        let row_a = get_container(pool, &id_a).await.unwrap().unwrap();
        assert!(row_a.removed_at.is_none());
        // first_seen_at locked at the first ingest's server clock.
        assert_eq!(row_a.first_seen_at, 1_700_000_300);

        let row_b = get_container(pool, &id_b).await.unwrap().unwrap();
        assert_eq!(row_b.removed_at, Some(1_700_000_600));
    }

    /// Phase 0.18: last_seen_at is min(server_now, observed_at_ms/1000).
    /// A future-shifted agent clock can't push the row's last-seen past
    /// the controller's clock.
    #[tokio::test]
    async fn last_seen_clamps_to_server_now_when_agent_clock_is_future() {
        let (inv, host_id) = setup().await;
        let pool = inv.pool();

        let mut info = sample("rt-future", "running");
        // Agent thinks it's 30 minutes ahead of the controller.
        info.observed_at_ms = 1_700_001_800_000;
        process_heartbeat_containers(pool, host_id, &[info], 1_700_000_300)
            .await
            .unwrap();

        let host_display = host_id.to_string();
        let id = derive_container_id(&host_display, "rt-future");
        let row = get_container(pool, &id).await.unwrap().unwrap();
        assert_eq!(row.last_seen_at, 1_700_000_300);
    }
}
