//! Containers entity: one runtime-level container observed on a host.
//!
//! Phase 0.18: containers become the leaf unit for `isd ps`. Each row is
//! keyed by a 16-char hex digest of `sha256(host_id || "|" ||
//! runtime_container_id)` so the operator-visible id is globally unique
//! per fleet and stable across reconnects. The native runtime id (bollard
//! container id, wisp handle) is kept alongside in `runtime_container_id`.
//!
//! Lifecycle: the controller upserts one row per `ContainerInfo` carried
//! in a heartbeat (see [`upsert_container`]). When a container falls out
//! of the agent's snapshot the controller calls [`mark_containers_removed`]
//! to stamp `removed_at`. A janitor periodically calls
//! [`reap_removed_before`] to drop rows older than the retention window
//! (default 1h; see the controller's reaper).
//!
//! Filtering surface lives on [`ContainerListFilter`] passed to
//! [`list_containers`]. The fields mirror the dashboard's
//! `/api/v1/containers` query params.

use sqlx::{Row, SqlitePool};

use crate::error::Result;
use crate::host::HostId;

/// One row from the `containers` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRow {
    /// 16-char hex digest, stable across reconnects for the same
    /// `(host_id, runtime_container_id)` pair.
    pub id: String,
    pub host_id: HostId,
    /// Optional link to the per-host service row. Heartbeat ingest can
    /// leave this NULL when the agent has not yet derived a service
    /// name for the container.
    pub service_id: Option<i64>,
    /// Bollard / wisp native id. Operators rarely need this; it is kept
    /// so `isd container inspect` can round-trip through the backend.
    pub runtime_container_id: String,
    pub image: String,
    pub command: Option<String>,
    /// Lowercase string vocabulary the agent emits:
    /// `running`, `restarting`, `paused`, `created`, `exited`, `dead`,
    /// `removing`. The controller never writes `unknown`.
    pub state: String,
    /// Agent-rendered status (e.g. `Up 5m`, `Exited (0) 1h ago`).
    pub status_message: Option<String>,
    /// Comma-separated container names. Bollard yields one; wisp yields
    /// the single handle name.
    pub names: String,
    /// Denormalised stack / service names so list queries don't have to
    /// join through `service_id`.
    pub stack: Option<String>,
    pub service: Option<String>,
    /// Unix seconds when the runtime created the container. May be 0
    /// when the agent could not read it.
    pub created_at: Option<i64>,
    /// Unix seconds when the controller first observed this id.
    pub first_seen_at: i64,
    /// Unix seconds derived from `min(server_now, info.observed_at_ms /
    /// 1000)`. Tied to the agent's clock so we render a sensible LAST
    /// SEEN when the controller's clock has drifted.
    pub last_seen_at: i64,
    /// Unix seconds when the controller noticed the container was gone
    /// from the agent's snapshot. NULL while alive.
    pub removed_at: Option<i64>,
}

