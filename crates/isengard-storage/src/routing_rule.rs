//! Routing rule row + insert request + CRUD helpers on `Inventory`.
//!
//! See spec §6 — `routing_rules` table is the source of truth for which
//! `(public_hostname → host:container:port)` mappings the proxy should serve.

use crate::error::{Error, Result};
use crate::host::HostId;
use crate::stack::StackId;
use serde::{Deserialize, Serialize};

/// Surrogate primary key for routing rules. The `routing_rules` table uses
/// an autoincrementing integer because there is no natural identifier — the
/// `(public_hostname, host_id)` pair is unique but heavy to use as a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoutingRuleId(pub i64);

impl std::fmt::Display for RoutingRuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// TLS terminates at an upstream edge (e.g. Cloudflare); proxy serves HTTP.
    Edge,
    /// Proxy obtains and renews a Let's Encrypt cert for the hostname.
    Acme,
    /// User-supplied cert in `tls_certs`.
    Manual,
}

impl TlsMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TlsMode::Edge => "edge",
            TlsMode::Acme => "acme",
            TlsMode::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingRuleState {
    /// Rule has been written but the proxy has not yet activated it.
    Pending,
    /// Proxy is serving traffic for this rule.
    Active,
    /// Rule is being torn down; proxy stops accepting new connections.
    Draining,
    /// Activation failed — see logs/journal for details.
    Failed,
}

impl RoutingRuleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingRuleState::Pending => "pending",
            RoutingRuleState::Active => "active",
            RoutingRuleState::Draining => "draining",
            RoutingRuleState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingRuleSource {
    /// Created by a user through the dashboard.
    Ui,
    /// Discovered from a `isengard.expose=*` container label.
    Label,
    /// Imported from an external source (NPM dump, Caddyfile, etc.).
    Imported,
}

