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
    ContainerLabelsRemoved, ContainerLabelsReport, ControllerMessage, Healthcheck, ProxyConfig,
    ProxySettings, RoutingRule as ProtoRule, TlsMode, Upstream, controller_message::Payload,
};
use isengard_storage::{
    HostId, InsertRoutingRule, Inventory, RoutingRule as StorageRule, RoutingRuleSource,
    RoutingRuleState,
};
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

    /// Translate a `ContainerLabelsReport` into routing rules. Any existing
    /// label-source rules for the same container are deleted first, so the
    /// report becomes the single source of truth — adds, edits and removals
    /// of individual labels all converge to the same end state.
    ///
    /// `fleet` is hardcoded to "default" for v1; resolving the host's actual
    /// fleet (and the `stack_id` from the container's compose project label)
    /// is a TODO for Plan B.
    pub async fn ingest_labels(
        &self,
        host_id: HostId,
        report: ContainerLabelsReport,
    ) -> Result<()> {
        let existing = self.inv.list_routing_rules_for_host(host_id).await?;
        for r in existing {
            if r.source == RoutingRuleSource::Label
                && r.source_container_id.as_deref() == Some(report.container_id.as_str())
            {
                self.inv.delete_routing_rule(r.id).await?;
            }
        }

        let parsed = isengard_core::labels::parse_labels(&report.labels);
        for rule in parsed {
            let port = rule.port.unwrap_or(80);
            let tls_mode = match rule.tls.as_deref() {
                Some("edge") => isengard_storage::TlsMode::Edge,
                Some("manual") => isengard_storage::TlsMode::Manual,
                _ => isengard_storage::TlsMode::Acme,
            };

            // Conflict resolution: labels win the hostname.
            // If a UI rule exists with this hostname on this host, snapshot its
            // overrides and delete the UI rule before inserting the label rule.
            let existing = self.inv.list_routing_rules_for_host(host_id).await?;
            let mut preserved_overrides: Vec<(String, serde_json::Value)> = vec![];
            for ex in existing {
                if ex.public_hostname == rule.hostname && ex.source == RoutingRuleSource::Ui {
                    for o in self.inv.list_routing_rule_overrides(ex.id).await? {
                        preserved_overrides.push((o.field, o.value_json));
                    }
                    self.inv.delete_routing_rule(ex.id).await?;
                }
            }

            let new_rule = self
                .inv
                .insert_routing_rule(InsertRoutingRule {
                    fleet: "default".into(),
                    host_id,
                    stack_id: None,
                    service_name: report.container_name.clone(),
                    container_port: port,
                    public_hostname: rule.hostname,
                    protocol: "http".into(),
                    adapter: rule.adapter.unwrap_or_else(|| "none".into()),
                    tls_mode,
                    healthcheck_path: rule.health,
                    healthcheck_interval_secs: 10,
                    auth: rule.auth,
                    state: RoutingRuleState::Pending,
                    source: RoutingRuleSource::Label,
                    source_container_id: Some(report.container_id.clone()),
                    source_imported_from: None,
                })
                .await?;

            for (field, value) in preserved_overrides {
                self.inv
                    .upsert_routing_rule_override(new_rule.id, &field, value)
                    .await?;
            }
        }
        Ok(())
    }

    /// Drop every label-source rule whose `source_container_id` matches the
    /// removed container. No-op if there are no such rules (e.g. the
    /// container never carried `isengard.expose*` labels).
    pub async fn ingest_labels_removed(
        &self,
        host_id: HostId,
        ev: ContainerLabelsRemoved,
    ) -> Result<()> {
        let existing = self.inv.list_routing_rules_for_host(host_id).await?;
        for r in existing {
            if r.source == RoutingRuleSource::Label
                && r.source_container_id.as_deref() == Some(ev.container_id.as_str())
            {
                self.inv.delete_routing_rule(r.id).await?;
            }
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
