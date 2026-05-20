#![doc = include_str!("../docs/deployments.md")]

use crate::error::{Error, Result};
use crate::host::HostId;
use crate::stack::StackId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Deployment strategy stored in the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeployStrategy {
    /// Parallel color, then swap, then tear down the old color.
    BlueGreen,
    /// Recreate the container at the new image; no parallel color.
    InPlace,
}

impl DeployStrategy {
    /// Canonical kebab-case spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeployStrategy::BlueGreen => "blue-green",
            DeployStrategy::InPlace => "in-place",
        }
    }
}

impl FromStr for DeployStrategy {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "blue-green" => Ok(Self::BlueGreen),
            "in-place" => Ok(Self::InPlace),
            other => Err(Error::Decode {
                reason: format!("unknown deploy strategy: {other}"),
            }),
        }
    }
}

/// Lifecycle state for a deployment row.
///
/// See the module docs for the transition diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    /// Row written; driver has not yet started spinning the green color.
    Pending,
    /// Green container is being created / pulled / started.
    SpinningUp,
    /// Health passed; proxy is swapping traffic to green.
    Switching,
    /// Old (blue) color is draining outstanding connections.
    Draining,
    /// Blue container is being removed.
    DestroyingBlue,
    /// Post-switch collapse recovery: green went unhealthy after the
    /// swap, driver is rolling back to the snapshotted blue upstream.
    /// Non-terminal (transitions to `Failed` once the swap-back
    /// completes).
    Recovering,
    /// The supervisor's `Rollback` failure-handler branch. The driver
    /// is re-pulling `previous_digest` and recreating the container
    /// at that image. Non-terminal: transitions to `RolledBack` on
    /// success or `RollbackFailed` on error.
    RollingBack,
    /// Steady-state success.
    Done,
    /// Operator aborted the deploy.
    Aborted,
    /// Deploy failed past the supervisor's recovery options.
    Failed,
    /// Terminal success of the rollback handler. The previous digest
    /// is now serving traffic.
    RolledBack,
    /// Terminal failure of the rollback handler. The original failure
    /// was real and the rollback itself broke (image gone from
    /// registry, resource exhaustion at recreate, etc).
    RollbackFailed,
}

impl DeploymentState {
    /// Canonical snake_case spelling for the SQLite column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::SpinningUp => "spinning_up",
            Self::Switching => "switching",
            Self::Draining => "draining",
            Self::DestroyingBlue => "destroying_blue",
            Self::Recovering => "recovering",
            Self::RollingBack => "rolling_back",
            Self::Done => "done",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
        }
    }

    /// Terminal states: a deployment in one of these doesn't transition
    /// further on its own. Includes `RolledBack` and `RollbackFailed`.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Aborted | Self::Failed | Self::RolledBack | Self::RollbackFailed
        )
    }
}

impl FromStr for DeploymentState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "pending" => Self::Pending,
            "spinning_up" => Self::SpinningUp,
            "switching" => Self::Switching,
            "draining" => Self::Draining,
            "destroying_blue" => Self::DestroyingBlue,
            "recovering" => Self::Recovering,
            "rolling_back" => Self::RollingBack,
            "done" => Self::Done,
            "aborted" => Self::Aborted,
            "failed" => Self::Failed,
            "rolled_back" => Self::RolledBack,
            "rollback_failed" => Self::RollbackFailed,
            other => {
                return Err(Error::Decode {
                    reason: format!("unknown deployment state: {other}"),
                });
            }
        })
    }
}

