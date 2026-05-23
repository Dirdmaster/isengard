//! Rich container detail rows: ports, env, mounts, networks, restart,
//! command, entrypoint, working_dir, user, healthcheck.
//!
//! Migration `0033_containers_rich` lands the `containers_rich` table.
//! Heartbeat ingest in `isengard-controller::sync_containers` upserts
//! one row per `ContainerInfo` whose `rich` block is populated; older
//! agents (no rich block) skip the upsert entirely.
//!
//! Data shape: each list-shaped column rides as JSON-encoded TEXT
//! (`ports_json`, `env_json`, `mounts_json`, `networks_json`,
//! `command_json`, `entrypoint_json`, `healthcheck_json`). The
//! synthesizer in the parallel compose-synthesize PR deserialises the
//! whole row per container; we never index inside the JSON.
//!
//! Scalar columns (`restart_policy`, `working_dir`, `user_spec`) stay
//! plain TEXT for cheap operator filtering.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::error::{Error, Result};
use crate::host::HostId;

/// One row from `containers_rich`. JSON columns are decoded into
/// typed Vec / Option values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRichRow {
    /// 16-char hex digest container id (matches `containers.id`).
    pub container_id: String,
    /// Host the container runs on (matches `containers.host_id`).
    pub host_id: HostId,
    /// Host -> container port mappings.
    pub ports: Vec<RichPortMapping>,
    /// `KEY=value` environment entries.
    pub env: Vec<String>,
    /// Bind / volume / tmpfs mounts.
    pub mounts: Vec<RichMount>,
    /// Network names the container is attached to.
    pub networks: Vec<String>,
    /// Effective restart policy string. `None` when the runtime didn't
    /// record one.
    pub restart_policy: Option<String>,
    /// `cmd` override. `None` keeps the image-baked command.
    pub command: Option<Vec<String>>,
    /// `entrypoint` override. `None` keeps the image-baked entrypoint.
    pub entrypoint: Option<Vec<String>>,
    /// Container working directory override.
    pub working_dir: Option<String>,
    /// `USER` override.
    pub user_spec: Option<String>,
    /// Healthcheck declaration, when the container has one.
    pub healthcheck: Option<RichHealthcheck>,
}

/// One host:container port binding plus transport protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichPortMapping {
    /// Host interface IP. Empty string when bound to all interfaces.
    pub host_ip: String,
    /// Host port number.
    pub host_port: u16,
    /// Container port number.
    pub container_port: u16,
    /// Transport protocol: `tcp`, `udp`, `sctp`.
    pub protocol: String,
}

/// One mount entry attached to a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichMount {
    /// Mount kind: `bind`, `volume`, `tmpfs`, `npipe`, `cluster`.
    pub kind: String,
    /// Host-side path (bind), volume name (volume), or empty (tmpfs).
    pub source: String,
    /// In-container target path.
    pub target: String,
    /// Read-only when true.
    pub read_only: bool,
}

/// Container healthcheck. Durations are nanoseconds, matching the
/// docker inspect format. Zero means "use the image default".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichHealthcheck {
    /// Command argv. First element is typically `CMD` or `CMD-SHELL`.
    pub test: Vec<String>,
    /// Interval between probes in nanoseconds.
    pub interval_ns: i64,
    /// Per-probe timeout in nanoseconds.
    pub timeout_ns: i64,
    /// Consecutive failures before unhealthy.
    pub retries: i64,
    /// Start grace period in nanoseconds.
    pub start_period_ns: i64,
}

