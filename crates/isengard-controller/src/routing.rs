//! Routing rule reconciler: turns the storage view of `routing_rules` into
//! `ProxyConfig` messages addressed at individual hosts.
//!
//! Per-host monotonic `generation`: only increments when the rule-set hash
//! actually changes. Agents use the generation to discard stale pushes that
//! arrive out of order.
//!
//! Tasks 14 / 16 / 17 will extend this with `register_sender`,
//! `unregister_sender`, `push_to_host`, and `ingest_labels`. Today we only
//! expose `build_for_host`, which is the foundation those layers reuse.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use isengard_proto::pb::{
    Healthcheck, ProxyConfig, ProxySettings, RoutingRule as ProtoRule, TlsMode, Upstream,
};
use isengard_storage::{HostId, Inventory, RoutingRule as StorageRule};
use tokio::sync::Mutex;

#[derive(Default)]
struct PerHostState {
    /// Last generation pushed to (or built for) the host.
    generation: u64,
    /// Hash of the rule-set used to compute the last generation. If a new
    /// build hashes to the same value, generation does NOT increment.
    last_hash: u64,
}

/// Reconciler that turns storage `routing_rules` into proto `ProxyConfig`
/// messages, one host at a time, with monotonic per-host generation.
pub struct RoutingPusher {
    inv: Arc<Inventory>,
    by_host: Mutex<HashMap<HostId, PerHostState>>,
}

impl RoutingPusher {
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_host: Mutex::new(HashMap::new()),
        }
    }

    /// Build the latest `ProxyConfig` for a host. Generation increments only
    /// when the rule-set has actually changed since the previous build.
    pub async fn build_for_host(&self, host_id: HostId) -> Result<ProxyConfig> {
        let rules = self.inv.list_routing_rules_for_host(host_id).await?;

        let hash = hash_rules(&rules);
        let mut guard = self.by_host.lock().await;
        let entry = guard.entry(host_id).or_default();
        if entry.last_hash != hash {
            entry.generation += 1;
            entry.last_hash = hash;
        }

        Ok(ProxyConfig {
            host_id: host_id.to_string(),
            generation: entry.generation,
            rules: rules.into_iter().map(to_proto).collect(),
            settings: Some(ProxySettings {
                http_port: 8080,
                https_port: 8443,
                acme_contact_email: String::new(),
                log_sample_rate: 0.0,
            }),
        })
    }
}

fn to_proto(r: StorageRule) -> ProtoRule {
    ProtoRule {
        id: r.id.0,
        public_hostname: r.public_hostname,
        upstream: Some(Upstream {
            // The controller doesn't know the container's IP. Carry the
            // service identity as `container_id`; the agent fills in
            // `container_ip` from its local Docker view at apply-time.
            container_id: r.service_name.clone(),
            container_ip: String::new(),
            container_port: r.container_port as u32,
        }),
        tls_mode: match r.tls_mode {
            isengard_storage::TlsMode::Edge => TlsMode::Edge as i32,
            isengard_storage::TlsMode::Acme => TlsMode::Acme as i32,
            isengard_storage::TlsMode::Manual => TlsMode::Manual as i32,
        },
        healthcheck: Some(Healthcheck {
            path: r.healthcheck_path.unwrap_or_default(),
            interval_secs: r.healthcheck_interval_secs,
        }),
        adapter: r.adapter,
    }
}

fn hash_rules(rules: &[StorageRule]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for r in rules {
        r.id.0.hash(&mut h);
        r.public_hostname.hash(&mut h);
        r.container_port.hash(&mut h);
        r.adapter.hash(&mut h);
        r.healthcheck_path.hash(&mut h);
        r.healthcheck_interval_secs.hash(&mut h);
    }
    h.finish()
}