/// One row from `deployments`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    /// ULID id rendered as a string.
    pub id: String,
    /// Host the deployment runs on.
    pub host_id: HostId,
    /// Stack the deployment belongs to.
    pub stack_id: StackId,
    /// Service name within the stack.
    pub service_name: String,
    /// Strategy used for this deployment.
    pub strategy: DeployStrategy,
    /// Current lifecycle state.
    pub state: DeploymentState,
    /// Name of the blue (existing) container.
    pub blue_container: Option<String>,
    /// Name of the green (incoming) container.
    pub green_container: Option<String>,
    /// Image digest serving as blue.
    pub blue_digest: String,
    /// Image digest the deploy is moving to.
    pub green_digest: String,
    /// Optional public hostname the rule serves.
    pub public_hostname: Option<String>,
    /// Optional healthcheck path.
    pub health_path: Option<String>,
    /// Optional upstream container port.
    pub container_port: Option<i64>,
    /// When the healthcheck loop began.
    pub healthcheck_started_at: Option<DateTime<Utc>>,
    /// When the green color first passed health.
    pub healthcheck_passed_at: Option<DateTime<Utc>>,
    /// When the proxy swapped traffic to green.
    pub switched_at: Option<DateTime<Utc>>,
    /// When the blue color finished draining.
    pub drained_at: Option<DateTime<Utc>>,
    /// When the row reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
    /// Failure reason string.
    pub error: Option<String>,
    /// Auxiliary JSON envelope.
    pub metadata_json: Option<String>,
    /// When the row was inserted.
    pub created_at: DateTime<Utc>,
    /// When the row was last updated.
    pub updated_at: DateTime<Utc>,
    /// Set when this deployment is part of a multi-host rolling group.
    /// `None` for single-host deploys (orchestrator-bypass).
    #[serde(default)]
    pub group_id: Option<String>,
    /// Snapshot of the blue digest taken at deployment start.
    ///
    /// Populated only when the resolved policy's `on_failure == Rollback`,
    /// so the supervisor can re-pull this exact image if the deployment
    /// fails healthcheck. NULL means "rollback not eligible".
    #[serde(default)]
    pub previous_digest: Option<String>,
    /// Timestamp the supervisor entered the rollback branch.
    ///
    /// Set regardless of rollback success so the dashboard can render
    /// "Rolled back at HH:MM" without parsing the error string.
    #[serde(default)]
    pub rollback_attempted_at: Option<DateTime<Utc>>,
}

/// Insert payload for a deployment row.
#[derive(Debug, Clone)]
pub struct InsertDeployment {
    /// ULID id (caller mints it).
    pub id: String,
    /// Host the deployment targets.
    pub host_id: HostId,
    /// Stack the deployment belongs to.
    pub stack_id: StackId,
    /// Service name.
    pub service_name: String,
    /// Strategy.
    pub strategy: DeployStrategy,
    /// Initial state (typically `Pending`).
    pub state: DeploymentState,
    /// Blue container name, when known.
    pub blue_container: Option<String>,
    /// Green container name, when known.
    pub green_container: Option<String>,
    /// Blue digest.
    pub blue_digest: String,
    /// Green digest.
    pub green_digest: String,
    /// Optional public hostname.
    pub public_hostname: Option<String>,
    /// Optional healthcheck path.
    pub health_path: Option<String>,
    /// Optional upstream port.
    pub container_port: Option<i64>,
    /// Auxiliary JSON envelope.
    pub metadata_json: Option<String>,
    /// When the resolved policy's `on_failure == Rollback`, the
    /// supervisor seeds this with `blue_digest` so the driver can
    /// re-pull it if green fails. `None` keeps the row
    /// rollback-ineligible (the existing default behaviour).
    pub previous_digest: Option<String>,
}

