//! `DeploymentGroup` entity: tracks a multi-host rolling deploy as one logical unit.
//!
//! Groups exist only when a stack-wide update fans out to more than one host.
//! Single-host deploys bypass the orchestrator entirely and never produce a row here.
//! See spec §Storage.

use crate::error::{Error, Result};
use crate::host::HostId;
use crate::stack::StackId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Lifecycle state for a deployment group.
///
/// Transitions: `Pending -> Rolling -> (Done | Aborted | Failed)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentGroupState {
    Pending,
    Rolling,
    Done,
    Aborted,
    Failed,
}

impl DeploymentGroupState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Rolling => "rolling",
            Self::Done => "done",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted | Self::Failed)
    }
}

impl FromStr for DeploymentGroupState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "pending" => Self::Pending,
            "rolling" => Self::Rolling,
            "done" => Self::Done,
            "aborted" => Self::Aborted,
            "failed" => Self::Failed,
            other => {
                return Err(Error::Decode {
                    reason: format!("unknown deployment_group state: {other}"),
                });
            }
        })
    }
}

/// One row from `deployment_groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentGroup {
    pub id: String,
    pub stack_id: StackId,
    pub service_name: String,
    /// Snapshot of stack parallelism at group-start. Either `"1"`..`"N"` or `"all"`.
    pub parallelism: String,
    pub state: DeploymentGroupState,
    /// Host ids that the orchestrator plans to roll over the course of the group.
    pub target_hosts: Vec<HostId>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InsertDeploymentGroup {
    pub id: String,
    pub stack_id: StackId,
    pub service_name: String,
    pub parallelism: String,
    pub state: DeploymentGroupState,
    pub target_hosts: Vec<HostId>,
}

impl crate::inventory::Inventory {
    /// Insert a new deployment group. Returns the freshly inserted row.
    pub async fn insert_deployment_group(
        &self,
        ins: InsertDeploymentGroup,
    ) -> Result<DeploymentGroup> {
        let target_hosts_json = serialize_target_hosts(&ins.target_hosts)?;
        sqlx::query(
            r#"
            INSERT INTO deployment_groups (
                id, stack_id, service_name, parallelism, state, target_hosts
            ) VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&ins.id)
        .bind(ins.stack_id.0)
        .bind(&ins.service_name)
        .bind(&ins.parallelism)
        .bind(ins.state.as_str())
        .bind(&target_hosts_json)
        .execute(self.pool())
        .await?;

        self.get_deployment_group(&ins.id)
            .await?
            .ok_or_else(|| Error::Decode {
                reason: format!("deployment_group {} not found after insert", ins.id),
            })
    }

    /// Fetch a single group by its ULID. Returns `None` if not present.
    pub async fn get_deployment_group(&self, id: &str) -> Result<Option<DeploymentGroup>> {
        let row = sqlx::query(
            r#"
            SELECT id, stack_id, service_name, parallelism, state,
                   target_hosts, started_at, finished_at, error
            FROM deployment_groups WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        let Some(r) = row else { return Ok(None) };
        Ok(Some(row_to_group(&r)?))
    }

    /// List groups for a stack ordered by `started_at DESC`. `limit` caps the result set.
    pub async fn list_deployment_groups(
        &self,
        stack_id: StackId,
        limit: u32,
    ) -> Result<Vec<DeploymentGroup>> {
        let rows = sqlx::query(
            r#"
            SELECT id, stack_id, service_name, parallelism, state,
                   target_hosts, started_at, finished_at, error
            FROM deployment_groups
            WHERE stack_id = ?
            ORDER BY started_at DESC
            LIMIT ?
            "#,
        )
        .bind(stack_id.0)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_group).collect()
    }

    /// Update the lifecycle state of a group. When the new state is terminal,
    /// `finished_at` is set to the current timestamp. The optional `error` is
    /// stored verbatim (typically populated when transitioning to `Failed` or
    /// `Aborted`).
    pub async fn update_deployment_group_state(
        &self,
        id: &str,
        state: DeploymentGroupState,
        error: Option<&str>,
    ) -> Result<()> {
        if state.is_terminal() {
            sqlx::query(
                r#"
                UPDATE deployment_groups
                SET state = ?, error = ?, finished_at = CURRENT_TIMESTAMP
                WHERE id = ?
                "#,
            )
            .bind(state.as_str())
            .bind(error)
            .bind(id)
            .execute(self.pool())
            .await?;
        } else {
            sqlx::query(
                r#"
                UPDATE deployment_groups
                SET state = ?, error = ?
                WHERE id = ?
                "#,
            )
            .bind(state.as_str())
            .bind(error)
            .bind(id)
            .execute(self.pool())
            .await?;
        }
        Ok(())
    }

