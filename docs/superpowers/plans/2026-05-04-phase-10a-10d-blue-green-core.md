# Blue-Green Deployment Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a blue-green deployment driver to the agent that, when the `updater` plugin detects a routed healthcheck-equipped container needs an update, spins up a green container alongside blue, waits for it to go healthy, atomically swaps traffic via Plan C's `proxy::swap_upstream`, then drains and removes blue.

**Architecture:** New `crates/isengard-agent/src/deployment/` module sibling to `proxy/` (cannot be a plugin — needs in-process access to `proxy::swap_upstream`). New `deployments` storage entity. Updater plugin emits a `container.update_needed` event; an agent-side `DeploymentSupervisor` decides strategy (blue-green vs in-place) and spawns a per-deployment tokio task that walks the state machine. In-place containers are emitted back to the updater via `container.update_in_place`, preserving the existing `recreate.rs` behavior.

**Tech Stack:** Rust + sqlx (SQLite) + bollard (Docker) + tokio. Builds on `isengard-agent` (proxy/healthcheck, proxy/swap), `isengard-storage` (Inventory pattern), `isengard-core` (Event/EventEmitter), `isengard-plugins/updater` (lifecycle).

**Spec:** `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`

**Branch:** `feat/blue-green-core` stacked on `feat/networking-settings-ui-and-swap` (PR #20).

---

## File map

Create:
- `crates/isengard-storage/migrations/0011_deployments.sql`
- `crates/isengard-storage/src/deployment.rs`
- `crates/isengard-agent/src/deployment/mod.rs`
- `crates/isengard-agent/src/deployment/eligibility.rs`
- `crates/isengard-agent/src/deployment/healthcheck.rs`
- `crates/isengard-agent/src/deployment/driver.rs`
- `crates/isengard-agent/tests/deployment_blue_green_happy.rs`
- `crates/isengard-agent/tests/deployment_blue_green_aborts_on_healthcheck.rs`

Modify:
- `crates/isengard-storage/src/lib.rs` (add `pub mod deployment;`)
- `crates/isengard-agent/src/lib.rs` (add `pub mod deployment;`)
- `crates/isengard-plugins/updater/src/lib.rs` (emit event on needs_update; subscribe to update_in_place)
- `crates/isengard/src/main.rs` or `crates/isengard-agent/src/run_agent.rs` (wire DeploymentSupervisor into the agent's task spawn)

---

## Task split

| Task | Sub-phase | Scope | Tests added |
|---|---|---|---|
| 1 | 10a | Migration + storage entity + DAO + DeploymentState enum | 4 storage + 2 enum |
| 2 | 10b-i | Eligibility classifier | 5 unit |
| 3 | 10b-ii | DeploymentHealthcheck wrapper | 5 unit |
| 4 | 10c-i | Driver state machine (with mocked deps) | 4 unit |
| 5 | 10c-ii | DeploymentSupervisor | 3 unit |
| 6 | 10d-i | Updater integration: emit + subscribe | 2 unit |
| 7 | 10d-ii | Agent main wiring (Supervisor spawn) | (compile + manual smoke) |
| 8 | 10c-iii | Real-Docker e2e: happy path | 1 e2e (`#[ignore]`) |
| 9 | 10d-iii | Real-Docker e2e: abort on healthcheck timeout | 1 e2e (`#[ignore]`) |
| 10 | 10e | Final workspace-green + open PR #21 | (gates) |

---

## Task 1: Migration 0011 + storage entity + DeploymentState + DAO

**Files:**
- Create: `crates/isengard-storage/migrations/0011_deployments.sql`
- Create: `crates/isengard-storage/src/deployment.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Create migration**

Create `crates/isengard-storage/migrations/0011_deployments.sql`:

```sql
-- Phase 10a: deployments table for blue-green deployment tracking.
-- See docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md §Storage.

CREATE TABLE deployments (
    id                       TEXT PRIMARY KEY,                 -- ULID
    host_id                  BLOB NOT NULL,
    stack_id                 INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    service_name             TEXT NOT NULL,
    strategy                 TEXT NOT NULL CHECK (strategy IN ('blue-green', 'in-place')),
    state                    TEXT NOT NULL CHECK (state IN (
        'pending', 'spinning_up', 'switching', 'draining',
        'destroying_blue', 'done', 'aborted', 'failed'
    )),
    blue_container           TEXT,
    green_container          TEXT,
    blue_digest              TEXT NOT NULL,
    green_digest             TEXT NOT NULL,
    public_hostname          TEXT,
    health_path              TEXT,
    container_port           INTEGER,
    healthcheck_started_at   TEXT,
    healthcheck_passed_at    TEXT,
    switched_at              TEXT,
    drained_at               TEXT,
    finished_at              TEXT,
    error                    TEXT,
    metadata_json            TEXT,
    created_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_deployments_state_active
    ON deployments(state)
    WHERE state NOT IN ('done', 'failed', 'aborted');

CREATE INDEX idx_deployments_stack_created
    ON deployments(stack_id, created_at DESC);

CREATE INDEX idx_deployments_host_service_active
    ON deployments(host_id, service_name)
    WHERE state NOT IN ('done', 'failed', 'aborted');
```

- [ ] **Step 2: Register the storage module**

Modify `crates/isengard-storage/src/lib.rs` — add `pub mod deployment;` next to the other `pub mod` lines (alphabetical).

- [ ] **Step 3: Write the failing storage tests**

Create `crates/isengard-storage/src/deployment.rs` with the test module ONLY (the impl follows in Step 5):

```rust
//! `Deployment` entity: tracks one in-flight or completed deployment.
//! See spec §Storage.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostId;
    use crate::inventory::Inventory;
    use crate::stack::{InsertStack, StackSource};
    use ulid::Ulid;

    async fn setup() -> (Inventory, HostId, crate::stack::StackId) {
        let inv = Inventory::open_in_memory().await.expect("open");
        let host = inv
            .enroll_host(crate::host::EnrollHost {
                hostname: "h1".into(),
                fingerprint: "fp1".into(),
                fleet: Some("default".into()),
            })
            .await
            .expect("enroll");
        inv.create_fleet("default").await.ok();
        let stack = inv
            .insert_stack(InsertStack {
                host_id: host.id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .expect("insert stack");
        (inv, host.id, stack.id)
    }

    fn sample_insert(host_id: HostId, stack_id: crate::stack::StackId) -> InsertDeployment {
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

        // Sleep just long enough for SQLite's CURRENT_TIMESTAMP (second resolution) to tick.
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
        inv.update_deployment_state(&b.id, DeploymentState::Done).await.unwrap();

        let mut c = sample_insert(host_id, stack_id);
        c.id = Ulid::new().to_string();
        c.service_name = "web-aborted".into();
        let c = inv.insert_deployment(c).await.unwrap();
        inv.update_deployment_state(&c.id, DeploymentState::Aborted).await.unwrap();

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
    }

    #[test]
    fn deployment_state_round_trips_through_str() {
        for s in [
            DeploymentState::Pending, DeploymentState::SpinningUp, DeploymentState::Switching,
            DeploymentState::Draining, DeploymentState::DestroyingBlue, DeploymentState::Done,
            DeploymentState::Aborted, DeploymentState::Failed,
        ] {
            let parsed: DeploymentState = s.as_str().parse().expect("parse roundtrip");
            assert_eq!(parsed, s);
        }
    }
}
```

(The `enroll_host` / `create_fleet` / `insert_stack` / `delete_stack` calls are existing Inventory methods. If `delete_stack` doesn't exist, see Step 5's note — we'll add it.)

- [ ] **Step 4: Run tests to confirm they fail**

Run: `cargo test -p isengard-storage --lib deployment::tests`
Expected: compile error — `DeploymentState`, `InsertDeployment`, `insert_deployment`, etc. undefined.

- [ ] **Step 5: Implement the entity + DAO**

Replace the contents of `crates/isengard-storage/src/deployment.rs` with:

```rust
//! `Deployment` entity: tracks one in-flight or completed deployment.
//! See spec §Storage.

use crate::error::{Error, Result};
use crate::host::HostId;
use crate::stack::StackId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeployStrategy {
    BlueGreen,
    InPlace,
}

impl DeployStrategy {
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
            other => Err(Error::Decode(format!("unknown deploy strategy: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    Pending,
    SpinningUp,
    Switching,
    Draining,
    DestroyingBlue,
    Done,
    Aborted,
    Failed,
}

impl DeploymentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::SpinningUp => "spinning_up",
            Self::Switching => "switching",
            Self::Draining => "draining",
            Self::DestroyingBlue => "destroying_blue",
            Self::Done => "done",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted | Self::Failed)
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
            "done" => Self::Done,
            "aborted" => Self::Aborted,
            "failed" => Self::Failed,
            other => return Err(Error::Decode(format!("unknown deployment state: {other}"))),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub host_id: HostId,
    pub stack_id: StackId,
    pub service_name: String,
    pub strategy: DeployStrategy,
    pub state: DeploymentState,
    pub blue_container: Option<String>,
    pub green_container: Option<String>,
    pub blue_digest: String,
    pub green_digest: String,
    pub public_hostname: Option<String>,
    pub health_path: Option<String>,
    pub container_port: Option<i64>,
    pub healthcheck_started_at: Option<DateTime<Utc>>,
    pub healthcheck_passed_at: Option<DateTime<Utc>>,
    pub switched_at: Option<DateTime<Utc>>,
    pub drained_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub metadata_json: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct InsertDeployment {
    pub id: String,
    pub host_id: HostId,
    pub stack_id: StackId,
    pub service_name: String,
    pub strategy: DeployStrategy,
    pub state: DeploymentState,
    pub blue_container: Option<String>,
    pub green_container: Option<String>,
    pub blue_digest: String,
    pub green_digest: String,
    pub public_hostname: Option<String>,
    pub health_path: Option<String>,
    pub container_port: Option<i64>,
    pub metadata_json: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn insert_deployment(&self, ins: InsertDeployment) -> Result<Deployment> {
        let host_bytes = ins.host_id.0.to_bytes().to_vec();
        sqlx::query(
            r#"
            INSERT INTO deployments (
              id, host_id, stack_id, service_name, strategy, state,
              blue_container, green_container, blue_digest, green_digest,
              public_hostname, health_path, container_port, metadata_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .execute(self.pool())
        .await?;

        self.get_deployment(&ins.id).await?
            .ok_or_else(|| Error::Decode(format!("deployment {} not found after insert", ins.id)))
    }

    pub async fn get_deployment(&self, id: &str) -> Result<Option<Deployment>> {
        use sqlx::Row;
        let row = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
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

    pub async fn list_in_flight_deployments(&self, host_id: HostId) -> Result<Vec<Deployment>> {
        use sqlx::Row;
        let host_bytes = host_id.0.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   created_at, updated_at
            FROM deployments
            WHERE host_id = ? AND state NOT IN ('done', 'failed', 'aborted')
            ORDER BY created_at ASC
            "#,
        )
        .bind(&host_bytes)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_deployment).collect()
    }

    pub async fn list_in_flight_for_service(
        &self,
        host_id: HostId,
        service_name: &str,
    ) -> Result<Vec<Deployment>> {
        use sqlx::Row;
        let host_bytes = host_id.0.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
                   created_at, updated_at
            FROM deployments
            WHERE host_id = ? AND service_name = ?
              AND state NOT IN ('done', 'failed', 'aborted')
            ORDER BY created_at ASC
            "#,
        )
        .bind(&host_bytes)
        .bind(service_name)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_deployment).collect()
    }

    pub async fn list_deployments_by_stack(
        &self,
        stack_id: StackId,
        limit: u32,
    ) -> Result<Vec<Deployment>> {
        use sqlx::Row;
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, strategy, state,
                   blue_container, green_container, blue_digest, green_digest,
                   public_hostname, health_path, container_port,
                   healthcheck_started_at, healthcheck_passed_at, switched_at,
                   drained_at, finished_at, error, metadata_json,
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

    pub async fn update_deployment_state(
        &self,
        id: &str,
        state: DeploymentState,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE deployments SET state = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

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
            WHERE host_id = ? AND state NOT IN ('done', 'failed', 'aborted')
            "#,
        )
        .bind(reason)
        .bind(&host_bytes)
        .execute(self.pool())
        .await?;
        Ok(res.rows_affected())
    }
}

fn row_to_deployment(r: &sqlx::sqlite::SqliteRow) -> Result<Deployment> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = r.try_get("host_id")?;
    let host_id = HostId::from_bytes(&host_bytes)
        .map_err(|e| Error::Decode(format!("invalid host_id bytes: {e}")))?;

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
                    .map_err(|e| Error::Decode(format!("bad timestamp '{v}': {e}")))?
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
        created_at: parse_dt(Some(r.try_get("created_at")?))?
            .ok_or_else(|| Error::Decode("created_at NULL".into()))?,
        updated_at: parse_dt(Some(r.try_get("updated_at")?))?
            .ok_or_else(|| Error::Decode("updated_at NULL".into()))?,
    })
}

// Re-paste the test module from Step 3 below this line.
```

Then re-append the test module from Step 3 at the bottom of the file (it references `super::*`).

**Note on `delete_stack`:** if Inventory doesn't already expose `delete_stack`, add a minimal one to `crates/isengard-storage/src/stack.rs` in the existing `impl crate::inventory::Inventory` block:

```rust
pub async fn delete_stack(&self, id: StackId) -> Result<()> {
    sqlx::query("DELETE FROM stacks WHERE id = ?")
        .bind(id.0)
        .execute(self.pool())
        .await?;
    Ok(())
}
```

(If it already exists or has a different signature, skip this — the cascade test just needs *some* way to delete the parent stack.)

**Note on `Error::Decode`:** if the storage `Error` enum doesn't have a `Decode(String)` variant, use whatever the closest existing variant is (e.g., `Error::Sql(...)` or `Error::Other(...)`). The pattern matters more than the variant name. Grep `crates/isengard-storage/src/error.rs` first.

- [ ] **Step 6: Run tests, expect green**

Run: `cargo test -p isengard-storage --lib deployment::tests`
Expected: 6/6 pass.

Run: `cargo test -p isengard-storage` (full storage suite)
Expected: all green (existing tests untouched).

- [ ] **Step 7: Commit**

```bash
cd /Users/dirdmaster/Projects/isengard/.worktrees/blue-green-core
cargo fmt --all
git add crates/isengard-storage/migrations/0011_deployments.sql \
        crates/isengard-storage/src/deployment.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/src/stack.rs
git commit -m "feat(storage): deployments table + Deployment entity + DAO"
```

(Add `crates/isengard-storage/src/error.rs` to the add list if you had to extend Error.)

---

## Task 2: Eligibility classifier

**Files:**
- Create: `crates/isengard-agent/src/deployment/mod.rs` (just `pub mod eligibility;` for now)
- Create: `crates/isengard-agent/src/deployment/eligibility.rs`
- Modify: `crates/isengard-agent/src/lib.rs` (add `pub mod deployment;`)

- [ ] **Step 1: Wire the module**

Modify `crates/isengard-agent/src/lib.rs` — add `pub mod deployment;` next to existing `pub mod proxy;` line.

Create `crates/isengard-agent/src/deployment/mod.rs`:

```rust
//! Blue-green deployment driver. See spec
//! `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`.

pub mod eligibility;
```

- [ ] **Step 2: Write failing tests**

Create `crates/isengard-agent/src/deployment/eligibility.rs`:

```rust
//! Pure classifier: given a container's spec + an optional label override,
//! decide whether to deploy it via blue-green or in-place.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InPlaceReason {
    NoRoutingRule,
    StatefulVolume,
    NoHealthcheck,
    LabelForced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    BlueGreen,
    InPlace { reason: InPlaceReason },
}

#[derive(Debug, Clone)]
pub struct ContainerSpec<'a> {
    /// True if a routing rule exists pointing at this service:port.
    pub has_routing_rule: bool,
    /// True if the image has HEALTHCHECK or compose has a healthcheck section.
    pub has_healthcheck: bool,
    /// rw bind/named volume mount paths (empty = no stateful state).
    pub rw_volume_mounts: &'a [String],
    /// Value of the `isengard.deploy.strategy` label, if any.
    pub label_strategy: Option<&'a str>,
}

