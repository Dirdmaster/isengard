//! Routing rule reconciler: turns the storage view of `routing_rules`
//! into per-host `ProxyConfig` messages.
//!
//! Each host has a monotonic `generation` that only increments when the
//! rule-set hash changes. Agents use the generation to discard stale
//! pushes that arrive out of order.
//!
//! # Sender registry
//!
//! The Sync RPC handler registers its outbound `mpsc::Sender` here
//! (keyed by [`HostId`]) on stream open and unregisters on close.
//! [`RoutingPusher::push_to_host`] builds the latest `ProxyConfig` and
//! shoves it down whichever sender is currently registered, best-effort:
//! a full queue drops the message rather than blocking the caller, and
//! the next push will carry the freshest config anyway.
//!
//! The same registry doubles as the channel for arbitrary
//! `ControllerMessage` payloads (`StartLogStream`, `StopLogStream`,
//! `AbortDeployment`) via [`RoutingPusher::send_to_host`] and
//! [`RoutingPusher::send_message_to_host`].
//!
//! # Label ingest
//!
//! [`RoutingPusher::ingest_labels`] translates `ContainerLabelsReport`
//! payloads into routing rules; [`RoutingPusher::ingest_labels_removed`]
//! drops the label-source rules when the container goes away.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use isengard_proto::pb::{
    ContainerLabelsRemoved, ContainerLabelsReport, ControllerMessage, Healthcheck, ProxyConfig,
    ProxySettings, RoutingRule as ProtoRule, TlsMode, Upstream, WildcardCert as ProtoWildcardCert,
    controller_message::Payload,
};
use isengard_storage::{
    HostId, InsertRoutingRule, Inventory, RoutingRule as StorageRule, RoutingRuleSource,
    RoutingRuleState,
};
use tokio::sync::{Mutex, mpsc};
use tonic::Status;

use crate::acme::WildcardCertStore;
use crate::config::{ConfigDispatcher, ConfigValue};

/// Per-host generation tracking. Only increments when the input hash
/// actually changes.
#[derive(Default)]
struct PerHostState {
    /// Last generation pushed to (or built for) the host.
    generation: u64,
    /// Hash of the rule-set used to compute the last generation. When a
    /// new build hashes to the same value, generation does not
    /// increment.
    last_hash: u64,
}

/// Outbound message channel for the Sync gRPC stream.
///
/// The registry stores this shape directly (rather than a bare
/// `Sender<ControllerMessage>`) so the Sync handler hands its actual
/// outbound sender in without an adapter task.
pub type OutboundSender = mpsc::Sender<Result<ControllerMessage, Status>>;

/// Sender registry keyed by host id.
#[derive(Default)]
struct Senders {
    /// One outbound sender per currently-connected host.
    by_host: HashMap<HostId, OutboundSender>,
}

/// Reconciler that turns storage `routing_rules` into proto
/// `ProxyConfig` messages, one host at a time, with monotonic per-host
/// generation.
pub struct RoutingPusher {
    /// Shared inventory for routing-rule reads.
    inv: Arc<Inventory>,
    /// Per-host generation tracking.
    by_host: Mutex<HashMap<HostId, PerHostState>>,
    /// Per-host outbound sender registry.
    senders: Mutex<Senders>,
    /// Wildcard certs the controller has issued. Snapshotted on every
    /// build so each push includes the current cert material. Optional
    /// so non-ACME deployments pay no cost.
    wildcard_certs: Option<Arc<WildcardCertStore>>,
    /// Schema-driven config dispatcher. Used to look up `acme.contact_email`
    /// (and other future routing-relevant keys) on every `build_for_host`
    /// so operator-set values flow into `ProxySettings` without a restart.
    /// Optional so test harnesses that build a pusher without a full
    /// controller boot keep working; `None` falls back to the previous
    /// hard-coded empty contact email.
    config: Option<Arc<ConfigDispatcher>>,
}