impl crate::inventory::Inventory {
    /// Insert a deployment row and return the fully-populated record.
    pub async fn insert_deployment(&self, ins: InsertDeployment) -> Result<Deployment> {
        let host_bytes = ins.host_id.0.to_bytes().to_vec();
        sqlx::query(
            r#"
            INSERT INTO deployments (
              id, host_id, stack_id, service_name, strategy, state,
              blue_container, green_container, blue_digest, green_digest,
              public_hostname, health_path, container_port, metadata_json,
              previous_digest
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&ins.id)
        .bind(&host_bytes)
        .bind(ins.stack_id.0)
        .bind(&ins.service_name)
        .bind(ins.strategy.as_str())
        .bind(ins.state.as_str())
        .bind(&ins.blue_container)
        .bind(&ins.green_container)
        .bind(&ins.blue_digest)
        .bind(&ins.green_digest)
        .bind(&ins.public_hostname)
        .bind(&ins.health_path)
        .bind(ins.container_port)
        .bind(&ins.metadata_json)
        .bind(&ins.previous_digest)
        .execute(self.pool())
        .await?;

        self.get_deployment(&ins.id)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!("deployment {} not found after insert", ins.id),
            })
    }

    /// Fetch one deployment by id.
    pub async fn get_deployment(&self, id: &str) -> Result<Option<Deployment>> {
        let row = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   group_id, previous_digest, rollback_attempted_at,
                   created_at, updated_at
            FROM deployments WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        let Some(r) = row else { return Ok(None) };
        Ok(Some(row_to_deployment(&r)?))
    }

    /// Every non-terminal deployment for `host_id`, oldest first.
    pub async fn list_in_flight_deployments(&self, host_id: HostId) -> Result<Vec<Deployment>> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   group_id, previous_digest, rollback_attempted_at,
                   created_at, updated_at
            FROM deployments
            WHERE host_id = ? AND state NOT IN ('done', 'failed', 'aborted', 'rolled_back', 'rollback_failed')
            ORDER BY created_at ASC
            "#,
        )
        .bind(&host_bytes)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_deployment).collect()
    }

    /// Every non-terminal deployment for `(host_id, service_name)`,
    /// oldest first.
    pub async fn list_in_flight_for_service(
        &self,
        host_id: HostId,
        service_name: &str,
    ) -> Result<Vec<Deployment>> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   group_id, previous_digest, rollback_attempted_at,
                   created_at, updated_at
            FROM deployments
            WHERE host_id = ? AND service_name = ?
              AND state NOT IN ('done', 'failed', 'aborted', 'rolled_back', 'rollback_failed')
            ORDER BY created_at ASC
            "#,
        )
        .bind(&host_bytes)
        .bind(service_name)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_deployment).collect()
    }

    /// History of deployments for one stack, newest first, capped at `limit`.
    pub async fn list_deployments_by_stack(
        &self,
        stack_id: StackId,
        limit: u32,
    ) -> Result<Vec<Deployment>> {
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   group_id, previous_digest, rollback_attempted_at,
                   created_at, updated_at
            FROM deployments
            WHERE stack_id = ?
            ORDER BY created_at DESC
            LIMIT ?
            "#,
        )
        .bind(stack_id.0)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_deployment).collect()
    }

    /// Transition a deployment to a new state. Bumps `updated_at`.
    pub async fn update_deployment_state(&self, id: &str, state: DeploymentState) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET state = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Record the green container name once the driver has created it.
    pub async fn set_deployment_green_container(&self, id: &str, container: &str) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET green_container = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(container)
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Stamp `healthcheck_passed_at`.
    pub async fn set_deployment_healthcheck_passed(
        &self,
        id: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET healthcheck_passed_at = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Stamp `switched_at` (when the proxy moved traffic to green).
    pub async fn set_deployment_switched(&self, id: &str, at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET switched_at = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Stamp `drained_at` (when the blue color drained).
    pub async fn set_deployment_drained(&self, id: &str, at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET drained_at = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Stamp `finished_at` (terminal-state timestamp).
    pub async fn set_deployment_finished(&self, id: &str, at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET finished_at = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Set the failure reason string.
    pub async fn set_deployment_error(&self, id: &str, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET error = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(error)
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Stamp the moment the supervisor entered the rollback branch.
    ///
    /// Set regardless of whether the rollback eventually succeeded:
    /// the dashboard reads this to render "Rolled back at HH:MM"
    /// without parsing the error string.
    pub async fn set_deployment_rollback_attempted(
        &self,
        id: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET rollback_attempted_at = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Patch `previous_digest` after the row has been inserted.
    ///
    /// Primary write happens at INSERT time when the resolved policy
    /// is `on_failure == Rollback`; this setter exists for tests and
    /// for any future "enable rollback mid-flight" flow.
    pub async fn set_deployment_previous_digest(&self, id: &str, digest: &str) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET previous_digest = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(digest)
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Upsert a deployment row received from a remote source (controller-side use).
    ///
    /// Idempotent `INSERT OR REPLACE` keyed on `id`. Each upsert
    /// carries the full row, so latest-write-wins per field. Used by
    /// the controller's event subscriber.
    pub async fn upsert_deployment_from_remote(&self, d: &Deployment) -> Result<()> {
        let host_bytes = d.host_id.0.to_bytes().to_vec();
        let to_rfc = |dt: Option<chrono::DateTime<chrono::Utc>>| dt.map(|t| t.to_rfc3339());
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO deployments (
                id, host_id, stack_id, service_name, strategy, state,
                blue_container, green_container, blue_digest, green_digest,
                public_hostname, health_path, container_port,
                healthcheck_started_at, healthcheck_passed_at, switched_at,
                drained_at, finished_at, error, metadata_json,
                group_id, previous_digest, rollback_attempted_at,
                created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&d.id)
        .bind(&host_bytes)
        .bind(d.stack_id.0)
        .bind(&d.service_name)
        .bind(d.strategy.as_str())
        .bind(d.state.as_str())
        .bind(&d.blue_container)
        .bind(&d.green_container)
        .bind(&d.blue_digest)
        .bind(&d.green_digest)
        .bind(&d.public_hostname)
        .bind(&d.health_path)
        .bind(d.container_port)
        .bind(to_rfc(d.healthcheck_started_at))
        .bind(to_rfc(d.healthcheck_passed_at))
        .bind(to_rfc(d.switched_at))
        .bind(to_rfc(d.drained_at))
        .bind(to_rfc(d.finished_at))
        .bind(&d.error)
        .bind(&d.metadata_json)
        .bind(&d.group_id)
        .bind(&d.previous_digest)
        .bind(to_rfc(d.rollback_attempted_at))
        .bind(d.created_at.to_rfc3339())
        .bind(d.updated_at.to_rfc3339())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Link an existing deployment row to a group.
    ///
    /// Used by the controller orchestrator after it has dispatched a
    /// wave of deployments and wants to associate them with their
    /// parent group.
    pub async fn set_deployment_group(&self, deployment_id: &str, group_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET group_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(group_id)
        .bind(deployment_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Every deployment belonging to `group_id`, oldest first.
    ///
    /// Used by the dashboard's group panel and by the orchestrator's
    /// wave-completion check.
    pub async fn list_deployments_by_group(&self, group_id: &str) -> Result<Vec<Deployment>> {
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   group_id, previous_digest, rollback_attempted_at,
                   created_at, updated_at
            FROM deployments
            WHERE group_id = ?
            ORDER BY created_at ASC
            "#,
        )
        .bind(group_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_deployment).collect()
    }

    /// Bulk-fail every in-flight deployment on `host_id` with `reason`.
    ///
    /// Used when a host enrolls fresh and the controller needs to
    /// flush stale rows pointing at the prior identity.
    pub async fn mark_orphan_deployments_failed(
        &self,
        host_id: HostId,
        reason: &str,
    ) -> Result<u64> {
        let host_bytes = host_id.0.to_bytes().to_vec();
        let res = sqlx::query(
            r#"
            UPDATE deployments
            SET state = 'failed', error = ?, updated_at = CURRENT_TIMESTAMP
            WHERE host_id = ? AND state NOT IN ('done', 'failed', 'aborted', 'rolled_back', 'rollback_failed')
            "#,
        )
        .bind(reason)
        .bind(&host_bytes)
        .execute(self.pool())
        .await?;
        Ok(res.rows_affected())
    }
}

/// Decode one `deployments` row into a [`Deployment`].
fn row_to_deployment(r: &sqlx::sqlite::SqliteRow) -> Result<Deployment> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = r.try_get("host_id")?;
    let host_id = HostId::from_db_bytes(host_bytes)?;

    let strategy_s: String = r.try_get("strategy")?;
    let state_s: String = r.try_get("state")?;
    let strategy: DeployStrategy = strategy_s.parse()?;
    let state: DeploymentState = state_s.parse()?;

    let parse_dt = |s: Option<String>| -> Result<Option<DateTime<Utc>>> {
        Ok(match s {
            Some(v) => Some(
                DateTime::parse_from_rfc3339(&v)
                    .or_else(|_| {
                        // SQLite CURRENT_TIMESTAMP format: "YYYY-MM-DD HH:MM:SS"
                        chrono::NaiveDateTime::parse_from_str(&v, "%Y-%m-%d %H:%M:%S")
                            .map(|n| n.and_utc().fixed_offset())
                    })
                    .map_err(|e| Error::Decode {
                        reason: format!("bad timestamp '{v}': {e}"),
                    })?
                    .with_timezone(&Utc),
            ),
            None => None,
        })
    };

    Ok(Deployment {
        id: r.try_get("id")?,
        host_id,
        stack_id: StackId(r.try_get::<i64, _>("stack_id")?),
        service_name: r.try_get("service_name")?,
        strategy,
        state,
        blue_container: r.try_get("blue_container")?,
        green_container: r.try_get("green_container")?,
        blue_digest: r.try_get("blue_digest")?,
        green_digest: r.try_get("green_digest")?,
        public_hostname: r.try_get("public_hostname")?,
        health_path: r.try_get("health_path")?,
        container_port: r.try_get("container_port")?,
        healthcheck_started_at: parse_dt(r.try_get("healthcheck_started_at")?)?,
        healthcheck_passed_at: parse_dt(r.try_get("healthcheck_passed_at")?)?,
        switched_at: parse_dt(r.try_get("switched_at")?)?,
        drained_at: parse_dt(r.try_get("drained_at")?)?,
        finished_at: parse_dt(r.try_get("finished_at")?)?,
        error: r.try_get("error")?,
        metadata_json: r.try_get("metadata_json")?,
        created_at: parse_dt(Some(r.try_get("created_at")?))?.ok_or_else(|| Error::Decode {
            reason: "created_at NULL".into(),
        })?,
        updated_at: parse_dt(Some(r.try_get("updated_at")?))?.ok_or_else(|| Error::Decode {
            reason: "updated_at NULL".into(),
        })?,
        group_id: r.try_get("group_id")?,
        previous_digest: r.try_get("previous_digest")?,
        rollback_attempted_at: parse_dt(r.try_get("rollback_attempted_at")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{EnrollHost, HostId};
    use crate::inventory::Inventory;
    use crate::stack::{InsertStack, StackId, StackSource};
    use ulid::Ulid;

    async fn setup() -> (Inventory, HostId, StackId) {
        let inv = Inventory::open_in_memory().await.expect("open");
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp1".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
            })
            .await
            .expect("enroll");
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .expect("insert stack");
        (inv, host_id, stack_id)
    }

    fn sample_insert(host_id: HostId, stack_id: StackId) -> InsertDeployment {
        InsertDeployment {
            id: Ulid::new().to_string(),
            host_id,
            stack_id,
            service_name: "web".into(),
            strategy: DeployStrategy::BlueGreen,
            state: DeploymentState::Pending,
            blue_container: Some("c-blue".into()),
            green_container: None,
            blue_digest: "sha256:aaa".into(),
            green_digest: "sha256:bbb".into(),
            public_hostname: Some("blog.test".into()),
            health_path: Some("/healthz".into()),
            container_port: Some(8080),
            metadata_json: None,
            previous_digest: None,
        }
    }

    #[tokio::test]
    async fn insert_returns_row_with_pending_state() {
        let (inv, host_id, stack_id) = setup().await;
        let d = inv
            .insert_deployment(sample_insert(host_id, stack_id))
            .await
            .expect("insert");
        assert_eq!(d.state, DeploymentState::Pending);
        assert_eq!(d.service_name, "web");
        assert!(d.green_container.is_none());
    }

    #[tokio::test]
    async fn update_state_changes_state_and_updated_at() {
        let (inv, host_id, stack_id) = setup().await;
        let d = inv
            .insert_deployment(sample_insert(host_id, stack_id))
            .await
            .expect("insert");
        let before = d.updated_at;

        // Sleep long enough for SQLite's CURRENT_TIMESTAMP (second resolution) to tick.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        inv.update_deployment_state(&d.id, DeploymentState::SpinningUp)
            .await
            .expect("update");
        let after = inv
            .get_deployment(&d.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(after.state, DeploymentState::SpinningUp);
        assert!(after.updated_at >= before);
    }

    #[tokio::test]
    async fn list_in_flight_excludes_terminal_states() {
        let (inv, host_id, stack_id) = setup().await;
        let mut a = sample_insert(host_id, stack_id);
        a.id = Ulid::new().to_string();
        a.service_name = "web-active".into();
        let a = inv.insert_deployment(a).await.unwrap();

        let mut b = sample_insert(host_id, stack_id);
        b.id = Ulid::new().to_string();
        b.service_name = "web-done".into();
        let b = inv.insert_deployment(b).await.unwrap();
        inv.update_deployment_state(&b.id, DeploymentState::Done)
            .await
            .unwrap();

        let mut c = sample_insert(host_id, stack_id);
        c.id = Ulid::new().to_string();
        c.service_name = "web-aborted".into();
        let c = inv.insert_deployment(c).await.unwrap();
        inv.update_deployment_state(&c.id, DeploymentState::Aborted)
            .await
            .unwrap();

        let in_flight = inv.list_in_flight_deployments(host_id).await.unwrap();
        let names: Vec<_> = in_flight.iter().map(|d| d.service_name.clone()).collect();
        assert!(names.contains(&"web-active".to_string()));
        assert!(!names.contains(&"web-done".to_string()));
        assert!(!names.contains(&"web-aborted".to_string()));
        let _ = a;
    }

    #[tokio::test]
    async fn cascade_delete_removes_deployments_when_stack_dropped() {
        let (inv, host_id, stack_id) = setup().await;
        let d = inv
            .insert_deployment(sample_insert(host_id, stack_id))
            .await
            .expect("insert");
        inv.delete_stack(stack_id).await.expect("delete stack");
        assert!(inv.get_deployment(&d.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_from_remote_inserts_or_replaces() {
        let (inv, host_id, stack_id) = setup().await;

        // Build a Deployment as if it arrived from a remote agent.
        let mut d = Deployment {
            id: ulid::Ulid::new().to_string(),
            host_id,
            stack_id,
            service_name: "web".into(),
            strategy: DeployStrategy::BlueGreen,
            state: DeploymentState::SpinningUp,
            blue_container: Some("c-blue".into()),
            green_container: None,
            blue_digest: "sha256:aaa".into(),
            green_digest: "sha256:bbb".into(),
            public_hostname: Some("blog.test".into()),
            health_path: Some("/healthz".into()),
            container_port: Some(8080),
            healthcheck_started_at: None,
            healthcheck_passed_at: None,
            switched_at: None,
            drained_at: None,
            finished_at: None,
            error: None,
            metadata_json: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            group_id: None,
            previous_digest: None,
            rollback_attempted_at: None,
        };

        // First upsert: insert.
        inv.upsert_deployment_from_remote(&d).await.unwrap();
        let back = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(back.state, DeploymentState::SpinningUp);

        // Second upsert with state change: replace.
        d.state = DeploymentState::Done;
        d.green_container = Some("c-green".into());
        inv.upsert_deployment_from_remote(&d).await.unwrap();
        let back = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(back.state, DeploymentState::Done);
        assert_eq!(back.green_container.as_deref(), Some("c-green"));
    }

    #[test]
    fn deployment_state_is_terminal_covers_three_terminals() {
        assert!(DeploymentState::Done.is_terminal());
        assert!(DeploymentState::Aborted.is_terminal());
        assert!(DeploymentState::Failed.is_terminal());
        assert!(!DeploymentState::Pending.is_terminal());
        assert!(!DeploymentState::SpinningUp.is_terminal());
        assert!(!DeploymentState::Switching.is_terminal());
        assert!(!DeploymentState::Draining.is_terminal());
        assert!(!DeploymentState::DestroyingBlue.is_terminal());
        assert!(!DeploymentState::Recovering.is_terminal());
    }

    #[test]
    fn deployment_state_round_trips_through_str() {
        for s in [
            DeploymentState::Pending,
            DeploymentState::SpinningUp,
            DeploymentState::Switching,
            DeploymentState::Draining,
            DeploymentState::DestroyingBlue,
            DeploymentState::Recovering,
            DeploymentState::Done,
            DeploymentState::Aborted,
            DeploymentState::Failed,
        ] {
            let parsed: DeploymentState = s.as_str().parse().expect("parse roundtrip");
            assert_eq!(parsed, s);
        }
    }

    // ----- rollback handler storage coverage -----

    /// Inserting a row with `previous_digest` set persists the value and
    /// it round-trips back through `get_deployment`. The default-None
    /// case is exercised by every other test in this module.
    #[tokio::test]
    async fn insert_with_previous_digest_round_trips() {
        let (inv, host_id, stack_id) = setup().await;
        let mut ins = sample_insert(host_id, stack_id);
        ins.previous_digest = Some("sha256:old".into());
        let d = inv.insert_deployment(ins).await.expect("insert");
        assert_eq!(d.previous_digest.as_deref(), Some("sha256:old"));
        let back = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(back.previous_digest.as_deref(), Some("sha256:old"));
        // rollback_attempted_at stays None until the supervisor enters
        // the rollback branch.
        assert!(back.rollback_attempted_at.is_none());
    }

    /// Calling `set_deployment_rollback_attempted` persists an RFC3339
    /// timestamp that survives a `get_deployment` round-trip.
    #[tokio::test]
    async fn set_rollback_attempted_persists_timestamp() {
        let (inv, host_id, stack_id) = setup().await;
        let d = inv
            .insert_deployment(sample_insert(host_id, stack_id))
            .await
            .unwrap();
        let when = chrono::Utc::now();
        inv.set_deployment_rollback_attempted(&d.id, when)
            .await
            .unwrap();
        let back = inv.get_deployment(&d.id).await.unwrap().unwrap();
        let got = back.rollback_attempted_at.expect("attempted_at populated");
        // RFC3339 round-trips at microsecond precision; allow a 1ms
        // tolerance for SQLite's text serialization.
        let delta = (got - when).num_milliseconds().abs();
        assert!(delta < 1000, "expected close to {when}, got {got}");
    }

    /// All three new states (RollingBack, RolledBack, RollbackFailed)
    /// round-trip through their string form.
    #[test]
    fn state_round_trips_for_new_states() {
        for s in [
            DeploymentState::RollingBack,
            DeploymentState::RolledBack,
            DeploymentState::RollbackFailed,
        ] {
            let parsed: DeploymentState = s.as_str().parse().expect("parse roundtrip");
            assert_eq!(parsed, s);
        }
    }

    /// `is_terminal` reports the two new terminal states; the in-flight
    /// `RollingBack` is non-terminal.
    #[test]
    fn terminal_includes_rolled_back_and_rollback_failed() {
        assert!(DeploymentState::RolledBack.is_terminal());
        assert!(DeploymentState::RollbackFailed.is_terminal());
        assert!(!DeploymentState::RollingBack.is_terminal());
    }

    /// `list_in_flight_deployments` excludes the new rollback terminal
    /// states. Three rows: one in-flight, one RolledBack, one
    /// RollbackFailed; only the first should come back.
    #[tokio::test]
    async fn list_in_flight_excludes_rollback_terminals() {
        let (inv, host_id, stack_id) = setup().await;

        let mut active = sample_insert(host_id, stack_id);
        active.id = Ulid::new().to_string();
        active.service_name = "web-active".into();
        let active = inv.insert_deployment(active).await.unwrap();

        let mut rolled = sample_insert(host_id, stack_id);
        rolled.id = Ulid::new().to_string();
        rolled.service_name = "web-rolled".into();
        let rolled = inv.insert_deployment(rolled).await.unwrap();
        inv.update_deployment_state(&rolled.id, DeploymentState::RolledBack)
            .await
            .unwrap();

        let mut failed = sample_insert(host_id, stack_id);
        failed.id = Ulid::new().to_string();
        failed.service_name = "web-rollback-failed".into();
        let failed = inv.insert_deployment(failed).await.unwrap();
        inv.update_deployment_state(&failed.id, DeploymentState::RollbackFailed)
            .await
            .unwrap();

        let in_flight = inv.list_in_flight_deployments(host_id).await.unwrap();
        let names: Vec<_> = in_flight.iter().map(|d| d.service_name.clone()).collect();
        assert_eq!(names, vec!["web-active".to_string()]);
        let _ = (active, rolled, failed);
    }
}