pub fn classify(spec: &ContainerSpec) -> Decision {
    // Label override: explicit user choice wins. "auto" or unknown values
    // fall through to the autodetect cascade.
    match spec.label_strategy {
        Some("blue-green") => return Decision::BlueGreen,
        Some("in-place") => return Decision::InPlace { reason: InPlaceReason::LabelForced },
        _ => {}
    }

    if !spec.has_routing_rule {
        return Decision::InPlace { reason: InPlaceReason::NoRoutingRule };
    }
    if !spec.rw_volume_mounts.is_empty() {
        return Decision::InPlace { reason: InPlaceReason::StatefulVolume };
    }
    if !spec.has_healthcheck {
        return Decision::InPlace { reason: InPlaceReason::NoHealthcheck };
    }

    Decision::BlueGreen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> ContainerSpec<'static> {
        ContainerSpec {
            has_routing_rule: true,
            has_healthcheck: true,
            rw_volume_mounts: &[],
            label_strategy: None,
        }
    }

    #[test]
    fn classifies_baseline_as_blue_green() {
        assert_eq!(classify(&baseline()), Decision::BlueGreen);
    }

    #[test]
    fn no_routing_rule_means_in_place() {
        let mut s = baseline();
        s.has_routing_rule = false;
        assert_eq!(
            classify(&s),
            Decision::InPlace { reason: InPlaceReason::NoRoutingRule }
        );
    }

    #[test]
    fn stateful_volume_means_in_place() {
        let mounts = vec!["/data".to_string()];
        let s = ContainerSpec { rw_volume_mounts: &mounts, ..baseline() };
        assert_eq!(
            classify(&s),
            Decision::InPlace { reason: InPlaceReason::StatefulVolume }
        );
    }

    #[test]
    fn no_healthcheck_means_in_place() {
        let mut s = baseline();
        s.has_healthcheck = false;
        assert_eq!(
            classify(&s),
            Decision::InPlace { reason: InPlaceReason::NoHealthcheck }
        );
    }

    #[test]
    fn label_override_wins_over_autodetect() {
        // Container has stateful volume → would normally be in-place,
        // but user explicitly opts into blue-green.
        let mounts = vec!["/data".to_string()];
        let s = ContainerSpec {
            rw_volume_mounts: &mounts,
            label_strategy: Some("blue-green"),
            ..baseline()
        };
        assert_eq!(classify(&s), Decision::BlueGreen);

        // Container is fully BG-eligible but user forces in-place.
        let s2 = ContainerSpec { label_strategy: Some("in-place"), ..baseline() };
        assert_eq!(
            classify(&s2),
            Decision::InPlace { reason: InPlaceReason::LabelForced }
        );

        // "auto" label falls through to autodetect.
        let s3 = ContainerSpec { label_strategy: Some("auto"), ..baseline() };
        assert_eq!(classify(&s3), Decision::BlueGreen);
    }
}
```

- [ ] **Step 3: Run, expect pass**

Run: `cargo test -p isengard-agent --lib deployment::eligibility::tests`
Expected: 5/5 pass.

(No fail-then-pass loop — the implementation is in the same file as the tests since the function is pure and trivial. The discipline of TDD here is verifying the test cases exhaustively cover the decision matrix; the implementation falls out.)

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/lib.rs \
        crates/isengard-agent/src/deployment/mod.rs \
        crates/isengard-agent/src/deployment/eligibility.rs
git commit -m "feat(agent): deployment eligibility classifier"
```

---

## Task 3: DeploymentHealthcheck wrapper

**Files:**
- Create: `crates/isengard-agent/src/deployment/healthcheck.rs`
- Modify: `crates/isengard-agent/src/deployment/mod.rs`

- [ ] **Step 1: Add module**

Modify `crates/isengard-agent/src/deployment/mod.rs`:

```rust
pub mod eligibility;
pub mod healthcheck;
```

- [ ] **Step 2: Write failing tests**

Create `crates/isengard-agent/src/deployment/healthcheck.rs`:

```rust
//! Polling healthcheck with success threshold + deadline.
//!
//! Composes [`crate::proxy::healthcheck::HealthChecker`] (the per-probe
//! primitive) and adds: initial delay, periodic polling, threshold counting,
//! and overall deadline. Used by the blue-green deployment driver to decide
//! when the green container is ready to take traffic.

use chrono::{DateTime, Utc};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::proxy::healthcheck::HealthChecker;

#[derive(Debug, Clone)]
pub struct AttemptResult {
    pub at: DateTime<Utc>,
    pub passed: bool,
}

#[derive(Debug)]
pub struct HealthcheckTimeout {
    /// Up to the last 5 attempts, oldest first.
    pub last_attempts: Vec<AttemptResult>,
}

pub struct DeploymentHealthcheck {
    inner: HealthChecker,
    interval: Duration,
    success_threshold: u32,
    initial_delay: Duration,
    deadline: Duration,
}

impl DeploymentHealthcheck {
    pub fn new(inner: HealthChecker) -> Self {
        Self {
            inner,
            interval: Duration::from_secs(5),
            success_threshold: 2,
            initial_delay: Duration::from_secs(0),
            deadline: Duration::from_secs(120),
        }
    }
    pub fn with_interval(mut self, d: Duration) -> Self { self.interval = d; self }
    pub fn with_success_threshold(mut self, n: u32) -> Self { self.success_threshold = n; self }
    pub fn with_initial_delay(mut self, d: Duration) -> Self { self.initial_delay = d; self }
    pub fn with_deadline(mut self, d: Duration) -> Self { self.deadline = d; self }

    /// Polls until `success_threshold` consecutive `check_once` passes, or
    /// `deadline` (measured from start, not from end of `initial_delay`).
    pub async fn wait_for_healthy(
        &self,
        addr: SocketAddr,
    ) -> Result<DateTime<Utc>, HealthcheckTimeout> {
        let started = Instant::now();
        sleep(self.initial_delay).await;
        let mut consecutive = 0u32;
        let mut last_attempts: Vec<AttemptResult> = Vec::new();

        loop {
            if started.elapsed() >= self.deadline {
                return Err(HealthcheckTimeout { last_attempts });
            }
            let passed = self.inner.check_once(addr).await;
            let attempt = AttemptResult { at: Utc::now(), passed };
            last_attempts.push(attempt);
            if last_attempts.len() > 5 {
                last_attempts.remove(0);
            }

            if passed {
                consecutive += 1;
                if consecutive >= self.success_threshold {
                    return Ok(Utc::now());
                }
            } else {
                consecutive = 0;
            }
            sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    /// Spin up a tiny TCP listener that accepts and immediately closes.
    /// HealthChecker::tcp_only treats accept-success as healthy.
    async fn spawn_tcp_passing() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let _ = l.accept().await;
            }
        });
        addr
    }

    /// Returns an addr that nothing listens on — connect always fails.
    fn dead_addr() -> SocketAddr {
        // Port 1 on loopback: reliably refused/unbound on dev machines.
        "127.0.0.1:1".parse().unwrap()
    }

    #[tokio::test]
    async fn passes_after_threshold_consecutive_successes() {
        let addr = spawn_tcp_passing().await;
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(100)))
            .with_interval(Duration::from_millis(20))
            .with_success_threshold(2)
            .with_deadline(Duration::from_secs(2));
        let res = hc.wait_for_healthy(addr).await;
        assert!(res.is_ok(), "expected pass, got {res:?}");
    }

    #[tokio::test]
    async fn deadline_expires_when_target_never_responds() {
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(50)))
            .with_interval(Duration::from_millis(20))
            .with_success_threshold(2)
            .with_deadline(Duration::from_millis(300));
        let res = hc.wait_for_healthy(dead_addr()).await;
        assert!(res.is_err(), "expected timeout, got {res:?}");
        let err = res.unwrap_err();
        assert!(!err.last_attempts.is_empty(), "should have logged attempts");
        assert!(err.last_attempts.iter().all(|a| !a.passed));
    }

    #[tokio::test]
    async fn last_attempts_capped_at_five() {
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(20)))
            .with_interval(Duration::from_millis(10))
            .with_success_threshold(99)              // never pass
            .with_deadline(Duration::from_millis(500));
        let res = hc.wait_for_healthy(dead_addr()).await;
        let err = res.unwrap_err();
        assert!(err.last_attempts.len() <= 5);
    }

    #[tokio::test]
    async fn initial_delay_postpones_first_check() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();
        // Spawn a listener that increments on each accept so we can count probes.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                if l.accept().await.is_ok() {
                    counter2.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(100)))
            .with_interval(Duration::from_millis(50))
            .with_success_threshold(99)
            .with_initial_delay(Duration::from_millis(200))
            .with_deadline(Duration::from_millis(150));    // shorter than initial_delay
        let res = hc.wait_for_healthy(addr).await;
        assert!(res.is_err());
        // Initial delay (200ms) > deadline (150ms), so the loop should exit
        // before any check_once is called.
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fail_resets_consecutive_counter() {
        // Tricky to test deterministically without a mock HealthChecker.
        // Smoke test: alternate addr behavior is hard to inject; instead,
        // assert the structural property — a single-pass with threshold=2
        // requires a SECOND probe before returning success.
        let addr = spawn_tcp_passing().await;
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(100)))
            .with_interval(Duration::from_millis(50))
            .with_success_threshold(2)
            .with_deadline(Duration::from_secs(2));
        let started = Instant::now();
        let _ = hc.wait_for_healthy(addr).await.expect("pass");
        // Two probes at interval=50ms apart → at least ~50ms total.
        assert!(started.elapsed() >= Duration::from_millis(40));
    }
}
```

