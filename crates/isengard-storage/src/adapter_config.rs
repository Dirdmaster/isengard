//! Per-host networking adapter configuration. See spec §6 — `adapter_config`
//! holds the JSON config + enabled flag for each adapter (e.g. `none`,
//! `caddy`, `traefik`) on a given host. Primary key is `(host_id, adapter)`.

use crate::error::{Error, Result};
use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterConfig {
    pub host_id: HostId,
    pub adapter: String,
    pub config_json: serde_json::Value,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UpsertAdapterConfig {
    pub host_id: HostId,
    pub adapter: String,
    pub config_json: serde_json::Value,
    pub enabled: bool,
}

impl crate::inventory::Inventory {
    pub async fn upsert_adapter_config(&self, ins: UpsertAdapterConfig) -> Result<()> {
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let cfg_str = ins.config_json.to_string();
        sqlx::query(
            r#"
            INSERT INTO adapter_config (host_id, adapter, config_json, enabled)
            VALUES (?, ?, ?, ?)
            ON CONFLICT (host_id, adapter) DO UPDATE SET
              config_json = excluded.config_json,
              enabled = excluded.enabled,
              updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(&host_bytes)
        .bind(&ins.adapter)
        .bind(&cfg_str)
        .bind(ins.enabled)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_adapter_config(
        &self,
        host_id: HostId,
        adapter: &str,
    ) -> Result<Option<AdapterConfig>> {
        use sqlx::Row;
        let host_bytes = host_id.to_bytes().to_vec();
        let row = sqlx::query(
            "SELECT host_id, adapter, config_json, enabled, created_at, updated_at \
             FROM adapter_config \
             WHERE host_id = ? AND adapter = ?",
        )
        .bind(&host_bytes)
        .bind(adapter)
        .fetch_optional(self.pool())
        .await?;

        let Some(r) = row else {
            return Ok(None);
        };

        let host_bytes: Vec<u8> = r.try_get("host_id")?;
        let host_id = HostId::from_db_bytes(host_bytes)?;
        let adapter: String = r.try_get("adapter")?;
        let config_str: String = r.try_get("config_json")?;
        let config_json = serde_json::from_str(&config_str).map_err(|e| Error::Decode {
            reason: format!("invalid config_json: {e}"),
        })?;
        let enabled: i64 = r.try_get("enabled")?;
        let enabled = enabled != 0;

        let parse_dt = |key: &str| -> Result<DateTime<Utc>> {
            let s: String = r.try_get(key)?;
            DateTime::parse_from_rfc3339(&s)
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                        .map(|n| n.and_utc().fixed_offset())
                })
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| Error::Decode {
                    reason: format!("bad timestamp '{s}' for {key}: {e}"),
                })
        };
        let created_at = parse_dt("created_at")?;
        let updated_at = parse_dt("updated_at")?;

        Ok(Some(AdapterConfig {
            host_id,
            adapter,
            config_json,
            enabled,
            created_at,
            updated_at,
        }))
    }
}