impl RoutingRuleSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingRuleSource::Ui => "ui",
            RoutingRuleSource::Label => "label",
            RoutingRuleSource::Imported => "imported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: RoutingRuleId,
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub service_name: String,
    pub container_port: u16,
    pub public_hostname: String,
    pub protocol: String,
    pub adapter: String,
    pub tls_mode: TlsMode,
    pub healthcheck_path: Option<String>,
    pub healthcheck_interval_secs: u32,
    pub auth: Option<String>,
    pub state: RoutingRuleState,
    pub source: RoutingRuleSource,
    pub source_container_id: Option<String>,
    pub source_imported_from: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InsertRoutingRule {
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub service_name: String,
    pub container_port: u16,
    pub public_hostname: String,
    pub protocol: String,
    pub adapter: String,
    pub tls_mode: TlsMode,
    pub healthcheck_path: Option<String>,
    pub healthcheck_interval_secs: u32,
    pub auth: Option<String>,
    pub state: RoutingRuleState,
    pub source: RoutingRuleSource,
    pub source_container_id: Option<String>,
    pub source_imported_from: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn insert_routing_rule(&self, ins: InsertRoutingRule) -> Result<RoutingRule> {
        use sqlx::Row;
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let stack_id = ins.stack_id.map(|s| s.0);
        let row = sqlx::query(
            r#"
            INSERT INTO routing_rules (
              host_id, stack_id, service_name, container_port,
              public_hostname, protocol, adapter, tls_mode, healthcheck_path,
              healthcheck_interval_secs, auth, state, source,
              source_container_id, source_imported_from
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(&host_bytes)
        .bind(stack_id)
        .bind(&ins.service_name)
        .bind(ins.container_port as i64)
        .bind(&ins.public_hostname)
        .bind(&ins.protocol)
        .bind(&ins.adapter)
        .bind(ins.tls_mode.as_str())
        .bind(&ins.healthcheck_path)
        .bind(ins.healthcheck_interval_secs as i64)
        .bind(&ins.auth)
        .bind(ins.state.as_str())
        .bind(ins.source.as_str())
        .bind(&ins.source_container_id)
        .bind(&ins.source_imported_from)
        .fetch_one(self.pool())
        .await?;

        let id: i64 = row.try_get("id")?;

        Ok(RoutingRule {
            id: RoutingRuleId(id),
            host_id: ins.host_id,
            stack_id: ins.stack_id,
            service_name: ins.service_name,
            container_port: ins.container_port,
            public_hostname: ins.public_hostname,
            protocol: ins.protocol,
            adapter: ins.adapter,
            tls_mode: ins.tls_mode,
            healthcheck_path: ins.healthcheck_path,
            healthcheck_interval_secs: ins.healthcheck_interval_secs,
            auth: ins.auth,
            state: ins.state,
            source: ins.source,
            source_container_id: ins.source_container_id,
            source_imported_from: ins.source_imported_from,
        })
    }

    pub async fn list_routing_rules_for_host(&self, host_id: HostId) -> Result<Vec<RoutingRule>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, container_port,
                   public_hostname, protocol, adapter, tls_mode, healthcheck_path,
                   healthcheck_interval_secs, auth, state, source,
                   source_container_id, source_imported_from
            FROM routing_rules
            WHERE host_id = ?
            ORDER BY id ASC
            "#,
        )
        .bind(&host_bytes)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(routing_rule_from_row).collect()
    }

    /// Listing across all hosts, ordered by host then id. Replaces the
    /// per-host fan-out the dashboard's `list_rules` endpoint did before:
    /// one query instead of N+1.
    pub async fn list_all_routing_rules(&self) -> Result<Vec<RoutingRule>> {
        let rows = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, container_port,
                   public_hostname, protocol, adapter, tls_mode, healthcheck_path,
                   healthcheck_interval_secs, auth, state, source,
                   source_container_id, source_imported_from
            FROM routing_rules
            ORDER BY host_id ASC, id ASC
            "#,
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(routing_rule_from_row).collect()
    }

    /// Look up a single rule by its primary key. Replaces the dashboard's
    /// "fan out by host then filter" find-pattern in update/delete handlers.
    pub async fn get_routing_rule(&self, id: RoutingRuleId) -> Result<Option<RoutingRule>> {
        let row = sqlx::query(
            r#"
            SELECT id, host_id, stack_id, service_name, container_port,
                   public_hostname, protocol, adapter, tls_mode, healthcheck_path,
                   healthcheck_interval_secs, auth, state, source,
                   source_container_id, source_imported_from
            FROM routing_rules
            WHERE id = ?
            "#,
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
        row.map(routing_rule_from_row).transpose()
    }

    pub async fn delete_routing_rule(&self, id: RoutingRuleId) -> Result<()> {
        sqlx::query("DELETE FROM routing_rules WHERE id = ?")
            .bind(id.0)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn update_routing_rule_state(
        &self,
        id: RoutingRuleId,
        state: RoutingRuleState,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE routing_rules SET state = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(id.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

fn routing_rule_from_row(row: sqlx::sqlite::SqliteRow) -> Result<RoutingRule> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    if host_bytes.len() != 16 {
        return Err(Error::InvalidHostId(host_bytes.len()));
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);

    let tls_mode_str: String = row.try_get("tls_mode")?;
    let state_str: String = row.try_get("state")?;
    let source_str: String = row.try_get("source")?;
    let container_port: i64 = row.try_get("container_port")?;
    let healthcheck_interval_secs: i64 = row.try_get("healthcheck_interval_secs")?;

    Ok(RoutingRule {
        id: RoutingRuleId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        stack_id: row.try_get::<Option<i64>, _>("stack_id")?.map(StackId),
        service_name: row.try_get("service_name")?,
        container_port: container_port as u16,
        public_hostname: row.try_get("public_hostname")?,
        protocol: row.try_get("protocol")?,
        adapter: row.try_get("adapter")?,
        tls_mode: parse_tls_mode(&tls_mode_str)?,
        healthcheck_path: row.try_get("healthcheck_path")?,
        healthcheck_interval_secs: healthcheck_interval_secs as u32,
        auth: row.try_get("auth")?,
        state: parse_state(&state_str)?,
        source: parse_source(&source_str)?,
        source_container_id: row.try_get("source_container_id")?,
        source_imported_from: row.try_get("source_imported_from")?,
    })
}

fn parse_tls_mode(s: &str) -> Result<TlsMode> {
    match s {
        "edge" => Ok(TlsMode::Edge),
        "acme" => Ok(TlsMode::Acme),
        "manual" => Ok(TlsMode::Manual),
        other => Err(Error::Decode {
            reason: format!("invalid tls_mode: {other}"),
        }),
    }
}

fn parse_state(s: &str) -> Result<RoutingRuleState> {
    match s {
        "pending" => Ok(RoutingRuleState::Pending),
        "active" => Ok(RoutingRuleState::Active),
        "draining" => Ok(RoutingRuleState::Draining),
        "failed" => Ok(RoutingRuleState::Failed),
        other => Err(Error::Decode {
            reason: format!("invalid state: {other}"),
        }),
    }
}

fn parse_source(s: &str) -> Result<RoutingRuleSource> {
    match s {
        "ui" => Ok(RoutingRuleSource::Ui),
        "label" => Ok(RoutingRuleSource::Label),
        "imported" => Ok(RoutingRuleSource::Imported),
        other => Err(Error::Decode {
            reason: format!("invalid source: {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_mode_round_trips() {
        for m in [TlsMode::Edge, TlsMode::Acme, TlsMode::Manual] {
            assert_eq!(parse_tls_mode(m.as_str()).unwrap(), m);
        }
    }

    #[test]
    fn state_round_trips() {
        for s in [
            RoutingRuleState::Pending,
            RoutingRuleState::Active,
            RoutingRuleState::Draining,
            RoutingRuleState::Failed,
        ] {
            assert_eq!(parse_state(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn source_round_trips() {
        for s in [
            RoutingRuleSource::Ui,
            RoutingRuleSource::Label,
            RoutingRuleSource::Imported,
        ] {
            assert_eq!(parse_source(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn parse_tls_mode_rejects_unknown() {
        let err = parse_tls_mode("bogus").unwrap_err();
        assert!(format!("{err}").contains("invalid tls_mode"));
    }
}