    /// Set the per-stack `deployment_parallelism` value. `None` clears the
    /// override (default behavior: rolling, one host at a time).
    pub async fn set_stack_parallelism(
        &self,
        stack_id: StackId,
        parallelism: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE stacks SET deployment_parallelism = ? WHERE id = ?")
            .bind(parallelism)
            .bind(stack_id.0)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Read back the stored stack parallelism. `None` means "unset" (callers
    /// should treat this as the default value `"1"`).
    pub async fn get_stack_parallelism(&self, stack_id: StackId) -> Result<Option<String>> {
        use sqlx::Row;
        let row = sqlx::query("SELECT deployment_parallelism FROM stacks WHERE id = ?")
            .bind(stack_id.0)
            .fetch_optional(self.pool())
            .await?;
        match row {
            Some(r) => Ok(r.try_get::<Option<String>, _>("deployment_parallelism")?),
            None => Ok(None),
        }
    }
}

fn serialize_target_hosts(hosts: &[HostId]) -> Result<String> {
    let hex: Vec<String> = hosts.iter().map(|h| hex_encode_host_id(*h)).collect();
    serde_json::to_string(&hex).map_err(|e| Error::Decode {
        reason: format!("encoding target_hosts: {e}"),
    })
}

fn deserialize_target_hosts(json: &str) -> Result<Vec<HostId>> {
    let raw: Vec<String> = serde_json::from_str(json).map_err(|e| Error::Decode {
        reason: format!("decoding target_hosts: {e}"),
    })?;
    raw.into_iter().map(|s| hex_decode_host_id(&s)).collect()
}

fn hex_encode_host_id(id: HostId) -> String {
    let bytes = id.to_bytes();
    let mut out = String::with_capacity(32);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode_host_id(s: &str) -> Result<HostId> {
    if s.len() != 32 {
        return Err(Error::Decode {
            reason: format!("host_id hex must be 32 chars, got {}", s.len()),
        });
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        let byte_str = &s[i * 2..i * 2 + 2];
        bytes[i] = u8::from_str_radix(byte_str, 16).map_err(|e| Error::Decode {
            reason: format!("host_id hex parse: {e}"),
        })?;
    }
    Ok(HostId::from_bytes(bytes))
}

fn row_to_group(r: &sqlx::sqlite::SqliteRow) -> Result<DeploymentGroup> {
    use sqlx::Row;
    let state_s: String = r.try_get("state")?;
    let state: DeploymentGroupState = state_s.parse()?;
    let target_hosts_json: String = r.try_get("target_hosts")?;
    let target_hosts = deserialize_target_hosts(&target_hosts_json)?;

    let parse_dt = |s: Option<String>| -> Result<Option<DateTime<Utc>>> {
        Ok(match s {
            Some(v) => Some(
                DateTime::parse_from_rfc3339(&v)
                    .or_else(|_| {
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

    Ok(DeploymentGroup {
        id: r.try_get("id")?,
        stack_id: StackId(r.try_get::<i64, _>("stack_id")?),
        service_name: r.try_get("service_name")?,
        parallelism: r.try_get("parallelism")?,
        state,
        target_hosts,
        started_at: parse_dt(Some(r.try_get("started_at")?))?.ok_or_else(|| Error::Decode {
            reason: "started_at NULL".into(),
        })?,
        finished_at: parse_dt(r.try_get("finished_at")?)?,
        error: r.try_get("error")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_state_round_trips_through_str() {
        for s in [
            DeploymentGroupState::Pending,
            DeploymentGroupState::Rolling,
            DeploymentGroupState::Done,
            DeploymentGroupState::Aborted,
            DeploymentGroupState::Failed,
        ] {
            let parsed: DeploymentGroupState = s.as_str().parse().expect("parse roundtrip");
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn group_state_terminal_check() {
        assert!(DeploymentGroupState::Done.is_terminal());
        assert!(DeploymentGroupState::Aborted.is_terminal());
        assert!(DeploymentGroupState::Failed.is_terminal());
        assert!(!DeploymentGroupState::Pending.is_terminal());
        assert!(!DeploymentGroupState::Rolling.is_terminal());
    }

    #[test]
    fn host_id_hex_round_trip() {
        let id = HostId::new();
        let hex = hex_encode_host_id(id);
        assert_eq!(hex.len(), 32);
        let decoded = hex_decode_host_id(&hex).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn target_hosts_json_round_trip() {
        let hosts = vec![HostId::new(), HostId::new(), HostId::new()];
        let json = serialize_target_hosts(&hosts).unwrap();
        let back = deserialize_target_hosts(&json).unwrap();
        assert_eq!(back, hosts);
    }

    #[test]
    fn unknown_group_state_fails_to_parse() {
        let err = "wandering".parse::<DeploymentGroupState>();
        assert!(err.is_err());
    }
}
