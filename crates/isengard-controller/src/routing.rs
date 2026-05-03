//! Routing rule reconciler: turns the storage view of `routing_rules` into
//! `ProxyConfig` messages addressed at individual hosts.
//!
//! Per-host monotonic `generation`: only increments when the rule-set hash
//! actually changes. Agents use the generation to discard stale pushes that
//! arrive out of order.
//!
//! Task 14: per-host sender registry. The Sync RPC handler registers its
//! outbound `mpsc::Sender` here (keyed by `HostId`) on stream open and
//! unregisters on close. `push_to_host` builds the latest `ProxyConfig` and
//! shoves it down whichever sender is currently registered (best-effort —
//! drop on full queue rather than block the caller).
//!
//! Task 17 will extend this with `ingest_labels` to translate
//! `ContainerLabelsReport` payloads into routing rules.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use isengard_proto::pb::{
    ControllerMessage, Healthcheck, ProxyConfig, ProxySettings, RoutingRule as ProtoRule, TlsMode,
    Upstream, controller_message::Payload,
};
use isengard_storage::{HostId, Inventory, RoutingRule as StorageRule};
use tokio::sync::{Mutex, mpsc};
use tonic::Status;

#[derive(Default)]
struct PerHostState {
    /// Last generation pushed to (or built for) the host.
    generation: u64,
    /// Hash of the rule-set used to compute the last generation. If a new
    /// build hashes to the same value, generation does NOT increment.
    last_hash: u64,
}

/// Outbound message type as carried on the Sync gRPC stream. The registry
/// stores this directly (rather than a bare `Sender<ControllerMessage>`) so
/// the Sync handler can hand its actual sender in without an adapter task.
pub type OutboundSender = mpsc::Sender<Result<ControllerMessage, Status>>;

#[derive(Default)]
struct Senders {
    by_host: HashMap<HostId, OutboundSender>,
}

/// Reconciler that turns storage `routing_rules` into proto `ProxyConfig`
/// messages, one host at a time, with monotonic per-host generation.
pub struct RoutingPusher {
    inv: Arc<Inventory>,
    by_host: Mutex<HashMap<HostId, PerHostState>>,
    senders: Mutex<Senders>,
}

impl RoutingPusher {
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_host: Mutex::new(HashMap::new()),
            senders: Mutex::new(Senders::default()),
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

    /// Register the Sync outbound sender for a host. Replaces any previous
    /// sender (last-writer-wins; a reconnecting agent supersedes its old
    /// stream which is already torn down on the agent side).
    pub async fn register_sender(&self, host: HostId, tx: OutboundSender) {
        let mut s = self.senders.lock().await;
        s.by_host.insert(host, tx);
    }

    /// Drop the registered sender for a host (called on stream close).
    pub async fn unregister_sender(&self, host: HostId) {
        let mut s = self.senders.lock().await;
        s.by_host.remove(&host);
    }

    /// Push the latest `ProxyConfig` to the host's currently registered
    /// sender (if any). Best-effort: a full queue drops the message rather
    /// than blocking — the next `push_to_host` will carry the freshest
    /// config anyway.
    pub async fn push_to_host(&self, host: HostId) -> Result<()> {
        let cfg = self.build_for_host(host).await?;
        let senders = self.senders.lock().await;
        if let Some(tx) = senders.by_host.get(&host) {
            let msg = ControllerMessage {
                payload: Some(Payload::ProxyConfig(cfg)),
            };
            let _ = tx.try_send(Ok(msg));
        }
        Ok(())
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