impl RoutingPusher {
    /// Builds a pusher without wildcard cert support.
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_host: Mutex::new(HashMap::new()),
            senders: Mutex::new(Senders::default()),
            wildcard_certs: None,
            config: None,
        }
    }

    /// Attaches the wildcard cert store.
    ///
    /// [`crate::run_controller`] calls this when the ACME subsystem is
    /// enabled.
    pub fn with_wildcard_certs(mut self, store: Arc<WildcardCertStore>) -> Self {
        self.wildcard_certs = Some(store);
        self
    }

    /// Attaches the `isd configure` dispatcher so per-host `ProxyConfig`
    /// builds can pull `acme.contact_email` (and other future routing
    /// keys) without a restart.
    ///
    /// [`crate::run_controller`] calls this after the dispatcher has
    /// booted alongside the secrets and settings stores.
    pub fn with_config_dispatcher(mut self, dispatcher: Arc<ConfigDispatcher>) -> Self {
        self.config = Some(dispatcher);
        self
    }

    /// Builds the latest `ProxyConfig` for `host_id`.
    ///
    /// Generation increments only when the rule-set or the
    /// wildcard-cert set has actually changed since the previous build.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage read failure.
    pub async fn build_for_host(&self, host_id: HostId) -> Result<ProxyConfig> {
        let rules = self.inv.list_routing_rules_for_host(host_id).await?;
        let wildcards = match &self.wildcard_certs {
            Some(store) => store.snapshot().await,
            None => Vec::new(),
        };

        let hash = hash_rules_and_wildcards(&self.inv, &rules, &wildcards).await?;
        let generation = {
            let mut guard = self.by_host.lock().await;
            let entry = guard.entry(host_id).or_default();
            if entry.last_hash != hash {
                entry.generation += 1;
                entry.last_hash = hash;
            }
            entry.generation
        };

        let proto_wildcards: Vec<ProtoWildcardCert> = wildcards
            .iter()
            .map(|w| ProtoWildcardCert {
                identifiers: w.identifiers.clone(),
                cert_pem: w.cert_pem.clone(),
                key_pem: w.key_pem.clone(),
            })
            .collect();

        // `acme.contact_email` is operator-settable via `isd configure`.
        // Pull it through the dispatcher so a value written by the
        // operator reaches every connected agent without a restart. The
        // previous behaviour was `String::new()`, which silently disabled
        // some ACME flows; preserve that fallback when the dispatcher is
        // absent (tests) or the key is unset.
        let contact_email = self.resolve_acme_contact_email().await;

        Ok(ProxyConfig {
            host_id: host_id.to_string(),
            generation,
            rules: rules.into_iter().map(to_proto).collect(),
            settings: Some(ProxySettings {
                http_port: 8080,
                https_port: 8443,
                acme_contact_email: contact_email,
                log_sample_rate: 0.0,
            }),
            wildcard_certs: proto_wildcards,
        })
    }

    /// Resolves the current `acme.contact_email` value from the
    /// dispatcher.
    ///
    /// Returns `Set` / `Default` string values as-is; any other shape,
    /// missing dispatcher, or backing-store failure falls back to the
    /// historical empty string so a misconfigured dispatcher does not
    /// stall the push pipeline. Non-string values and lookup failures
    /// are logged at `warn` to surface during operator triage.
    async fn resolve_acme_contact_email(&self) -> String {
        let Some(dispatcher) = self.config.as_ref() else {
            return String::new();
        };
        match dispatcher.get("acme.contact_email").await {
            Ok(Some(ConfigValue::Set(v))) | Ok(Some(ConfigValue::Default(v))) => match v {
                serde_json::Value::String(s) => s,
                other => {
                    tracing::warn!(
                        value = %other,
                        "acme.contact_email is not a string; using empty fallback"
                    );
                    String::new()
                }
            },
            Ok(None) => String::new(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "acme.contact_email lookup failed; using empty fallback"
                );
                String::new()
            }
        }
    }

    /// Registers the Sync outbound sender for `host`.
    ///
    /// Last-writer-wins: a reconnecting agent's new sender supersedes
    /// the previous one (which is already torn down on the agent side).
    pub async fn register_sender(&self, host: HostId, tx: OutboundSender) {
        let mut s = self.senders.lock().await;
        s.by_host.insert(host, tx);
    }

    /// Drops the registered sender for `host`.
    ///
    /// Called by the Sync handler on stream close.
    pub async fn unregister_sender(&self, host: HostId) {
        let mut s = self.senders.lock().await;
        s.by_host.remove(&host);
    }

    /// Sends an arbitrary `ControllerMessage` (e.g. `StartLogStream`,
    /// `StopLogStream`) to `host`'s current Sync sender.
    ///
    /// Returns `true` on a successful enqueue, `false` when `host` has
    /// no registered sender or the sender's queue is full or closed.
    /// Best-effort: full queues drop the message and the caller decides
    /// whether to retry.
    pub async fn send_to_host(&self, host: HostId, msg: ControllerMessage) -> bool {
        let senders = self.senders.lock().await;
        match senders.by_host.get(&host) {
            Some(tx) => tx.try_send(Ok(msg)).is_ok(),
            None => false,
        }
    }

    /// Pushes the latest `ProxyConfig` to `host`'s currently registered
    /// sender, if any.
    ///
    /// Best-effort: a full queue drops the message rather than
    /// blocking. The next `push_to_host` carries the freshest config
    /// anyway.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage read failure inside `build_for_host`.
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

    /// Pushes the latest `ProxyConfig` to every host with a currently
    /// registered Sync sender.
    ///
    /// The ACME wildcard pusher calls this on issuance or renewal so
    /// every connected agent gets the new cert material without waiting
    /// for its next reconnect.
    ///
    /// # Errors
    ///
    /// Returns `Ok(())` even when per-host pushes fail. Per-host errors
    /// are logged at `warn`.
    pub async fn push_to_all_hosts(&self) -> Result<()> {
        let hosts: Vec<HostId> = {
            let senders = self.senders.lock().await;
            senders.by_host.keys().copied().collect()
        };
        for host in hosts {
            if let Err(e) = self.push_to_host(host).await {
                tracing::warn!(host = %host, error = %e, "push_to_all_hosts: per-host push failed");
            }
        }
        Ok(())
    }

    /// Sends an arbitrary `ControllerMessage` to a specific host.
    ///
    /// The dashboard's abort endpoint uses this to deliver
    /// `AbortDeployment` immediately rather than piggybacking on the
    /// next proxy_config push.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `host_id` is not currently connected or when
    /// the outbound channel rejects the send.
    pub async fn send_message_to_host(
        &self,
        host_id: HostId,
        message: ControllerMessage,
    ) -> Result<()> {
        let senders = self.senders.lock().await;
        let sender = senders
            .by_host
            .get(&host_id)
            .ok_or_else(|| anyhow::anyhow!("host {host_id:?} not connected"))?;
        sender
            .try_send(Ok(message))
            .map_err(|e| anyhow::anyhow!("send_to_host {host_id:?} failed: {e}"))?;
        Ok(())
    }

    /// Translates a `ContainerLabelsReport` into routing rules.
    ///
    /// Any existing label-source rules for the same container are
    /// deleted first, so the report becomes the single source of
    /// truth: adds, edits, and removals of individual labels all
    /// converge to the same end state. UI rules that share a hostname
    /// with a label rule lose: their overrides snapshot forward onto
    /// the new label rule and the UI row is deleted.
    ///
    /// Resolving the `stack_id` from the container's compose project
    /// label is a TODO for Plan B.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failures during rule listing or
    /// override copy. Per-rule UNIQUE collisions are logged at `warn`
    /// and skipped (first-writer-wins on hostname).
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
            let Some(port) = route_intent_port(&report, &rule) else {
                tracing::warn!(
                    host_id = %host_id,
                    container_id = %report.container_id,
                    hostname = %rule.hostname,
                    "ingest_labels: skipping expose label with unresolved port"
                );
                continue;
            };
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

            let hostname_for_log = rule.hostname.clone();
            let insert_result = self
                .inv
                .insert_routing_rule(InsertRoutingRule {
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
                .await;
            let new_rule = match insert_result {
                Ok(r) => r,
                Err(e) => {
                    // Most likely cause: another label rule already owns this
                    // hostname on this host (UNIQUE(public_hostname, host_id)).
                    // First-writer-wins; we log and skip rather than tearing
                    // down the existing rule. Use `ingest_labels_removed` from
                    // the other container's exit, or have the user resolve
                    // the duplicate label, to free the hostname.
                    tracing::warn!(
                        host_id = %host_id,
                        container_id = %report.container_id,
                        hostname = %hostname_for_log,
                        error = %e,
                        "ingest_labels: skipping label rule (likely hostname collision \
                         with another label rule on this host)"
                    );
                    continue;
                }
            };

            for (field, value) in preserved_overrides {
                self.inv
                    .upsert_routing_rule_override(new_rule.id, &field, value)
                    .await?;
            }
        }
        Ok(())
    }

    /// Drops every label-source rule whose `source_container_id`
    /// matches the removed container.
    ///
    /// No-op when the container never carried `isengard.expose*`
    /// labels.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure.
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