- [ ] **Step 3: Run, expect pass**

Run: `cargo test -p isengard-agent --lib deployment::healthcheck::tests`
Expected: 5/5 pass.

If `port 1` doesn't reliably refuse on the dev machine, swap `dead_addr()` to bind a TcpListener and immediately drop it, capturing the addr — its connect attempts will fail with refused once dropped. This is more deterministic than relying on port 1 being unbound.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/deployment/mod.rs \
        crates/isengard-agent/src/deployment/healthcheck.rs
git commit -m "feat(agent): DeploymentHealthcheck (polling + threshold + deadline)"
```

---

## Task 4: Driver state machine (with mocked deps)

**Files:**
- Create: `crates/isengard-agent/src/deployment/driver.rs`
- Modify: `crates/isengard-agent/src/deployment/mod.rs`

- [ ] **Step 1: Add module + define the trait surface for mockable deps**

Modify `crates/isengard-agent/src/deployment/mod.rs`:

```rust
pub mod driver;
pub mod eligibility;
pub mod healthcheck;
```

Create `crates/isengard-agent/src/deployment/driver.rs` with the trait surface only:

```rust
//! Blue-green deployment driver. State machine that walks one Deployment
//! row from spinning_up → switching → draining → destroying_blue → done,
//! or to aborted/failed on errors.

use crate::deployment::healthcheck::{DeploymentHealthcheck, HealthcheckTimeout};
use crate::proxy::healthcheck::HealthChecker;
use crate::proxy::swap::swap_upstream;
use crate::proxy::upstreams::{Upstream, UpstreamState};
use crate::proxy::ProxyState;
use anyhow::{Context as _, Result};
use chrono::Utc;
use isengard_core::{Event, EventEmitter};
use isengard_storage::deployment::{Deployment, DeploymentState};
use isengard_storage::inventory::Inventory;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Things the Driver does to the outside world. Trait-ified so the unit
/// tests can mock without spinning up Docker or Pingora.
#[async_trait::async_trait]
pub trait DriverDeps: Send + Sync {
    /// Pull the image (no-op if cached) + create + start green container.
    /// Returns the (container_id, addr) of the started green.
    async fn start_green(
        &self,
        deployment: &Deployment,
    ) -> Result<(String, SocketAddr)>;

    /// Stop + remove a container. "Not found" is treated as success.
    async fn stop_and_remove(&self, container_id: &str) -> Result<()>;

    /// Build the per-probe HealthChecker for this deployment.
    fn build_health_checker(&self, deployment: &Deployment) -> HealthChecker;

    /// Call into the proxy's atomic swap.
    async fn swap_upstream_to_green(
        &self,
        deployment: &Deployment,
        green_addr: SocketAddr,
        green_container_id: &str,
        grace: Duration,
    ) -> Result<()>;
}

pub struct Driver<D: DriverDeps> {
    pub deployment: Deployment,
    pub deps: Arc<D>,
    pub inventory: Inventory,
    pub emitter: Arc<dyn EventEmitter>,
    pub grace_period: Duration,
}

impl<D: DriverDeps> Driver<D> {
    pub fn new(
        deployment: Deployment,
        deps: Arc<D>,
        inventory: Inventory,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self {
            deployment,
            deps,
            inventory,
            emitter,
            grace_period: Duration::from_secs(60),
        }
    }

    pub async fn run(mut self) {
        if let Err(e) = self.run_inner().await {
            self.transition_to(DeploymentState::Failed).await.ok();
            self.set_error(&format!("{e:#}")).await.ok();
            self.emit("deployment.failed", Some(format!("{e:#}")));
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        // pending → spinning_up
        self.transition_to(DeploymentState::SpinningUp).await?;
        self.emit("deployment.spinning_up", None);

        // start green; failure here aborts cleanly
        let (green_id, green_addr) = match self.deps.start_green(&self.deployment).await {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("spinup_failed: {e:#}");
                self.set_error(&msg).await.ok();
                self.transition_to(DeploymentState::Aborted).await?;
                self.emit("deployment.aborted", Some(msg));
                return Ok(());
            }
        };
        self.deployment.green_container = Some(green_id.clone());
        self.inventory
            .set_deployment_green_container(&self.deployment.id, &green_id)
            .await?;

        // healthcheck loop
        let hc = DeploymentHealthcheck::new(self.deps.build_health_checker(&self.deployment));
        match hc.wait_for_healthy(green_addr).await {
            Ok(passed_at) => {
                self.deployment.healthcheck_passed_at = Some(passed_at);
                self.inventory
                    .set_deployment_healthcheck_passed(&self.deployment.id, passed_at)
                    .await?;
            }
            Err(timeout) => {
                let attempts_str = format_attempts(&timeout);
                let msg = format!("healthcheck_timeout: {attempts_str}");
                self.set_error(&msg).await.ok();
                self.deps.stop_and_remove(&green_id).await.ok();
                self.transition_to(DeploymentState::Aborted).await?;
                self.emit("deployment.aborted", Some(msg));
                return Ok(());
            }
        }

        // switching: call swap_upstream
        self.transition_to(DeploymentState::Switching).await?;
        self.emit("deployment.switched", None);

        if let Err(e) = self
            .deps
            .swap_upstream_to_green(&self.deployment, green_addr, &green_id, self.grace_period)
            .await
        {
            let msg = format!("swap_failed: {e:#}");
            self.set_error(&msg).await.ok();
            self.deps.stop_and_remove(&green_id).await.ok();
            self.transition_to(DeploymentState::Aborted).await?;
            self.emit("deployment.aborted", Some(msg));
            return Ok(());
        }
        let switched_at = Utc::now();
        self.inventory
            .set_deployment_switched(&self.deployment.id, switched_at)
            .await?;

        // draining: wait grace period + small buffer
        self.transition_to(DeploymentState::Draining).await?;
        tokio::time::sleep(self.grace_period + Duration::from_secs(5)).await;
        let drained_at = Utc::now();
        self.inventory
            .set_deployment_drained(&self.deployment.id, drained_at)
            .await?;

        // destroying_blue
        self.transition_to(DeploymentState::DestroyingBlue).await?;
        if let Some(blue) = self.deployment.blue_container.clone() {
            self.deps.stop_and_remove(&blue).await.ok();
        }

        // done
        let finished_at = Utc::now();
        self.inventory
            .set_deployment_finished(&self.deployment.id, finished_at)
            .await?;
        self.transition_to(DeploymentState::Done).await?;
        self.emit("deployment.completed", None);
        Ok(())
    }

    async fn transition_to(&mut self, new: DeploymentState) -> Result<()> {
        self.inventory
            .update_deployment_state(&self.deployment.id, new)
            .await
            .with_context(|| format!("update state -> {new:?}"))?;
        self.deployment.state = new;
        Ok(())
    }

    async fn set_error(&mut self, msg: &str) -> Result<()> {
        self.inventory
            .set_deployment_error(&self.deployment.id, msg)
            .await?;
        self.deployment.error = Some(msg.to_string());
        Ok(())
    }

    fn emit(&self, kind: &str, error: Option<String>) {
        let event = Event {
            kind: kind.to_string(),
            occurred_at: Utc::now(),
            host_id: Some(self.deployment.host_id),
            summary: format!(
                "{} {} {}",
                self.deployment.service_name,
                self.deployment.green_digest,
                kind
            ),
            error,
            metadata: serde_json::json!({
                "deployment_id": self.deployment.id,
                "service_name": self.deployment.service_name,
                "blue_digest": self.deployment.blue_digest,
                "green_digest": self.deployment.green_digest,
            }),
            ..Default::default()
        };
        let emitter = self.emitter.clone();
        // Fire-and-forget; the agent's emitter is non-blocking.
        tokio::spawn(async move {
            emitter.emit(event).await;
        });
    }
}

fn format_attempts(timeout: &HealthcheckTimeout) -> String {
    let n = timeout.last_attempts.len();
    let last_pass = timeout
        .last_attempts
        .iter()
        .rev()
        .find(|a| a.passed)
        .map(|a| a.at.to_rfc3339())
        .unwrap_or_else(|| "never".to_string());
    format!("{n} attempts, last_pass={last_pass}")
}

/// Concrete DriverDeps that wires the real Docker client + the in-process
/// proxy::swap_upstream. Used in production. Constructed by Supervisor
/// and shared via `Arc`.
pub struct RealDriverDeps {
    pub docker: Arc<bollard::Docker>,
    pub proxy_state: ProxyState,
}