/// Upsert a rich container row keyed by `container_id`. Overwrites every
/// column on conflict so the row always reflects the most recent
/// heartbeat. `updated_at` is stamped by the SQL `excluded` clause via
/// `strftime` so SQLite supplies a monotonic UTC timestamp.
pub async fn upsert_container_rich(pool: &SqlitePool, row: &ContainerRichRow) -> Result<()> {
    let ports_json = serde_json::to_string(&row.ports).map_err(|e| Error::Decode {
        reason: e.to_string(),
    })?;
    let env_json = serde_json::to_string(&row.env).map_err(|e| Error::Decode {
        reason: e.to_string(),
    })?;
    let mounts_json = serde_json::to_string(&row.mounts).map_err(|e| Error::Decode {
        reason: e.to_string(),
    })?;
    let networks_json = serde_json::to_string(&row.networks).map_err(|e| Error::Decode {
        reason: e.to_string(),
    })?;
    let command_json = row
        .command
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
    let entrypoint_json = row
        .entrypoint
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
    let healthcheck_json = row
        .healthcheck
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;

    sqlx::query(
        r#"
        INSERT INTO containers_rich (
            container_id, host_id,
            ports_json, env_json, mounts_json, networks_json,
            restart_policy, command_json, entrypoint_json,
            working_dir, user_spec, healthcheck_json,
            updated_at
        ) VALUES (
            ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?,
            ?, ?, ?,
            strftime('%Y-%m-%d %H:%M:%f', 'now')
        )
        ON CONFLICT(container_id) DO UPDATE SET
            host_id          = excluded.host_id,
            ports_json       = excluded.ports_json,
            env_json         = excluded.env_json,
            mounts_json      = excluded.mounts_json,
            networks_json    = excluded.networks_json,
            restart_policy   = excluded.restart_policy,
            command_json     = excluded.command_json,
            entrypoint_json  = excluded.entrypoint_json,
            working_dir      = excluded.working_dir,
            user_spec        = excluded.user_spec,
            healthcheck_json = excluded.healthcheck_json,
            updated_at       = strftime('%Y-%m-%d %H:%M:%f', 'now')
        "#,
    )
    .bind(&row.container_id)
    .bind(row.host_id.to_bytes().as_slice())
    .bind(&ports_json)
    .bind(&env_json)
    .bind(&mounts_json)
    .bind(&networks_json)
    .bind(row.restart_policy.as_deref())
    .bind(command_json.as_deref())
    .bind(entrypoint_json.as_deref())
    .bind(row.working_dir.as_deref())
    .bind(row.user_spec.as_deref())
    .bind(healthcheck_json.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch the rich row for one container by its operator-visible id.
/// Returns `Ok(None)` when no row matches (older agent, or container
/// row was reaped before the rich block landed).
pub async fn get_container_rich(
    pool: &SqlitePool,
    container_id: &str,
) -> Result<Option<ContainerRichRow>> {
    let row = sqlx::query(
        "SELECT container_id, host_id, \
                ports_json, env_json, mounts_json, networks_json, \
                restart_policy, command_json, entrypoint_json, \
                working_dir, user_spec, healthcheck_json \
         FROM containers_rich WHERE container_id = ?",
    )
    .bind(container_id)
    .fetch_optional(pool)
    .await?;
    row.map(decode_row).transpose()
}

/// List every rich row for one host. Used by the compose-synthesize
/// endpoint (the parallel PR) to fetch every container that belongs to
/// a stack and stitch together the YAML.
pub async fn list_container_rich_for_host(
    pool: &SqlitePool,
    host_id: HostId,
) -> Result<Vec<ContainerRichRow>> {
    let rows = sqlx::query(
        "SELECT container_id, host_id, \
                ports_json, env_json, mounts_json, networks_json, \
                restart_policy, command_json, entrypoint_json, \
                working_dir, user_spec, healthcheck_json \
         FROM containers_rich WHERE host_id = ?",
    )
    .bind(host_id.to_bytes().as_slice())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode_row).collect()
}

/// Decode one `containers_rich` row, deserialising every JSON column.
fn decode_row(row: sqlx::sqlite::SqliteRow) -> Result<ContainerRichRow> {
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    let host_id = HostId::from_db_bytes(host_bytes)?;
    let ports_json: String = row.try_get("ports_json")?;
    let env_json: String = row.try_get("env_json")?;
    let mounts_json: String = row.try_get("mounts_json")?;
    let networks_json: String = row.try_get("networks_json")?;
    let command_json: Option<String> = row.try_get("command_json")?;
    let entrypoint_json: Option<String> = row.try_get("entrypoint_json")?;
    let healthcheck_json: Option<String> = row.try_get("healthcheck_json")?;

    let ports: Vec<RichPortMapping> =
        serde_json::from_str(&ports_json).map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
    let env: Vec<String> = serde_json::from_str(&env_json).map_err(|e| Error::Decode {
        reason: e.to_string(),
    })?;
    let mounts: Vec<RichMount> = serde_json::from_str(&mounts_json).map_err(|e| Error::Decode {
        reason: e.to_string(),
    })?;
    let networks: Vec<String> =
        serde_json::from_str(&networks_json).map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
    let command = command_json
        .as_deref()
        .map(serde_json::from_str::<Vec<String>>)
        .transpose()
        .map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
    let entrypoint = entrypoint_json
        .as_deref()
        .map(serde_json::from_str::<Vec<String>>)
        .transpose()
        .map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;
    let healthcheck = healthcheck_json
        .as_deref()
        .map(serde_json::from_str::<RichHealthcheck>)
        .transpose()
        .map_err(|e| Error::Decode {
            reason: e.to_string(),
        })?;

    Ok(ContainerRichRow {
        container_id: row.try_get("container_id")?,
        host_id,
        ports,
        env,
        mounts,
        networks,
        restart_policy: row.try_get("restart_policy")?,
        command,
        entrypoint,
        working_dir: row.try_get("working_dir")?,
        user_spec: row.try_get("user_spec")?,
        healthcheck,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::{ContainerRow, upsert_container};
    use crate::host::EnrollHost;
    use crate::inventory::Inventory;

    async fn setup_with_host_and_container() -> (Inventory, HostId, String) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-rich-1".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.6.0".into(),
                docker_version: "27.0".into(),
            })
            .await
            .unwrap();
        // containers_rich.container_id FK references containers(id), so
        // we need an actual container row before we can upsert a rich
        // row.
        let cid = "a1b2c3d4e5f6a7b8".to_string();
        upsert_container(
            inv.pool(),
            &ContainerRow {
                id: cid.clone(),
                host_id,
                service_id: None,
                runtime_container_id: "rt-1".into(),
                image: "nginx:alpine".into(),
                command: None,
                state: "running".into(),
                status_message: None,
                names: "hello-web.1".into(),
                stack: Some("hello".into()),
                service: Some("web".into()),
                created_at: None,
                first_seen_at: 1_700_000_000,
                last_seen_at: 1_700_000_300,
                removed_at: None,
            },
        )
        .await
        .unwrap();
        (inv, host_id, cid)
    }

    fn sample_row(container_id: &str, host_id: HostId) -> ContainerRichRow {
        ContainerRichRow {
            container_id: container_id.to_string(),
            host_id,
            ports: vec![
                RichPortMapping {
                    host_ip: "0.0.0.0".into(),
                    host_port: 8080,
                    container_port: 80,
                    protocol: "tcp".into(),
                },
                RichPortMapping {
                    host_ip: "127.0.0.1".into(),
                    host_port: 5353,
                    container_port: 53,
                    protocol: "udp".into(),
                },
            ],
            env: vec!["FOO=bar".into(), "BAZ=qux".into()],
            mounts: vec![
                RichMount {
                    kind: "bind".into(),
                    source: "/host/data".into(),
                    target: "/data".into(),
                    read_only: true,
                },
                RichMount {
                    kind: "volume".into(),
                    source: "dbvol".into(),
                    target: "/var/lib/mysql".into(),
                    read_only: false,
                },
            ],
            networks: vec!["frontend".into(), "backend".into()],
            restart_policy: Some("on-failure:5".into()),
            command: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
            entrypoint: Some(vec!["/docker-entrypoint.sh".into()]),
            working_dir: Some("/srv".into()),
            user_spec: Some("nginx".into()),
            healthcheck: Some(RichHealthcheck {
                test: vec!["CMD".into(), "curl".into(), "-f".into(), "/".into()],
                interval_ns: 30_000_000_000,
                timeout_ns: 5_000_000_000,
                retries: 3,
                start_period_ns: 10_000_000_000,
            }),
        }
    }

    /// Insert a fully-populated rich row, read it back via
    /// `get_container_rich`, assert every JSON-shaped column survives
    /// the round-trip intact.
    #[tokio::test]
    async fn upsert_then_read_round_trips_every_field() {
        let (inv, host_id, cid) = setup_with_host_and_container().await;
        let pool = inv.pool();
        let row = sample_row(&cid, host_id);
        upsert_container_rich(pool, &row).await.unwrap();
        let got = get_container_rich(pool, &cid).await.unwrap().unwrap();
        assert_eq!(got, row);
    }

    /// Second upsert with new values overwrites the previous row. The
    /// `ports` list of the second insert wins; nothing from the first
    /// insert leaks through (no merging semantics).
    #[tokio::test]
    async fn second_upsert_overwrites_every_column() {
        let (inv, host_id, cid) = setup_with_host_and_container().await;
        let pool = inv.pool();
        let row1 = sample_row(&cid, host_id);
        upsert_container_rich(pool, &row1).await.unwrap();

        let mut row2 = row1.clone();
        row2.ports = vec![RichPortMapping {
            host_ip: "::".into(),
            host_port: 443,
            container_port: 8443,
            protocol: "tcp".into(),
        }];
        row2.env = vec!["NEW=value".into()];
        row2.restart_policy = Some("always".into());
        upsert_container_rich(pool, &row2).await.unwrap();

        let got = get_container_rich(pool, &cid).await.unwrap().unwrap();
        assert_eq!(got.ports.len(), 1);
        assert_eq!(got.ports[0].host_port, 443);
        assert_eq!(got.env, vec!["NEW=value".to_string()]);
        assert_eq!(got.restart_policy.as_deref(), Some("always"));
    }

    /// Optional fields encode + decode as NULL/None. A minimal row with
    /// every Option `None` and every Vec empty round-trips.
    #[tokio::test]
    async fn round_trip_with_optional_fields_unset() {
        let (inv, host_id, cid) = setup_with_host_and_container().await;
        let pool = inv.pool();
        let bare = ContainerRichRow {
            container_id: cid.clone(),
            host_id,
            ports: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
            restart_policy: None,
            command: None,
            entrypoint: None,
            working_dir: None,
            user_spec: None,
            healthcheck: None,
        };
        upsert_container_rich(pool, &bare).await.unwrap();
        let got = get_container_rich(pool, &cid).await.unwrap().unwrap();
        assert_eq!(got, bare);
    }

    /// Deleting the parent `containers` row cascades to the rich row.
    /// Locks the FK so we don't accumulate orphan rich rows.
    #[tokio::test]
    async fn delete_parent_cascades_to_rich_row() {
        let (inv, host_id, cid) = setup_with_host_and_container().await;
        let pool = inv.pool();
        upsert_container_rich(pool, &sample_row(&cid, host_id))
            .await
            .unwrap();
        assert!(get_container_rich(pool, &cid).await.unwrap().is_some());

        sqlx::query("DELETE FROM containers WHERE id = ?")
            .bind(&cid)
            .execute(pool)
            .await
            .unwrap();

        assert!(get_container_rich(pool, &cid).await.unwrap().is_none());
    }

    /// `list_container_rich_for_host` returns every rich row attached
    /// to the supplied host and skips rows from other hosts.
    #[tokio::test]
    async fn list_for_host_filters_by_host_id() {
        let (inv, host_a, cid_a) = setup_with_host_and_container().await;
        let pool = inv.pool();
        upsert_container_rich(pool, &sample_row(&cid_a, host_a))
            .await
            .unwrap();

        let host_b = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-rich-2".into(),
                hostname: "h2".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.6.0".into(),
                docker_version: "27.0".into(),
            })
            .await
            .unwrap();
        let cid_b = "00112233aabbccdd".to_string();
        upsert_container(
            pool,
            &ContainerRow {
                id: cid_b.clone(),
                host_id: host_b,
                service_id: None,
                runtime_container_id: "rt-b".into(),
                image: "nginx:alpine".into(),
                command: None,
                state: "running".into(),
                status_message: None,
                names: "other".into(),
                stack: None,
                service: None,
                created_at: None,
                first_seen_at: 1_700_000_000,
                last_seen_at: 1_700_000_300,
                removed_at: None,
            },
        )
        .await
        .unwrap();
        upsert_container_rich(pool, &sample_row(&cid_b, host_b))
            .await
            .unwrap();

        let listed_a = list_container_rich_for_host(pool, host_a).await.unwrap();
        assert_eq!(listed_a.len(), 1);
        assert_eq!(listed_a[0].container_id, cid_a);

        let listed_b = list_container_rich_for_host(pool, host_b).await.unwrap();
        assert_eq!(listed_b.len(), 1);
        assert_eq!(listed_b[0].container_id, cid_b);
    }
}