/// Resolves a label rule to an explicit or agent-inferred upstream port.
fn route_intent_port(
    report: &ContainerLabelsReport,
    rule: &isengard_core::labels::LabelRule,
) -> Option<u16> {
    if let Some(port) = rule.port {
        return Some(port);
    }
    let expected_name = rule.name.as_deref().unwrap_or("");
    report
        .label_route_intents
        .iter()
        .find(|intent| intent.name == expected_name && intent.hostname == rule.hostname)
        .and_then(|intent| u16::try_from(intent.container_port).ok())
        .filter(|port| *port != 0)
}

/// Converts a storage `RoutingRule` into its proto wire form.
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

/// Computes a generation-bumping hash over the rule set, every rule's
/// overrides, and the current wildcard cert set.
///
/// Override changes bump the hash because the agent needs to see the
/// new override-driven config; cert PEM changes bump the hash because a
/// renewed wildcard must reach the agent without waiting for an
/// unrelated rule edit.
async fn hash_rules_and_wildcards(
    inv: &Inventory,
    rules: &[StorageRule],
    wildcards: &[Arc<crate::acme::WildcardCert>],
) -> Result<u64> {
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
        // Include per-rule overrides so a UI override change bumps the
        // generation and the agent receives the new ProxyConfig.
        // Without this, hash-only-on-rule-fields would silently stall on
        // override-only edits.
        let overrides = inv.list_routing_rule_overrides(r.id).await?;
        for o in &overrides {
            o.field.hash(&mut h);
            o.value_json.to_string().hash(&mut h);
        }
    }
    // Hash wildcard certs too: a freshly-issued or renewed cert must bump
    // the generation so agents see it. Using `cert_pem` is enough because
    // issuance / renewal always produces new cert bytes.
    for w in wildcards {
        for id in &w.identifiers {
            id.hash(&mut h);
        }
        w.cert_pem.hash(&mut h);
    }
    Ok(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_proto::pb::LabelRouteIntent;
    use isengard_storage::{EnrollHost, Inventory};
    use std::collections::HashMap;

    async fn setup() -> (Arc<Inventory>, HostId, RoutingPusher) {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "lausanne".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
            })
            .await
            .unwrap();
        let routing = RoutingPusher::new(inv.clone());
        (inv, host, routing)
    }

    fn report(pairs: &[(&str, &str)], intents: Vec<LabelRouteIntent>) -> ContainerLabelsReport {
        let labels: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ContainerLabelsReport {
            container_id: "cid-plex".into(),
            container_name: "plex".into(),
            image: "plex".into(),
            labels,
            label_route_intents: intents,
        }
    }

    #[tokio::test]
    async fn hostname_only_without_resolved_intent_does_not_default_to_80() {
        let (inv, host, routing) = setup().await;
        let r = report(&[("isengard.expose", "plex.vallee.casa")], Vec::new());

        routing.ingest_labels(host, r).await.unwrap();

        assert!(
            inv.list_routing_rules_for_host(host)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn hostname_only_with_agent_intent_inserts_inferred_port() {
        let (inv, host, routing) = setup().await;
        let r = report(
            &[("isengard.expose", "plex.vallee.casa")],
            vec![LabelRouteIntent {
                name: String::new(),
                hostname: "plex.vallee.casa".into(),
                container_port: 32400,
                unresolved_reason: String::new(),
            }],
        );

        routing.ingest_labels(host, r).await.unwrap();

        let rules = inv.list_routing_rules_for_host(host).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].public_hostname, "plex.vallee.casa");
        assert_eq!(rules[0].container_port, 32400);
    }

    #[tokio::test]
    async fn explicit_port_label_still_inserts_without_agent_intent() {
        let (inv, host, routing) = setup().await;
        let r = report(
            &[
                ("isengard.expose", "plex.vallee.casa"),
                ("isengard.expose.port", "32400"),
            ],
            Vec::new(),
        );

        routing.ingest_labels(host, r).await.unwrap();

        let rules = inv.list_routing_rules_for_host(host).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].container_port, 32400);
    }
}