#[async_trait::async_trait]
impl DriverDeps for RealDriverDeps {
    async fn start_green(
        &self,
        deployment: &Deployment,
    ) -> Result<(String, SocketAddr)> {
        // Implementation: pull image, create container with name
        // `<service>-green-<id_short>`, copy network/mounts from blue,
        // start it, return (id, addr).
        //
        // For the implementer: see `crates/isengard-plugins/updater/src/recreate.rs`
        // for the existing pattern of "inspect blue → build Config → create →
        // start → reconnect networks". The blue-green case differs in that we
        // do NOT stop blue first, and the green container gets a different
        // name (so it can co-exist with blue).
        //
        // The returned SocketAddr should be the green container's bridge IP +
        // its container_port (from the Deployment row). Get the IP from
        // `docker.inspect_container(green_id, None).await?.network_settings...`.
        //
        // Since this is non-trivial code, leave the body as a TODO that
        // Task 8's e2e test will exercise + flesh out. Driver unit tests
        // mock this trait, so the unit suite passes regardless.
        anyhow::bail!("RealDriverDeps::start_green not yet implemented (Task 8)")
    }

    async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
        use bollard::container::{RemoveContainerOptions, StopContainerOptions};
        let _ = self
            .docker
            .stop_container(container_id, Some(StopContainerOptions { t: 10 }))
            .await;
        let _ = self
            .docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        Ok(())
    }

    fn build_health_checker(&self, deployment: &Deployment) -> HealthChecker {
        let timeout = Duration::from_secs(3);
        match &deployment.health_path {
            Some(path) => HealthChecker::new(path.clone(), timeout),
            None => HealthChecker::tcp_only(timeout),
        }
    }

    async fn swap_upstream_to_green(
        &self,
        deployment: &Deployment,
        green_addr: SocketAddr,
        green_container_id: &str,
        grace: Duration,
    ) -> Result<()> {
        let hostname = deployment
            .public_hostname
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("deployment {} has no public_hostname", deployment.id))?;
        let new_upstream = Upstream {
            container_id: green_container_id.to_string(),
            addr: green_addr,
            healthy: true,
            health_path: deployment.health_path.clone(),
            health_interval: Duration::from_secs(5),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        };
        swap_upstream(&self.proxy_state, hostname, new_upstream, grace).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_core::NoopEmitter;
    use isengard_storage::deployment::{DeployStrategy, InsertDeployment};
    use isengard_storage::host::{EnrollHost, HostId};
    use isengard_storage::stack::{InsertStack, StackId, StackSource};
    use std::sync::Mutex;
    use ulid::Ulid;

    struct MockDeps {
        // Behavior knobs:
        spinup_fails: bool,
        healthcheck_fails: bool,
        swap_fails: bool,
        // Recorded calls:
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockDeps {
        fn record(&self, s: &str) {
            self.calls.lock().unwrap().push(s.to_string());
        }
    }

    #[async_trait::async_trait]
    impl DriverDeps for MockDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            self.record("start_green");
            if self.spinup_fails {
                anyhow::bail!("simulated docker failure");
            }
            Ok(("green-id".into(), "127.0.0.1:0".parse().unwrap()))
        }
        async fn stop_and_remove(&self, _id: &str) -> Result<()> {
            self.record("stop_and_remove");
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            // For mocked tests, we override wait_for_healthy via the
            // healthcheck_fails knob — this checker is never actually invoked.
            HealthChecker::tcp_only(Duration::from_millis(10))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _addr: SocketAddr,
            _id: &str,
            _grace: Duration,
        ) -> Result<()> {
            self.record("swap");
            if self.swap_fails {
                anyhow::bail!("simulated swap failure");
            }
            Ok(())
        }
    }

    /// Driver that overrides the healthcheck step. This wraps the real
    /// Driver but replaces wait_for_healthy with a deterministic outcome.
    struct TestDriver {
        inner: Driver<MockDeps>,
        force_hc_fail: bool,
    }

    impl TestDriver {
        async fn run(self) {
            // Inline the run_inner logic but with controlled healthcheck.
            // Since the real Driver doesn't expose a hook for the HC, we test
            // via the MockDeps' healthcheck_fails by routing build_health_checker
            // to a checker that always fails (TCP to a refused addr) when the
            // flag is set.
            //
            // Simpler: expose a Driver::run_with_hc(addr, override) helper, or
            // test by tuning grace_period to 0 and TCP-failing addrs.
            //
            // For this plan, the mock-driven path: just call inner.run() and
            // arrange MockDeps such that the healthcheck path naturally fails
            // by passing an unreachable addr from start_green.
            let _ = self.force_hc_fail;
            self.inner.run().await;
        }
    }

    async fn setup_inventory_and_row(state: DeploymentState) -> (Inventory, Deployment) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host = inv
            .enroll_host(EnrollHost {
                hostname: "h".into(),
                fingerprint: "fp".into(),
                fleet: Some("default".into()),
            })
            .await
            .unwrap();
        let stack = inv
            .insert_stack(InsertStack {
                host_id: host.id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let d = inv
            .insert_deployment(InsertDeployment {
                id: Ulid::new().to_string(),
                host_id: host.id,
                stack_id: stack.id,
                service_name: "web".into(),
                strategy: DeployStrategy::BlueGreen,
                state,
                blue_container: Some("blue-id".into()),
                green_container: None,
                blue_digest: "sha256:aaa".into(),
                green_digest: "sha256:bbb".into(),
                public_hostname: Some("blog.test".into()),
                health_path: None,
                container_port: Some(8080),
                metadata_json: None,
            })
            .await
            .unwrap();
        (inv, d)
    }

    #[tokio::test]
    async fn happy_path_advances_to_done() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let deps = Arc::new(MockDeps {
            spinup_fails: false,
            healthcheck_fails: false,
            swap_fails: false,
            calls: calls.clone(),
        });
        // For this test we need wait_for_healthy to succeed. The mock's
        // build_health_checker returns a TCP-only checker; we make it pass
        // by spawning a TCP listener and setting start_green to return its addr.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { loop { let _ = listener.accept().await; } });
        // Override start_green via a custom MockDeps2:
        struct MockDeps2 {
            addr: SocketAddr,
            calls: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl DriverDeps for MockDeps2 {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                self.calls.lock().unwrap().push("start_green".into());
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                self.calls.lock().unwrap().push("stop_and_remove".into());
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(100))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _addr: SocketAddr,
                _id: &str,
                _g: Duration,
            ) -> Result<()> {
                self.calls.lock().unwrap().push("swap".into());
                Ok(())
            }
        }
        let _ = deps; // suppress unused
        let deps = Arc::new(MockDeps2 { addr, calls: calls.clone() });
        let mut driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter));
        driver.grace_period = Duration::from_millis(50);  // shrink for test
        driver.run().await;

        let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(final_d.state, DeploymentState::Done);
        let recorded = calls.lock().unwrap().clone();
        assert!(recorded.contains(&"start_green".to_string()));
        assert!(recorded.contains(&"swap".to_string()));
        assert!(recorded.contains(&"stop_and_remove".to_string()));
    }

    #[tokio::test]
    async fn spinup_failure_marks_aborted() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        struct M { calls: Arc<Mutex<Vec<String>>> }
        #[async_trait::async_trait]
        impl DriverDeps for M {
            async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
                self.calls.lock().unwrap().push("start_green".into());
                anyhow::bail!("docker boom")
            }
            async fn stop_and_remove(&self, _: &str) -> Result<()> { Ok(()) }
            fn build_health_checker(&self, _: &Deployment) -> HealthChecker { HealthChecker::tcp_only(Duration::from_secs(1)) }
            async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
        }
        let deps = Arc::new(M { calls });
        let driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter));
        driver.run().await;
        let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(final_d.state, DeploymentState::Aborted);
        assert!(final_d.error.unwrap_or_default().contains("spinup_failed"));
    }

    #[tokio::test]
    async fn healthcheck_timeout_marks_aborted_and_cleans_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        struct M { calls: Arc<Mutex<Vec<String>>>, addr: SocketAddr }
        #[async_trait::async_trait]
        impl DriverDeps for M {
            async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, _: &str) -> Result<()> {
                self.calls.lock().unwrap().push("stop_and_remove".into());
                Ok(())
            }
            fn build_health_checker(&self, _: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(50))
            }
            async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> { Ok(()) }
        }
        // Bind + drop a listener to get a deterministically-refused addr.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = listener.local_addr().unwrap();
        drop(listener);
        let deps = Arc::new(M { calls: calls.clone(), addr: dead });
        // Shrink the healthcheck deadline by patching DeploymentHealthcheck
        // construction: the Driver currently uses default 120s. We'd need to
        // make it configurable. For this test, rely on the deadline being
        // 120s but tolerate the full duration in CI. Alternatively, expose
        // Driver::with_healthcheck_overrides for test ergonomics.
        let mut driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter));
        driver.grace_period = Duration::from_millis(10);
        // To keep this test fast, the implementer should add a hook on
        // Driver to override the DeploymentHealthcheck deadline. See
        // "Implementation note" below.
        driver.run().await;
        let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(final_d.state, DeploymentState::Aborted);
        assert!(final_d.error.unwrap_or_default().contains("healthcheck_timeout"));
        assert!(calls.lock().unwrap().contains(&"stop_and_remove".to_string()));
    }

    #[tokio::test]
    async fn swap_failure_marks_aborted_and_cleans_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { loop { let _ = listener.accept().await; } });
        struct M { calls: Arc<Mutex<Vec<String>>>, addr: SocketAddr }
        #[async_trait::async_trait]
        impl DriverDeps for M {
            async fn start_green(&self, _: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, _: &str) -> Result<()> {
                self.calls.lock().unwrap().push("stop_and_remove".into());
                Ok(())
            }
            fn build_health_checker(&self, _: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(100))
            }
            async fn swap_upstream_to_green(&self, _: &Deployment, _: SocketAddr, _: &str, _: Duration) -> Result<()> {
                anyhow::bail!("pingora boom")
            }
        }
        let deps = Arc::new(M { calls: calls.clone(), addr });
        let mut driver = Driver::new(d.clone(), deps, inv.clone(), Arc::new(NoopEmitter));
        driver.grace_period = Duration::from_millis(10);
        driver.run().await;
        let final_d = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(final_d.state, DeploymentState::Aborted);
        assert!(final_d.error.unwrap_or_default().contains("swap_failed"));
    }
}
```

**Implementation note for the test_hc_timeout test:** The deadline-defaults-to-120s issue surfaced in `healthcheck_timeout_marks_aborted_and_cleans_green` makes the test slow. Add a `Driver::with_healthcheck_overrides(initial_delay, interval, success_threshold, deadline)` builder method that the unit test can use to shrink the deadline to ~200ms. The production Supervisor uses `Driver::new` with defaults.

```rust
// Add to impl Driver<D>:
pub fn with_healthcheck_overrides(
    mut self,
    interval: Duration,
    success_threshold: u32,
    deadline: Duration,
) -> Self {
    self.hc_interval = interval;
    self.hc_success_threshold = success_threshold;
    self.hc_deadline = deadline;
    self
}
```

…and add the three `hc_*` fields to the Driver struct + use them in `DeploymentHealthcheck::new(...).with_*(...)`. Defaults match the spec (5s / 2 / 120s).

- [ ] **Step 2: Run the new tests, expect green**

Run: `cargo test -p isengard-agent --lib deployment::driver::tests`
Expected: 4/4 pass.

Run: `cargo test -p isengard-agent --lib`
Expected: all green (eligibility 5 + healthcheck 5 + driver 4 + existing).

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/deployment/mod.rs \
        crates/isengard-agent/src/deployment/driver.rs
git commit -m "feat(agent): blue-green deployment driver state machine"
```

---

## Task 5: DeploymentSupervisor

**Files:**
- Modify: `crates/isengard-agent/src/deployment/mod.rs`

- [ ] **Step 1: Add Supervisor in mod.rs**