/// Query filter for [`list_containers`].
#[derive(Debug, Clone, Default)]
pub struct ContainerListFilter {
    pub host_id: Option<HostId>,
    pub stack: Option<String>,
    pub service: Option<String>,
    pub state: Option<String>,
    /// When false (default), rows with `removed_at IS NOT NULL` are hidden.
    pub include_removed: bool,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Upsert a container row keyed by `id`. On conflict, every mutable
/// field is overwritten with the supplied row but `first_seen_at` is
/// preserved (the SQL self-references the existing column on the UPDATE
/// branch). Use this for the heartbeat ingest path: one row per
/// `ContainerInfo` per heartbeat.
pub async fn upsert_container(pool: &SqlitePool, row: &ContainerRow) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO containers (
            id, host_id, service_id, runtime_container_id, image,
            command, state, status_message, names, stack, service,
            created_at, first_seen_at, last_seen_at, removed_at
        ) VALUES (
            ?, ?, ?, ?, ?,
            ?, ?, ?, ?, ?, ?,
            ?, ?, ?, ?
        )
        ON CONFLICT(id) DO UPDATE SET
            service_id           = excluded.service_id,
            runtime_container_id = excluded.runtime_container_id,
            image                = excluded.image,
            command              = excluded.command,
            state                = excluded.state,
            status_message       = excluded.status_message,
            names                = excluded.names,
            stack                = excluded.stack,
            service              = excluded.service,
            created_at           = excluded.created_at,
            last_seen_at         = excluded.last_seen_at,
            removed_at           = excluded.removed_at,
            first_seen_at        = containers.first_seen_at
        "#,
    )
    .bind(&row.id)
    .bind(row.host_id.to_bytes().as_slice())
    .bind(row.service_id)
    .bind(&row.runtime_container_id)
    .bind(&row.image)
    .bind(row.command.as_deref())
    .bind(&row.state)
    .bind(row.status_message.as_deref())
    .bind(&row.names)
    .bind(row.stack.as_deref())
    .bind(row.service.as_deref())
    .bind(row.created_at)
    .bind(row.first_seen_at)
    .bind(row.last_seen_at)
    .bind(row.removed_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark every container row for `host_id` whose `id` is NOT in
/// `alive_ids` and whose `removed_at IS NULL` as removed at `at`.
/// Returns the number of rows changed. No-op when `alive_ids` is empty
/// and there are no other rows for the host (the IN clause becomes a
/// degenerate `NOT IN ()` which SQLite evaluates as true, so we handle
/// the empty case by building the SQL without the `NOT IN` predicate).
pub async fn mark_containers_removed(
    pool: &SqlitePool,
    host_id: HostId,
    alive_ids: &[String],
    at: i64,
) -> Result<u64> {
    if alive_ids.is_empty() {
        // No containers reported alive: every row for this host becomes
        // removed. Skip the IN clause entirely.
        let result = sqlx::query(
            "UPDATE containers SET removed_at = ? \
             WHERE host_id = ? AND removed_at IS NULL",
        )
        .bind(at)
        .bind(host_id.to_bytes().as_slice())
        .execute(pool)
        .await?;
        return Ok(result.rows_affected());
    }

    // Build a parameterised IN clause: `?, ?, ?, ...` with N binds.
    let placeholders = std::iter::repeat_n("?", alive_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE containers SET removed_at = ? \
         WHERE host_id = ? AND removed_at IS NULL AND id NOT IN ({placeholders})"
    );
    let host_bytes = host_id.to_bytes().to_vec();
    let mut q = sqlx::query(&sql).bind(at).bind(host_bytes);
    for id in alive_ids {
        q = q.bind(id);
    }
    let result = q.execute(pool).await?;
    Ok(result.rows_affected())
}

/// List containers, applying every populated field in `filter`. Rows
/// are returned ordered by `last_seen_at DESC, id ASC` so the most
/// recent activity is at the top with a deterministic tie-break.
pub async fn list_containers(
    pool: &SqlitePool,
    filter: ContainerListFilter,
) -> Result<Vec<ContainerRow>> {
    let mut sql = String::from(
        "SELECT id, host_id, service_id, runtime_container_id, image, \
                command, state, status_message, names, stack, service, \
                created_at, first_seen_at, last_seen_at, removed_at \
         FROM containers WHERE 1=1",
    );
    if !filter.include_removed {
        sql.push_str(" AND removed_at IS NULL");
    }
    if filter.host_id.is_some() {
        sql.push_str(" AND host_id = ?");
    }
    if filter.stack.is_some() {
        sql.push_str(" AND stack = ?");
    }
    if filter.service.is_some() {
        sql.push_str(" AND service = ?");
    }
    if filter.state.is_some() {
        sql.push_str(" AND state = ?");
    }
    sql.push_str(" ORDER BY last_seen_at DESC, id ASC");
    if filter.limit.is_some() {
        sql.push_str(" LIMIT ?");
    }
    if filter.offset.is_some() {
        sql.push_str(" OFFSET ?");
    }

    let mut q = sqlx::query(&sql);
    if let Some(h) = filter.host_id {
        q = q.bind(h.to_bytes().to_vec());
    }
    if let Some(s) = filter.stack.as_deref() {
        q = q.bind(s.to_string());
    }
    if let Some(s) = filter.service.as_deref() {
        q = q.bind(s.to_string());
    }
    if let Some(s) = filter.state.as_deref() {
        q = q.bind(s.to_string());
    }
    if let Some(l) = filter.limit {
        q = q.bind(l);
    }
    if let Some(o) = filter.offset {
        q = q.bind(o);
    }
    let rows = q.fetch_all(pool).await?;
    rows.into_iter().map(decode_row).collect()
}

/// Fetch a single container by its operator-visible id (the 16-char hex
/// digest). Returns `Ok(None)` when no row matches.
pub async fn get_container(pool: &SqlitePool, id: &str) -> Result<Option<ContainerRow>> {
    let row = sqlx::query(
        "SELECT id, host_id, service_id, runtime_container_id, image, \
                command, state, status_message, names, stack, service, \
                created_at, first_seen_at, last_seen_at, removed_at \
         FROM containers WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(decode_row).transpose()
}

/// Janitor: delete rows whose `removed_at` is older than `cutoff`
/// (unix seconds). Returns the number of rows removed. Rows whose
/// `removed_at IS NULL` (live containers) are never touched.
pub async fn reap_removed_before(pool: &SqlitePool, cutoff: i64) -> Result<u64> {
    let result =
        sqlx::query("DELETE FROM containers WHERE removed_at IS NOT NULL AND removed_at < ?")
            .bind(cutoff)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

fn decode_row(row: sqlx::sqlite::SqliteRow) -> Result<ContainerRow> {
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    let host_id = HostId::from_db_bytes(host_bytes)?;
    Ok(ContainerRow {
        id: row.try_get("id")?,
        host_id,
        service_id: row.try_get("service_id")?,
        runtime_container_id: row.try_get("runtime_container_id")?,
        image: row.try_get("image")?,
        command: row.try_get("command")?,
        state: row.try_get("state")?,
        status_message: row.try_get("status_message")?,
        names: row.try_get("names")?,
        stack: row.try_get("stack")?,
        service: row.try_get("service")?,
        created_at: row.try_get("created_at")?,
        first_seen_at: row.try_get("first_seen_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        removed_at: row.try_get("removed_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::EnrollHost;
    use crate::inventory::Inventory;

    async fn setup_with_host() -> (Inventory, HostId) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-c1".into(),
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

    fn sample_row(host_id: HostId, id: &str, runtime_id: &str) -> ContainerRow {
        ContainerRow {
            id: id.into(),
            host_id,
            service_id: None,
            runtime_container_id: runtime_id.into(),
            image: "nginx:alpine".into(),
            command: Some("nginx -g 'daemon off;'".into()),
            state: "running".into(),
            status_message: Some("Up 5m".into()),
            names: "hello-web.1".into(),
            stack: Some("hello".into()),
            service: Some("web".into()),
            created_at: Some(1_700_000_000),
            first_seen_at: 1_700_000_100,
            last_seen_at: 1_700_000_200,
            removed_at: None,
        }
    }

    #[tokio::test]
    async fn upsert_then_upsert_preserves_first_seen_at() {
        let (inv, host_id) = setup_with_host().await;
        let pool = inv.pool();

        let row = sample_row(host_id, "a1b2c3d4e5f6a7b8", "runtime-1");
        upsert_container(pool, &row).await.unwrap();

        // Second upsert with a later last_seen_at and a different
        // first_seen_at value: the stored first_seen_at must NOT move.
        let mut row2 = row.clone();
        row2.first_seen_at = 1_700_999_999;
        row2.last_seen_at = 1_700_000_500;
        upsert_container(pool, &row2).await.unwrap();

        let got = get_container(pool, &row.id).await.unwrap().unwrap();
        assert_eq!(got.first_seen_at, 1_700_000_100);
        assert_eq!(got.last_seen_at, 1_700_000_500);
    }

    #[tokio::test]
    async fn second_upsert_updates_last_seen_at_and_state() {
        let (inv, host_id) = setup_with_host().await;
        let pool = inv.pool();

        let row = sample_row(host_id, "id-update-1", "runtime-1");
        upsert_container(pool, &row).await.unwrap();

        let mut row2 = row.clone();
        row2.last_seen_at = 1_700_000_999;
        row2.state = "exited".into();
        row2.status_message = Some("Exited (0) 1m ago".into());
        upsert_container(pool, &row2).await.unwrap();

        let got = get_container(pool, &row.id).await.unwrap().unwrap();
        assert_eq!(got.last_seen_at, 1_700_000_999);
        assert_eq!(got.state, "exited");
        assert_eq!(got.status_message.as_deref(), Some("Exited (0) 1m ago"));
    }

    #[tokio::test]
    async fn mark_containers_removed_marks_only_missing_ids() {
        let (inv, host_id) = setup_with_host().await;
        let pool = inv.pool();

        let alive = sample_row(host_id, "id-alive", "runtime-alive");
        let going = sample_row(host_id, "id-going", "runtime-going");
        upsert_container(pool, &alive).await.unwrap();
        upsert_container(pool, &going).await.unwrap();

        let changed =
            mark_containers_removed(pool, host_id, &["id-alive".to_string()], 1_700_000_300)
                .await
                .unwrap();
        assert_eq!(changed, 1);

        let still_here = get_container(pool, "id-alive").await.unwrap().unwrap();
        assert!(still_here.removed_at.is_none());
        let removed = get_container(pool, "id-going").await.unwrap().unwrap();
        assert_eq!(removed.removed_at, Some(1_700_000_300));

        // Second call is a no-op (already marked).
        let changed_again =
            mark_containers_removed(pool, host_id, &["id-alive".to_string()], 1_700_000_400)
                .await
                .unwrap();
        assert_eq!(changed_again, 0);
    }

    #[tokio::test]
    async fn list_containers_filters_by_host() {
        let (inv, host_a) = setup_with_host().await;
        let pool = inv.pool();
        let host_b = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-c2".into(),
                hostname: "h2".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
            })
            .await
            .unwrap();

        upsert_container(pool, &sample_row(host_a, "id-a", "rt-a"))
            .await
            .unwrap();
        upsert_container(pool, &sample_row(host_b, "id-b", "rt-b"))
            .await
            .unwrap();

        let only_a = list_containers(
            pool,
            ContainerListFilter {
                host_id: Some(host_a),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].id, "id-a");

        let everything = list_containers(pool, ContainerListFilter::default())
            .await
            .unwrap();
        assert_eq!(everything.len(), 2);
    }

    #[tokio::test]
    async fn list_containers_filters_by_stack() {
        let (inv, host_id) = setup_with_host().await;
        let pool = inv.pool();

        let mut hello = sample_row(host_id, "id-hello", "rt-hello");
        hello.stack = Some("hello".into());
        let mut other = sample_row(host_id, "id-other", "rt-other");
        other.stack = Some("other".into());
        upsert_container(pool, &hello).await.unwrap();
        upsert_container(pool, &other).await.unwrap();

        let only_hello = list_containers(
            pool,
            ContainerListFilter {
                stack: Some("hello".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_hello.len(), 1);
        assert_eq!(only_hello[0].stack.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn reap_removed_before_deletes_expired_rows_only() {
        let (inv, host_id) = setup_with_host().await;
        let pool = inv.pool();

        let mut old_removed = sample_row(host_id, "id-old", "rt-old");
        old_removed.removed_at = Some(100);
        let mut recent_removed = sample_row(host_id, "id-recent", "rt-recent");
        recent_removed.removed_at = Some(500);
        let alive = sample_row(host_id, "id-alive", "rt-alive");
        upsert_container(pool, &old_removed).await.unwrap();
        upsert_container(pool, &recent_removed).await.unwrap();
        upsert_container(pool, &alive).await.unwrap();

        let dropped = reap_removed_before(pool, 200).await.unwrap();
        assert_eq!(dropped, 1);

        // Old row gone; recent + alive still here.
        assert!(get_container(pool, "id-old").await.unwrap().is_none());
        assert!(get_container(pool, "id-recent").await.unwrap().is_some());
        assert!(get_container(pool, "id-alive").await.unwrap().is_some());
    }
}