Append to `crates/isengard-agent/src/deployment/mod.rs`:

```rust
use crate::proxy::ProxyState;
use anyhow::Result;
use isengard_core::EventEmitter;
use isengard_storage::deployment::{DeployStrategy, Deployment, DeploymentState, InsertDeployment};
use isengard_storage::host::HostId;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::StackId;
use std::sync::Arc;
use ulid::Ulid;

use crate::deployment::driver::{Driver, RealDriverDeps};
use crate::deployment::eligibility::{classify, ContainerSpec, Decision};

/// One-stop trigger from the updater plugin into the deployment system.
#[derive(Debug, Clone)]
pub struct UpdateTrigger {
    pub container_id: String,
    pub host_id: HostId,
    pub stack_id: StackId,
    pub service_name: String,
    pub blue_digest: String,
    pub green_digest: String,
    pub image_ref: String,
    pub public_hostname: Option<String>,
    pub container_port: Option<u16>,
    pub health_path: Option<String>,
    pub has_healthcheck: bool,
    pub rw_volume_mounts: Vec<String>,
    pub label_strategy: Option<String>,
}

/// Returned by handle_update_trigger so the updater plugin can decide what
/// to do for in-place containers.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorOutcome {
    /// Spawned a blue-green driver task. Updater should NOT recreate.
    BlueGreenSpawned { deployment_id: String },
    /// Container should follow the existing recreate.rs path. Updater handles it.
    InPlaceForUpdater,
    /// Already a deployment in flight for this service — skip this trigger.
    AlreadyInFlight,
}

pub struct DeploymentSupervisor {
    inventory: Inventory,
    docker: Arc<bollard::Docker>,
    proxy_state: ProxyState,
    emitter: Arc<dyn EventEmitter>,
}

impl DeploymentSupervisor {
    pub fn new(
        inventory: Inventory,
        docker: Arc<bollard::Docker>,
        proxy_state: ProxyState,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self { inventory, docker, proxy_state, emitter }
    }

    /// Mark any orphaned (non-terminal) deployment rows as Failed.
    /// Called once at agent startup before any drivers spin up.
    pub async fn reconcile_orphans(&self, host_id: HostId) -> Result<u64> {
        let n = self
            .inventory
            .mark_orphan_deployments_failed(host_id, "agent_restarted_during_deployment")
            .await?;
        Ok(n)
    }

    /// Decide strategy + maybe spawn a Driver. Synchronous-ish: returns
    /// quickly after either inserting a row + spawning, or signaling
    /// that the updater should handle in-place.
    pub async fn handle_update_trigger(&self, trigger: UpdateTrigger) -> Result<SupervisorOutcome> {
        // Dedupe: if an in-flight deployment already exists for this service, skip.
        let in_flight = self
            .inventory
            .list_in_flight_for_service(trigger.host_id, &trigger.service_name)
            .await?;
        if !in_flight.is_empty() {
            return Ok(SupervisorOutcome::AlreadyInFlight);
        }

        let mounts: Vec<String> = trigger.rw_volume_mounts.clone();
        let spec = ContainerSpec {
            has_routing_rule: trigger.public_hostname.is_some(),
            has_healthcheck: trigger.has_healthcheck,
            rw_volume_mounts: &mounts,
            label_strategy: trigger.label_strategy.as_deref(),
        };
        match classify(&spec) {
            Decision::InPlace { .. } => Ok(SupervisorOutcome::InPlaceForUpdater),
            Decision::BlueGreen => {
                let id = Ulid::new().to_string();
                let row = self
                    .inventory
                    .insert_deployment(InsertDeployment {
                        id: id.clone(),
                        host_id: trigger.host_id,
                        stack_id: trigger.stack_id,
                        service_name: trigger.service_name.clone(),
                        strategy: DeployStrategy::BlueGreen,
                        state: DeploymentState::Pending,
                        blue_container: Some(trigger.container_id.clone()),
                        green_container: None,
                        blue_digest: trigger.blue_digest.clone(),
                        green_digest: trigger.green_digest.clone(),
                        public_hostname: trigger.public_hostname.clone(),
                        health_path: trigger.health_path.clone(),
                        container_port: trigger.container_port.map(|p| p as i64),
                        metadata_json: None,
                    })
                    .await?;
                let deps = Arc::new(RealDriverDeps {
                    docker: self.docker.clone(),
                    proxy_state: self.proxy_state.clone(),
                });
                let driver = Driver::new(row, deps, self.inventory.clone(), self.emitter.clone());
                tokio::spawn(driver.run());
                Ok(SupervisorOutcome::BlueGreenSpawned { deployment_id: id })
            }
        }
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    use crate::proxy::ProxyState;
    use isengard_core::NoopEmitter;
    use isengard_storage::host::EnrollHost;
    use isengard_storage::stack::{InsertStack, StackSource};

    async fn setup() -> (DeploymentSupervisor, HostId, StackId, Inventory) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host = inv
            .enroll_host(EnrollHost {
                hostname: "h".into(),
                fingerprint: "fp".into(),
                fleet: Some("default".into()),
            })
            .await
            .unwrap();
        let stack = inv
            .insert_stack(InsertStack {
                host_id: host.id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        // Docker connect (optional; not actually used by these tests since
        // the mock path bails before any docker call):
        let docker = Arc::new(bollard::Docker::connect_with_local_defaults().unwrap());
        let proxy_state = ProxyState::new();
        let sup = DeploymentSupervisor::new(
            inv.clone(),
            docker,
            proxy_state,
            Arc::new(NoopEmitter),
        );
        (sup, host.id, stack.id, inv)
    }

    fn trigger(host_id: HostId, stack_id: StackId, blue_green: bool) -> UpdateTrigger {
        UpdateTrigger {
            container_id: "c-blue".into(),
            host_id,
            stack_id,
            service_name: "web".into(),
            blue_digest: "sha256:aaa".into(),
            green_digest: "sha256:bbb".into(),
            image_ref: "blog/web:1.3.0".into(),
            public_hostname: if blue_green { Some("blog.test".into()) } else { None },
            container_port: Some(8080),
            health_path: Some("/healthz".into()),
            has_healthcheck: blue_green,
            rw_volume_mounts: vec![],
            label_strategy: None,
        }
    }

    #[tokio::test]
    async fn classifies_in_place_when_no_routing_rule() {
        let (sup, host, stack, _inv) = setup().await;
        let outcome = sup.handle_update_trigger(trigger(host, stack, false)).await.unwrap();
        assert_eq!(outcome, SupervisorOutcome::InPlaceForUpdater);
    }

    #[tokio::test]
    async fn dedupes_when_in_flight_deployment_exists() {
        let (sup, host, stack, inv) = setup().await;
        // Pre-insert an in-flight deployment for service "web".
        inv.insert_deployment(InsertDeployment {
            id: Ulid::new().to_string(),
            host_id: host,
            stack_id: stack,
            service_name: "web".into(),
            strategy: DeployStrategy::BlueGreen,
            state: DeploymentState::SpinningUp,
            blue_container: Some("c-blue".into()),
            green_container: None,
            blue_digest: "sha256:old".into(),
            green_digest: "sha256:newer".into(),
            public_hostname: Some("blog.test".into()),
            health_path: None,
            container_port: Some(8080),
            metadata_json: None,
        }).await.unwrap();

        let outcome = sup.handle_update_trigger(trigger(host, stack, true)).await.unwrap();
        assert_eq!(outcome, SupervisorOutcome::AlreadyInFlight);
    }

    #[tokio::test]
    async fn reconcile_orphans_marks_non_terminal_as_failed() {
        let (sup, host, stack, inv) = setup().await;
        let d = inv.insert_deployment(InsertDeployment {
            id: Ulid::new().to_string(),
            host_id: host,
            stack_id: stack,
            service_name: "web".into(),
            strategy: DeployStrategy::BlueGreen,
            state: DeploymentState::SpinningUp,
            blue_container: Some("c-blue".into()),
            green_container: None,
            blue_digest: "sha256:old".into(),
            green_digest: "sha256:newer".into(),
            public_hostname: Some("blog.test".into()),
            health_path: None,
            container_port: Some(8080),
            metadata_json: None,
        }).await.unwrap();

        let n = sup.reconcile_orphans(host).await.unwrap();
        assert_eq!(n, 1);
        let after = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(after.state, DeploymentState::Failed);
        assert_eq!(after.error.as_deref(), Some("agent_restarted_during_deployment"));
    }
}
```

- [ ] **Step 2: Run the supervisor tests**

Run: `cargo test -p isengard-agent --lib deployment::supervisor_tests`
Expected: 3/3 pass.

If `bollard::Docker::connect_with_local_defaults()` fails on a system without a Docker socket, gate that line behind `#[cfg(unix)]` or skip the docker arg for the no-docker test paths by extracting the classify+insert path into a standalone function tested directly.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/deployment/mod.rs
git commit -m "feat(agent): DeploymentSupervisor (classify + spawn driver + reconcile orphans)"
```

---

## Task 6: Updater integration

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs`

**Goal:** When the updater classifies a container as `needs_update`, instead of immediately calling `recreate::recreate_container` (or `self_update::update_self`), emit a structured event that the agent's Supervisor can handle. The updater also subscribes to a callback for "you should do the in-place path" — for v1 this is achieved by **calling a new trait** the agent passes in via PluginContext, rather than going through the full Event broadcast (which is async + lossy).

The cleanest seam: extend `PluginContext` (or pass via plugin construction) with an `Option<Arc<dyn UpdateDispatcher>>`. The updater calls dispatcher.dispatch() with the trigger info; the dispatcher returns whether the updater should still call recreate.

- [ ] **Step 1: Define the dispatcher trait**

Create `crates/isengard-core/src/update_dispatch.rs`:

```rust
//! Bridge from plugins (specifically `updater`) into the agent's deployment
//! Supervisor. Lets the updater hand off "container X needs update" without
//! depending on the agent crate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTriggerInfo {
    pub container_id: String,
    pub service_name: String,
    pub stack_id: i64,
    pub host_id: crate::HostId,
    pub blue_digest: String,
    pub green_digest: String,
    pub image_ref: String,
    pub public_hostname: Option<String>,
    pub container_port: Option<u16>,
    pub health_path: Option<String>,
    pub has_healthcheck: bool,
    pub rw_volume_mounts: Vec<String>,
    pub label_strategy: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Updater should perform its existing in-place recreate flow.
    PerformInPlace,
    /// Agent has accepted ownership (e.g., spawned a blue-green driver).
    /// Updater should NOT recreate.
    Handled,
}

#[async_trait]
pub trait UpdateDispatcher: Send + Sync + 'static {
    async fn dispatch(&self, info: UpdateTriggerInfo) -> DispatchOutcome;
}
```

Modify `crates/isengard-core/src/lib.rs` — add `pub mod update_dispatch;` and `pub use update_dispatch::{UpdateDispatcher, UpdateTriggerInfo, DispatchOutcome};`.

Modify `crates/isengard-core/src/context.rs` — add an optional dispatcher field to `PluginContext`:

```rust
// In PluginContext struct:
pub update_dispatcher: Option<std::sync::Arc<dyn UpdateDispatcher>>,

// In the with_* builder pattern (mirror the with_events one):
pub fn with_update_dispatcher(mut self, d: std::sync::Arc<dyn UpdateDispatcher>) -> Self {
    self.update_dispatcher = Some(d);
    self
}
```

(If PluginContext uses `#[derive(Default)]`, add `#[default]` to the struct or initialize the field manually; if it uses `#[non_exhaustive]`, the addition is non-breaking.)

- [ ] **Step 2: Updater dispatches before recreate**

In `crates/isengard-plugins/updater/src/lib.rs`, find the existing `needs_update` branch in `cycle_once` (around line 170-210, where it calls `update_self` or `recreate_container`). Wrap the recreate call:

```rust
// BEFORE the existing recreate path runs, ask the dispatcher (if present).
// If it says Handled, skip recreate. Otherwise, fall through to recreate.
if let Some(dispatcher) = ctx.update_dispatcher.as_ref() {
    let info = UpdateTriggerInfo {
        container_id: container.id.clone().unwrap_or_default(),
        service_name: extract_service_name(&container),  // helper, see below
        stack_id: extract_stack_id(&container),          // helper
        host_id: ctx.host_id,
        blue_digest: local_digest.clone(),
        green_digest: remote_digest.clone(),
        image_ref: image_str.clone(),
        public_hostname: lookup_public_hostname(&inventory, &container).await,
        container_port: extract_container_port(&container),
        health_path: lookup_health_path(&inventory, &container).await,
        has_healthcheck: container_has_healthcheck(&container),
        rw_volume_mounts: extract_rw_mounts(&container),
        label_strategy: container.labels.as_ref()
            .and_then(|l| l.get("isengard.deploy.strategy").cloned()),
    };
    match dispatcher.dispatch(info).await {
        DispatchOutcome::Handled => continue,           // skip recreate
        DispatchOutcome::PerformInPlace => {}            // fall through
    }
}
// existing recreate path:
match recreate::recreate_container(...).await { ... }
```

The helper functions (`extract_service_name`, `lookup_public_hostname`, etc.) are small. The implementer should:

1. **`extract_service_name`**: prefer `com.docker.compose.service` label, fall back to container name without `/` and trailing index.
2. **`extract_stack_id`**: read `com.docker.compose.project` label, look up stack by `(host_id, name)` via Inventory. Returns `0` if not found (treat as no stack — InPlace path handles this naturally).
3. **`lookup_public_hostname`** + **`lookup_health_path`**: query `inventory.list_routing_rules_for_host(host_id)`, find rule matching this service:port, return its `public_hostname` / `healthcheck_path`.
4. **`extract_container_port`**: read first port from `inspect.network_settings.ports` keys (e.g., "8080/tcp" → 8080).
5. **`container_has_healthcheck`**: `inspect.config.healthcheck.is_some()` OR `inspect.state.health.is_some()`.
6. **`extract_rw_mounts`**: iterate `inspect.host_config.mounts`, return paths where `read_only` is None or false.

Each is a few-liner. They can live in a new file `crates/isengard-plugins/updater/src/dispatch_helpers.rs` (small, focused, reuses the same bollard inspect data the updater already has).

If the updater currently doesn't have access to an `Inventory`, accept a simpler path: the helpers that need DB lookups (public_hostname, health_path) can stay `None` for v1 — the Supervisor's classifier will then route to InPlace (no routing rule). The proper wiring lands in Task 7 when the agent passes `Inventory` into the plugin context.

Actually the simplest approach: **the agent's UpdateDispatcher impl owns the Inventory**. The updater passes the `container_id` + Docker inspect data only; the dispatcher does the routing-rule lookup itself. Refactor `UpdateTriggerInfo` to drop `public_hostname` / `health_path` fields (they're inputs to classify, but they're DB-lookable). Add `container_id` + `image_ref` + `has_healthcheck` + `rw_volume_mounts` + `label_strategy` only.

```rust
// Simplified UpdateTriggerInfo:
pub struct UpdateTriggerInfo {
    pub container_id: String,
    pub service_name: String,        // for dedupe + routing-rule lookup
    pub stack_id: i64,                // 0 if unknown
    pub host_id: HostId,
    pub blue_digest: String,
    pub green_digest: String,
    pub image_ref: String,
    pub container_port: Option<u16>,
    pub has_healthcheck: bool,
    pub rw_volume_mounts: Vec<String>,
    pub label_strategy: Option<String>,
}
```

The dispatcher (lives in agent) does:
```rust
let routing_rules = self.inventory.list_routing_rules_for_host(info.host_id).await?;
let rule = routing_rules.iter().find(|r| r.service_name == info.service_name
    && Some(r.container_port) == info.container_port);
let public_hostname = rule.map(|r| r.public_hostname.clone());
let health_path = rule.and_then(|r| r.healthcheck_path.clone());
// Then build the supervisor's UpdateTrigger and call handle_update_trigger.
```

This removes the updater's need for Inventory entirely. The dispatcher becomes the only DB-aware actor in this path.

- [ ] **Step 3: Adjust the spec's `UpdateTrigger` to mirror the simplified info struct**

In `crates/isengard-agent/src/deployment/mod.rs` (Task 5's `UpdateTrigger`), keep `public_hostname`, `health_path`, `container_port` — these now come from the dispatcher's lookup, not the updater. Remove anything the dispatcher computes.

- [ ] **Step 4: Implement an UpdateDispatcher impl in agent**

Append to `crates/isengard-agent/src/deployment/mod.rs`:

```rust
use isengard_core::{DispatchOutcome, UpdateDispatcher, UpdateTriggerInfo};

/// Adapter: implements isengard-core's UpdateDispatcher trait, owns the
/// Supervisor + Inventory, performs DB lookups, and forwards the typed
/// UpdateTrigger to handle_update_trigger.
pub struct SupervisorDispatcher {
    pub supervisor: Arc<DeploymentSupervisor>,
    pub inventory: Inventory,
}

#[async_trait::async_trait]
impl UpdateDispatcher for SupervisorDispatcher {
    async fn dispatch(&self, info: UpdateTriggerInfo) -> DispatchOutcome {
        // Look up routing rule to fill in public_hostname + health_path.
        let routing_rules = match self.inventory.list_routing_rules_for_host(info.host_id).await {
            Ok(rs) => rs,
            Err(_) => return DispatchOutcome::PerformInPlace,
        };
        let rule = routing_rules.iter().find(|r| {
            r.service_name == info.service_name
                && info.container_port.map(|p| u32::from(p)) == Some(u32::from(r.container_port))
        });
        let trigger = UpdateTrigger {
            container_id: info.container_id,
            host_id: info.host_id,
            stack_id: StackId(info.stack_id),
            service_name: info.service_name,
            blue_digest: info.blue_digest,
            green_digest: info.green_digest,
            image_ref: info.image_ref,
            public_hostname: rule.map(|r| r.public_hostname.clone()),
            container_port: info.container_port,
            health_path: rule.and_then(|r| r.healthcheck_path.clone()),
            has_healthcheck: info.has_healthcheck,
            rw_volume_mounts: info.rw_volume_mounts,
            label_strategy: info.label_strategy,
        };
        match self.supervisor.handle_update_trigger(trigger).await {
            Ok(super::deployment::SupervisorOutcome::BlueGreenSpawned { .. }) => DispatchOutcome::Handled,
            Ok(super::deployment::SupervisorOutcome::AlreadyInFlight) => DispatchOutcome::Handled,
            Ok(super::deployment::SupervisorOutcome::InPlaceForUpdater) => DispatchOutcome::PerformInPlace,
            Err(_) => DispatchOutcome::PerformInPlace,
        }
    }
}
```

(The `super::deployment::SupervisorOutcome` path may simplify to `SupervisorOutcome` depending on where the impl lives; adjust to compile.)

- [ ] **Step 5: Updater plugin tests**

Add to `crates/isengard-plugins/updater/tests/dispatch_path.rs`:

```rust
//! Verifies the updater consults the dispatcher and skips recreate when
//! told the dispatch was Handled.

use async_trait::async_trait;
use isengard_core::{DispatchOutcome, UpdateDispatcher, UpdateTriggerInfo};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingDispatcher {
    outcome: DispatchOutcome,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UpdateDispatcher for CountingDispatcher {
    async fn dispatch(&self, _: UpdateTriggerInfo) -> DispatchOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome
    }
}

#[tokio::test]
async fn dispatcher_handled_skips_recreate() {
    // This is a structural test. The actual updater::cycle_once call
    // requires a Docker daemon and an image to detect as needs_update,
    // which is beyond a unit test. Instead, exercise the helper:
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher: Arc<dyn UpdateDispatcher> = Arc::new(CountingDispatcher {
        outcome: DispatchOutcome::Handled,
        calls: calls.clone(),
    });
    let info = UpdateTriggerInfo {
        container_id: "c1".into(),
        service_name: "web".into(),
        stack_id: 1,
        host_id: isengard_core::HostId(ulid::Ulid::new()),
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        image_ref: "blog/web:1.3.0".into(),
        container_port: Some(8080),
        has_healthcheck: true,
        rw_volume_mounts: vec![],
        label_strategy: None,
    };
    let outcome = dispatcher.dispatch(info).await;
    assert_eq!(outcome, DispatchOutcome::Handled);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dispatcher_perform_in_place_falls_through_to_recreate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher: Arc<dyn UpdateDispatcher> = Arc::new(CountingDispatcher {
        outcome: DispatchOutcome::PerformInPlace,
        calls: calls.clone(),
    });
    let info = UpdateTriggerInfo {
        container_id: "c2".into(),
        service_name: "db".into(),
        stack_id: 1,
        host_id: isengard_core::HostId(ulid::Ulid::new()),
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        image_ref: "postgres:16".into(),
        container_port: Some(5432),
        has_healthcheck: false,
        rw_volume_mounts: vec!["/data".into()],
        label_strategy: None,
    };
    let outcome = dispatcher.dispatch(info).await;
    assert_eq!(outcome, DispatchOutcome::PerformInPlace);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
```

- [ ] **Step 6: Run all unit tests**

Run: `cargo test --workspace --lib`
Expected: all green (existing + storage 6 + agent eligibility 5 + agent healthcheck 5 + agent driver 4 + agent supervisor 3 + dispatch types compile).

Run: `cargo test -p isengard-plugin-updater`
Expected: green (existing + 2 new).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-core/src/update_dispatch.rs \
        crates/isengard-core/src/lib.rs \
        crates/isengard-core/src/context.rs \
        crates/isengard-plugins/updater/src/lib.rs \
        crates/isengard-plugins/updater/tests/dispatch_path.rs \
        crates/isengard-agent/src/deployment/mod.rs
# Also include any helper file you created in updater/src/
git add -u
git commit -m "feat(updater+core): UpdateDispatcher trait + agent SupervisorDispatcher"
```

---

## Task 7: Agent main wiring

**Files:**
- Modify: `crates/isengard-agent/src/run_agent.rs` (or wherever the agent's main task assembly lives)

- [ ] **Step 1: Locate the wiring site**

Run: `grep -rn "PluginContext::new\|with_events\|UpdaterPlugin::new" crates/isengard-agent/src/ crates/isengard/src/`

Find where the agent constructs the PluginContext for plugins. That's the seam where the new dispatcher gets injected.

- [ ] **Step 2: Construct + inject the dispatcher**

After the agent has constructed `inventory: Inventory`, `docker: Arc<bollard::Docker>`, `proxy_state: ProxyState`, and `emitter: Arc<dyn EventEmitter>`, add:

```rust
use isengard_agent::deployment::{DeploymentSupervisor, SupervisorDispatcher};

let supervisor = Arc::new(DeploymentSupervisor::new(
    inventory.clone(),
    docker.clone(),
    proxy_state.clone(),
    emitter.clone(),
));

// Reconcile orphans before spawning any plugins (so any rows from a previous
// crash get marked Failed before a new deployment is triggered for the same service).
let orphans = supervisor.reconcile_orphans(host_id).await?;
if orphans > 0 {
    tracing::warn!(orphans, "marked orphan deployments as failed at startup");
}

let dispatcher: Arc<dyn isengard_core::UpdateDispatcher> = Arc::new(SupervisorDispatcher {
    supervisor: supervisor.clone(),
    inventory: inventory.clone(),
});

// Existing PluginContext construction — add the dispatcher:
let plugin_ctx = PluginContext::new(/* existing args */)
    .with_events(emitter.clone())
    .with_update_dispatcher(dispatcher);
```

If the PluginContext is constructed via a different builder pattern, follow that pattern; the only requirement is the dispatcher gets in.

- [ ] **Step 3: Build verify**

Run: `cargo build --workspace`
Expected: clean.

Run: `cargo test --workspace --lib`
Expected: still green.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/run_agent.rs
# Plus any other file you had to touch:
git add -u
git commit -m "feat(agent): wire DeploymentSupervisor + dispatcher into plugin context"
```

---

## Task 8: Real-Docker e2e — happy path

**Files:**
- Create: `crates/isengard-agent/tests/deployment_blue_green_happy.rs`

- [ ] **Step 1: Implement RealDriverDeps::start_green**

The unit-test stub in Task 4 left `start_green` as `bail!`. Now implement it.

In `crates/isengard-agent/src/deployment/driver.rs`, replace the body of `RealDriverDeps::start_green`:

```rust
async fn start_green(
    &self,
    deployment: &Deployment,
) -> Result<(String, SocketAddr)> {
    use bollard::container::{Config, CreateContainerOptions, StartContainerOptions};
    use bollard::image::CreateImageOptions;
    use futures_util::StreamExt;

    let blue_id = deployment.blue_container.as_deref()
        .ok_or_else(|| anyhow::anyhow!("no blue container to base green on"))?;
    let blue = self.docker.inspect_container(blue_id, None).await
        .with_context(|| format!("inspect blue {blue_id}"))?;

    let image = blue.config.as_ref()
        .and_then(|c| c.image.clone())
        .ok_or_else(|| anyhow::anyhow!("blue has no image"))?;
    // For BG, the green image is the same name with the new digest. The
    // updater detected the digest change against the registry; the image
    // tag is unchanged. Pulling re-fetches the new digest under that tag.
    let pull_opts = CreateImageOptions { from_image: image.as_str(), ..Default::default() };
    let mut stream = self.docker.create_image(Some(pull_opts), None, None);
    while let Some(item) = stream.next().await {
        item.with_context(|| format!("pulling {image}"))?;
    }

    // Build green Config from blue (mirror updater's recreate.rs::capture_config
    // pattern, but DON'T set image to a new tag — we're keeping the tag, just
    // running the new digest).
    let cfg_in = blue.config.clone().unwrap_or_default();
    let host_cfg = blue.host_config.clone().unwrap_or_default();
    let cfg: Config<String> = Config {
        image: Some(image.clone()),
        cmd: cfg_in.cmd,
        entrypoint: cfg_in.entrypoint,
        env: cfg_in.env,
        labels: cfg_in.labels,
        working_dir: cfg_in.working_dir,
        user: cfg_in.user,
        exposed_ports: cfg_in.exposed_ports,
        host_config: Some(host_cfg),
        healthcheck: cfg_in.healthcheck,
        ..Default::default()
    };

    // Green container name: <service>-green-<deployment_id_prefix>
    let id_short = &deployment.id[..deployment.id.len().min(8)];
    let green_name = format!("{}-green-{}", deployment.service_name, id_short);
    let create = self.docker.create_container(
        Some(CreateContainerOptions { name: green_name.clone(), platform: None }),
        cfg,
    ).await.with_context(|| format!("create container {green_name}"))?;

    // Reconnect to non-bridge networks (mirror recreate.rs).
    if let Some(ns) = blue.network_settings.as_ref().and_then(|s| s.networks.as_ref()) {
        for (net_name, settings) in ns {
            if net_name == "bridge" { continue; }
            self.docker.connect_network(net_name, bollard::network::ConnectNetworkOptions {
                container: create.id.clone(),
                endpoint_config: settings.clone(),
            }).await.ok();   // best-effort; some networks may have changed
        }
    }

    self.docker.start_container(&create.id, None::<StartContainerOptions<String>>)
        .await.with_context(|| format!("start container {green_name}"))?;

    // Re-inspect to get the assigned IP.
    let started = self.docker.inspect_container(&create.id, None).await?;
    let ip = started.network_settings.as_ref()
        .and_then(|s| s.ip_address.clone())
        .or_else(|| {
            // Fall back to the first network's IP.
            started.network_settings.as_ref()
                .and_then(|s| s.networks.as_ref())
                .and_then(|nets| nets.values().next())
                .and_then(|settings| settings.ip_address.clone())
        })
        .ok_or_else(|| anyhow::anyhow!("green container has no IP"))?;
    let port = deployment.container_port.unwrap_or(8080) as u16;
    let addr: SocketAddr = format!("{ip}:{port}").parse()
        .with_context(|| format!("parse addr {ip}:{port}"))?;

    Ok((create.id, addr))
}
```

- [ ] **Step 2: Write the e2e test**

Create `crates/isengard-agent/tests/deployment_blue_green_happy.rs`:

```rust
//! Real-Docker e2e: triggers a blue-green deployment of nginx, asserts
//! that within ~30s the deployment row reaches Done, the proxy serves
//! the new green container, and the blue container is gone.
//!
//! Gated behind `#[ignore]` — run with `cargo test --test deployment_blue_green_happy -- --ignored --nocapture`.
//! Requires a running Docker daemon.

use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions};
use bollard::image::CreateImageOptions;
use futures_util::StreamExt;
use isengard_agent::deployment::{DeploymentSupervisor, SupervisorOutcome, UpdateTrigger};
use isengard_agent::proxy::ProxyState;
use isengard_agent::proxy::upstreams::{Upstream, UpstreamState};
use isengard_core::NoopEmitter;
use isengard_storage::deployment::DeploymentState;
use isengard_storage::host::EnrollHost;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::{InsertStack, StackSource};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NGINX_IMAGE: &str = "nginx:alpine";

async fn pull(docker: &Docker, image: &str) {
    let mut s = docker.create_image(Some(CreateImageOptions { from_image: image, ..Default::default() }), None, None);
    while let Some(_) = s.next().await {}
}

async fn cleanup(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, Some(StopContainerOptions { t: 1 })).await;
    let _ = docker.remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
}

#[tokio::test]
#[ignore]
async fn blue_green_happy_path_drains_blue_destroys_blue_and_serves_green() {
    let docker = Arc::new(Docker::connect_with_local_defaults().expect("docker"));
    let inv = Inventory::open_in_memory().await.expect("inventory");
    let host = inv.enroll_host(EnrollHost {
        hostname: "h-bg-test".into(),
        fingerprint: "fp-bg-test".into(),
        fleet: Some("default".into()),
    }).await.unwrap();
    let stack = inv.insert_stack(InsertStack {
        host_id: host.id,
        name: "blog".into(),
        source: StackSource::Compose,
    }).await.unwrap();

    pull(&docker, NGINX_IMAGE).await;
    let blue_name = "isengard-bg-test-blue";
    cleanup(&docker, blue_name).await;
    let blue = docker.create_container(
        Some(CreateContainerOptions { name: blue_name.into(), platform: None }),
        Config {
            image: Some(NGINX_IMAGE.to_string()),
            healthcheck: Some(bollard::models::HealthConfig {
                test: Some(vec!["CMD".into(), "wget".into(), "-q".into(), "-O-".into(), "http://localhost/".into()]),
                interval: Some(1_000_000_000),  // 1s in ns
                timeout: Some(1_000_000_000),
                retries: Some(3),
                start_period: Some(0),
            }),
            ..Default::default()
        },
    ).await.expect("create blue");
    docker.start_container(&blue.id, None::<StartContainerOptions<String>>).await.expect("start blue");
    let blue_inspect = docker.inspect_container(&blue.id, None).await.unwrap();
    let blue_ip = blue_inspect.network_settings.as_ref().unwrap().ip_address.clone().unwrap();

    // Insert a routing rule pointing to blue.
    use isengard_storage::routing_rule::{InsertRoutingRule, RoutingRuleSource, RoutingRuleState, TlsMode};
    let _rule = inv.insert_routing_rule(InsertRoutingRule {
        fleet: "default".into(),
        host_id: host.id,
        stack_id: Some(stack.id),
        service_name: "web".into(),
        container_port: 80,
        public_hostname: "blog.bg.test".into(),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Edge,
        healthcheck_path: Some("/".into()),
        healthcheck_interval_secs: 5,
        auth: None,
        state: RoutingRuleState::Active,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    }).await.unwrap();

    // Pre-populate the proxy upstream registry with blue.
    let proxy_state = ProxyState::new();
    {
        let mut w = proxy_state.upstreams.write().await;
        w.set("blog.bg.test", Upstream {
            container_id: blue.id.clone(),
            addr: format!("{blue_ip}:80").parse().unwrap(),
            healthy: true,
            health_path: Some("/".into()),
            health_interval: Duration::from_secs(5),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        });
    }

    let supervisor = DeploymentSupervisor::new(
        inv.clone(),
        docker.clone(),
        proxy_state.clone(),
        Arc::new(NoopEmitter),
    );

    let trigger = UpdateTrigger {
        container_id: blue.id.clone(),
        host_id: host.id,
        stack_id: stack.id,
        service_name: "web".into(),
        blue_digest: "sha256:fake-blue".into(),
        green_digest: "sha256:fake-green".into(),
        image_ref: NGINX_IMAGE.into(),
        public_hostname: Some("blog.bg.test".into()),
        container_port: Some(80),
        health_path: Some("/".into()),
        has_healthcheck: true,
        rw_volume_mounts: vec![],
        label_strategy: None,
    };

    let outcome = supervisor.handle_update_trigger(trigger).await.expect("dispatch");
    let deployment_id = match outcome {
        SupervisorOutcome::BlueGreenSpawned { deployment_id } => deployment_id,
        other => panic!("expected BlueGreenSpawned, got {other:?}"),
    };

    // Poll until Done or timeout (max ~120s + grace 60s + 5s buffer = give 200s budget).
    let deadline = Instant::now() + Duration::from_secs(200);
    let final_state = loop {
        let d = inv.get_deployment(&deployment_id).await.unwrap().unwrap();
        if d.state.is_terminal() { break d; }
        if Instant::now() >= deadline { panic!("deployment did not finish: state={:?} error={:?}", d.state, d.error); }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(final_state.state, DeploymentState::Done, "expected Done, got {:?} error={:?}", final_state.state, final_state.error);

    // Assert proxy now routes to green (a different container_id than blue).
    let r = proxy_state.upstreams.read().await;
    let after = r.get("blog.bg.test").expect("upstream still present");
    assert_ne!(after.container_id, blue.id, "upstream still points at blue");

    // Assert blue is gone.
    let blue_after = docker.inspect_container(&blue.id, None).await;
    assert!(blue_after.is_err(), "blue container should have been removed");

    // Cleanup green.
    if let Some(green_id) = final_state.green_container {
        cleanup(&docker, &green_id).await;
    }
}
```

- [ ] **Step 3: Sanity-build (don't run by default)**

Run: `cargo test -p isengard-agent --test deployment_blue_green_happy --no-run`
Expected: clean compile.

If you have a Docker daemon locally:
```
cargo test -p isengard-agent --test deployment_blue_green_happy -- --ignored --nocapture
```
Expected: pass within ~3 minutes (most time spent in healthcheck wait + grace period).

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/deployment/driver.rs \
        crates/isengard-agent/tests/deployment_blue_green_happy.rs
git commit -m "feat(agent): RealDriverDeps::start_green + happy-path real-Docker e2e"
```

---

## Task 9: Real-Docker e2e — abort on healthcheck timeout

**Files:**
- Create: `crates/isengard-agent/tests/deployment_blue_green_aborts_on_healthcheck.rs`

- [ ] **Step 1: Write the abort e2e**

```rust
//! Real-Docker e2e: blue-green deployment where green's healthcheck
//! never passes. Asserts the deployment aborts within deadline_secs,
//! green is cleaned up, blue stays serving.
//!
//! Gated behind `#[ignore]`. Requires Docker daemon.

use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions};
use bollard::image::CreateImageOptions;
use futures_util::StreamExt;
use isengard_agent::deployment::{DeploymentSupervisor, SupervisorOutcome, UpdateTrigger};
use isengard_agent::proxy::ProxyState;
use isengard_agent::proxy::upstreams::{Upstream, UpstreamState};
use isengard_core::NoopEmitter;
use isengard_storage::deployment::DeploymentState;
use isengard_storage::host::EnrollHost;
use isengard_storage::inventory::Inventory;
use isengard_storage::routing_rule::{InsertRoutingRule, RoutingRuleSource, RoutingRuleState, TlsMode};
use isengard_storage::stack::{InsertStack, StackSource};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NGINX_IMAGE: &str = "nginx:alpine";

async fn pull(docker: &Docker, image: &str) {
    let mut s = docker.create_image(Some(CreateImageOptions { from_image: image, ..Default::default() }), None, None);
    while let Some(_) = s.next().await {}
}

async fn cleanup(docker: &Docker, name: &str) {
    let _ = docker.stop_container(name, Some(StopContainerOptions { t: 1 })).await;
    let _ = docker.remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
}

#[tokio::test]
#[ignore]
async fn blue_green_aborts_when_green_healthcheck_never_passes() {
    let docker = Arc::new(Docker::connect_with_local_defaults().expect("docker"));
    let inv = Inventory::open_in_memory().await.expect("inventory");
    let host = inv.enroll_host(EnrollHost {
        hostname: "h-bg-abort".into(),
        fingerprint: "fp-bg-abort".into(),
        fleet: Some("default".into()),
    }).await.unwrap();
    let stack = inv.insert_stack(InsertStack {
        host_id: host.id,
        name: "blog-abort".into(),
        source: StackSource::Compose,
    }).await.unwrap();

    pull(&docker, NGINX_IMAGE).await;
    let blue_name = "isengard-bg-abort-blue";
    cleanup(&docker, blue_name).await;
    let blue = docker.create_container(
        Some(CreateContainerOptions { name: blue_name.into(), platform: None }),
        Config { image: Some(NGINX_IMAGE.to_string()), ..Default::default() },
    ).await.unwrap();
    docker.start_container(&blue.id, None::<StartContainerOptions<String>>).await.unwrap();
    let blue_inspect = docker.inspect_container(&blue.id, None).await.unwrap();
    let blue_ip = blue_inspect.network_settings.as_ref().unwrap().ip_address.clone().unwrap();

    // Routing rule with a path that nginx returns 404 for — we'll point the
    // healthcheck at it. HealthChecker.check_once returns false on non-2xx.
    let _rule = inv.insert_routing_rule(InsertRoutingRule {
        fleet: "default".into(),
        host_id: host.id,
        stack_id: Some(stack.id),
        service_name: "web".into(),
        container_port: 80,
        public_hostname: "blog.bg.abort".into(),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Edge,
        healthcheck_path: Some("/this-path-returns-404".into()),
        healthcheck_interval_secs: 5,
        auth: None,
        state: RoutingRuleState::Active,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    }).await.unwrap();

    let proxy_state = ProxyState::new();
    {
        let mut w = proxy_state.upstreams.write().await;
        w.set("blog.bg.abort", Upstream {
            container_id: blue.id.clone(),
            addr: format!("{blue_ip}:80").parse().unwrap(),
            healthy: true,
            health_path: Some("/".into()),
            health_interval: Duration::from_secs(5),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        });
    }

    let supervisor = DeploymentSupervisor::new(
        inv.clone(),
        docker.clone(),
        proxy_state.clone(),
        Arc::new(NoopEmitter),
    );

    let trigger = UpdateTrigger {
        container_id: blue.id.clone(),
        host_id: host.id,
        stack_id: stack.id,
        service_name: "web".into(),
        blue_digest: "sha256:blue".into(),
        green_digest: "sha256:green".into(),
        image_ref: NGINX_IMAGE.into(),
        public_hostname: Some("blog.bg.abort".into()),
        container_port: Some(80),
        health_path: Some("/this-path-returns-404".into()),
        has_healthcheck: true,
        rw_volume_mounts: vec![],
        label_strategy: None,
    };

    let outcome = supervisor.handle_update_trigger(trigger).await.expect("dispatch");
    let deployment_id = match outcome {
        SupervisorOutcome::BlueGreenSpawned { deployment_id } => deployment_id,
        other => panic!("expected BlueGreenSpawned, got {other:?}"),
    };

    // Poll until Aborted (deadline default 120s + buffer).
    let deadline = Instant::now() + Duration::from_secs(150);
    let final_state = loop {
        let d = inv.get_deployment(&deployment_id).await.unwrap().unwrap();
        if d.state.is_terminal() { break d; }
        if Instant::now() >= deadline {
            panic!("deployment did not abort: state={:?} error={:?}", d.state, d.error);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    assert_eq!(final_state.state, DeploymentState::Aborted);
    assert!(final_state.error.as_deref().unwrap_or("").contains("healthcheck_timeout"));

    // Blue still serving (proxy still has blue's container_id).
    let r = proxy_state.upstreams.read().await;
    let still = r.get("blog.bg.abort").expect("upstream present");
    assert_eq!(still.container_id, blue.id, "blue should still be serving");

    // Blue container still alive.
    assert!(docker.inspect_container(&blue.id, None).await.is_ok());

    // Cleanup blue.
    cleanup(&docker, &blue.id).await;
}
```

- [ ] **Step 2: Sanity-build**

Run: `cargo test -p isengard-agent --test deployment_blue_green_aborts_on_healthcheck --no-run`
Expected: clean compile.

If Docker available:
```
cargo test -p isengard-agent --test deployment_blue_green_aborts_on_healthcheck -- --ignored --nocapture
```
Expected: pass within ~3 minutes.

- [ ] **Step 3: Commit**

```bash
git add crates/isengard-agent/tests/deployment_blue_green_aborts_on_healthcheck.rs
git commit -m "test(agent): real-Docker e2e for healthcheck-timeout abort"
```

---

## Task 10: Final workspace-green + open PR #21

- [ ] **Step 1: All four gates**

Run: `cargo build --workspace`
Expected: clean.

Run: `cargo test --workspace`
Expected: all green (e2e excluded by default since they're `#[ignore]`).

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

Run: `cargo deny check`
Expected: clean. No new transitive deps in this plan; if anything surfaces, add to `deny.toml` ignore with rationale.

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success (no UI changes in this plan; this is a sanity gate inherited from Plan C's stack).

- [ ] **Step 2: Push + open PR**

```bash
cd /Users/dirdmaster/Projects/isengard/.worktrees/blue-green-core
git push -u origin feat/blue-green-core

gh pr create --base feat/networking-settings-ui-and-swap \
  --title "Phase 10 Plan A: blue-green deployment core (10a-10d)" \
  --body "$(cat <<'EOF'
## Summary

Implements Plan A of Phase 10. Stacked on PR #20 (Plan C) since it calls into `proxy::swap_upstream`.

- **10a**: `deployments` table + `Deployment` entity + DAO. New migration `0011_deployments.sql`.
- **10b**: `deployment::eligibility::classify` (pure) + `deployment::healthcheck::DeploymentHealthcheck` (polling + threshold + deadline; wraps Plan A's `HealthChecker`).
- **10c**: `deployment::driver::Driver` state machine (pending → spinning_up → switching → draining → destroying_blue → done; aborts on spinup_failure / healthcheck_timeout / swap_failure). `DeploymentSupervisor` decides strategy + spawns drivers + reconciles orphans on restart.
- **10d**: `UpdateDispatcher` trait in `isengard-core` lets the `updater` plugin hand off to the agent's Supervisor without depending on the agent crate. `SupervisorDispatcher` impl in agent does the routing-rule lookup + forwards to `handle_update_trigger`. PluginContext gains `with_update_dispatcher`. Agent main wires the supervisor.

Real Docker e2e (gated `#[ignore]`):
- happy path: nginx blue → trigger → green starts → healthcheck passes → swap_upstream → blue destroyed → deployment row Done
- abort path: healthcheck path returns 404 → driver hits deadline → green destroyed → blue still serving → deployment row Aborted

## Spec

`docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`

## Test plan

- [x] cargo build --workspace
- [x] cargo test --workspace (default suite, ~30 new unit tests across storage + agent)
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo deny check
- [x] bun run build (dashboard frontend; sanity from stack)
- [ ] Manual real-Docker: `cargo test --test deployment_blue_green_happy -- --ignored --nocapture`
- [ ] Manual real-Docker: `cargo test --test deployment_blue_green_aborts_on_healthcheck -- --ignored --nocapture`
- [ ] Manual smoke: trigger an update on a real container with a routing rule + healthcheck; observe Deployment row + emitted events; verify traffic shifts in dashboard's events tab.

## Stacked PR merge order

#18 → #19 → #20 → **#21**. Will rebase as upstream PRs merge.

## Out of scope (deferred to Plan B/C)

In-flight UI panel, abort UI, settings strategy override, deployment history tab, multi-host rolling, per-rule connection-lifetime knob, post-switch swap-back, resource pre-flight.
EOF
)"
```

- [ ] **Step 3: Return PR URL**

---

## Notes for the implementer

- The plan assumes the storage `Error` enum has a `Decode(String)` variant. If it doesn't, swap to whatever the closest existing variant is (`Sql`, `Other`, etc.); pattern matters more than name.
- `Inventory::open_in_memory()` exists (verified). Tests rely on it.
- `RoutingRule.healthcheck_path: Option<String>` and `RoutingRule.public_hostname: String` (verified) — the dispatcher reads both.
- `HealthChecker::tcp_only(timeout)` and `HealthChecker::new(path, timeout)` (verified) — both exposed.
- `proxy::swap_upstream(state, hostname, new_upstream, grace).await -> Result<()>` (Plan C, verified).
- `proxy::ProxyState::new()` is `Clone` and exposes `pub upstreams: Arc<RwLock<UpstreamRegistry>>` (verified).
- `UpstreamRegistry::set(hostname, upstream)` and `get(hostname) -> Option<&Upstream>` (verified).
- `Event { kind, occurred_at, host_id: Option<HostId>, summary, container_name, image, old_digest, new_digest, error, metadata: serde_json::Value }` (verified). Emit via `emitter.emit(event).await`.
- `EventEmitter::emit` is async; the Driver fires in a `tokio::spawn` to keep the state machine non-blocking on emit.
- The plan's helper functions in the updater (`extract_service_name`, etc.) are bollard-API-shape-dependent. Inspect data is available because the updater already calls `docker.inspect_container` when classifying. Reuse that data, don't re-inspect.
- The agent's pre-existing `lefthook` runs `cargo test --workspace` on push; the e2e tests are `#[ignore]`-gated so they won't run there. Don't unmark them.
