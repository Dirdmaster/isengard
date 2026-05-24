//! Runtime event subscription. On `Start`, snapshots a container's labels
//! and pushes a `ContainerLabelsReport` up the sync stream. On `Stop` /
//! `Die`, pushes a `ContainerLabelsRemoved`.
//!
//! The watcher also runs an initial scan at startup so a freshly-connected
//! agent reports the world it sees, not only deltas.
//!
//! A container is reported when it carries either:
//!
//! - any `isengard.expose*` label (proxy / routing rules), OR
//! - any `isengard.policy.*` label (: container-scope policy
//!   discovery).
//!
//! Containers without either family of labels are filtered out: the
//! controller has nothing to do with them.
//!
//! Rewritten to drive off [`crate::runtime::RuntimeBackend`]
//! instead of bollard directly. The dockerd code path is unchanged in
//! shape (BollardBackend wraps `Docker::events` + `inspect_container`); the
//! watcher feeds Pingora's label-derived routing through that.
//!
//! Behavioral deltas vs the pre-0.5 bollard-direct watcher (documented for
//! follow-up):
//! - `container.update` events are not currently mapped to a
//!   [`crate::runtime::RuntimeEventType`] variant. Update-driven label
//!   refresh is lost under both backends temporarily; revisit when a real
//!   workload needs it.
//! - `container.destroy` events are likewise not in the trait. `Die`
//!   already drives the removed-report path, so this is a notification
//!   redundancy loss only (the controller still learns the container is
//!   gone via Die).

use std::sync::Arc;

use futures_util::StreamExt;
use isengard_proto::pb::{
    AgentMessage, ContainerLabelsRemoved, ContainerLabelsReport, LabelRouteIntent, agent_message,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::runtime::{ContainerSnapshot, ListFilter, RuntimeBackend, RuntimeEventType};

/// Long-running watcher: initial scan, then drain the runtime event stream.
/// Returns Err only on a fatal backend error (the spawn site logs and exits).
pub async fn watch(
    backend: Arc<dyn RuntimeBackend>,
    out: mpsc::Sender<AgentMessage>,
) -> anyhow::Result<()> {
    initial_scan(backend.as_ref(), &out).await?;

    let mut events = backend.stream_events();
    while let Some(ev) = events.next().await {
        match ev.event_type {
            RuntimeEventType::Start => match inspect_to_report(backend.as_ref(), &ev.container_id)
                .await
            {
                Ok(Some(report)) => {
                    let _ = out
                        .send(AgentMessage {
                            payload: Some(agent_message::Payload::ContainerLabelsReport(report)),
                        })
                        .await;
                }
                Ok(None) => {
                    debug!(
                        cid = %ev.container_id,
                        "labels: container has no isengard.expose labels, skipping",
                    );
                }
                Err(e) => {
                    warn!(error = %e, cid = %ev.container_id, "labels: inspect failed");
                }
            },
            RuntimeEventType::Stop | RuntimeEventType::Die { .. } => {
                let _ = out
                    .send(AgentMessage {
                        payload: Some(agent_message::Payload::ContainerLabelsRemoved(
                            ContainerLabelsRemoved {
                                container_id: ev.container_id.clone(),
                            },
                        )),
                    })
                    .await;
            }
            // HealthcheckPassed / HealthcheckFailed don't change reported
            // labels; the proxy healthcheck loop already evicts unhealthy
            // upstreams via its own probe.
            _ => {}
        }
    }
    Ok(())
}

/// Internal helper: initial scan.
async fn initial_scan(
    backend: &dyn RuntimeBackend,
    out: &mpsc::Sender<AgentMessage>,
) -> anyhow::Result<()> {
    let containers = backend
        .list_containers(ListFilter::default())
        .await
        .map_err(|e| anyhow::anyhow!("labels: initial list_containers failed: {e}"))?;
    for snap in containers {
        // list_containers populates labels from the summary, so we can
        // pre-filter without a full inspect for the common case (the
        // overwhelming majority of containers carry no isengard labels).
        if !snap.labels.keys().any(|k| is_reportable_label_key(k)) {
            continue;
        }
        // Re-inspect to pick up any inspect-only fields the summary might
        // miss (and to keep the build path identical to the event-driven
        // one). Fall back to the summary if inspect 404s mid-scan.
        let snap = match backend.inspect_container(&snap.id).await {
            Ok(Some(s)) => s,
            Ok(None) => snap,
            Err(e) => {
                warn!(error = %e, cid = %snap.id, "labels: initial inspect failed");
                continue;
            }
        };
        if let Some(report) = snapshot_to_report(&snap) {
            let _ = out
                .send(AgentMessage {
                    payload: Some(agent_message::Payload::ContainerLabelsReport(report)),
                })
                .await;
        }
    }
    Ok(())
}

/// `true` if the controller wants to know about a container that carries
/// this label key. Today: any `isengard.expose*` (routing) or
/// `isengard.policy.*` key.
///
/// Lives here (not in `isengard-core`) because it's a coupling between the
/// agent's report filter and the two consumer subsystems on the controller.
/// Splitting it would force the agent to depend on every consumer crate.
pub(crate) fn is_reportable_label_key(key: &str) -> bool {
    key.starts_with("isengard.expose") || key.starts_with("isengard.policy.")
}

/// Internal helper: inspect to report.
async fn inspect_to_report(
    backend: &dyn RuntimeBackend,
    cid: &str,
) -> anyhow::Result<Option<ContainerLabelsReport>> {
    let snap = match backend
        .inspect_container(cid)
        .await
        .map_err(|e| anyhow::anyhow!("inspect_container {cid}: {e}"))?
    {
        Some(s) => s,
        None => return Ok(None),
    };
    Ok(snapshot_to_report(&snap))
}

/// Build a `ContainerLabelsReport` from a [`ContainerSnapshot`], or `None`
/// if the snapshot's labels carry no reportable key. Mirrors the pre-0.5
/// `inspect_to_report` body; the only shape difference is that fields are
/// already mapped (we don't walk
/// `bollard::secret::ContainerInspectResponse` ourselves).
fn snapshot_to_report(snap: &ContainerSnapshot) -> Option<ContainerLabelsReport> {
    if !snap.labels.keys().any(|k| is_reportable_label_key(k)) {
        return None;
    }
    // ContainerLabelsReport.labels is a HashMap<String, String> on the
    // wire; ContainerSnapshot stores BTreeMap for determinism. Translate.
    let labels: std::collections::HashMap<String, String> = snap
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Some(ContainerLabelsReport {
        container_id: snap.id.clone(),
        container_name: snap.name.clone(),
        image: snap.image.clone(),
        labels,
        label_route_intents: label_route_intents(snap),
    })
}

fn label_route_intents(snap: &ContainerSnapshot) -> Vec<LabelRouteIntent> {
    let labels: std::collections::HashMap<String, String> = snap
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let candidates = infer_container_ports(snap);

    isengard_core::labels::parse_labels(&labels)
        .into_iter()
        .map(|rule| {
            let (container_port, unresolved_reason) = match rule.port {
                Some(port) => (u32::from(port), String::new()),
                None => match candidates.as_slice() {
                    [only] => (u32::from(*only), String::new()),
                    [] => (0, "no candidate ports from inspect".to_string()),
                    many => (
                        0,
                        format!(
                            "multiple candidate ports: {}",
                            many.iter()
                                .map(u16::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ),
                },
            };
            LabelRouteIntent {
                name: rule.name.unwrap_or_default(),
                hostname: rule.hostname,
                container_port,
                unresolved_reason,
            }
        })
        .collect()
}

fn infer_container_ports(snap: &ContainerSnapshot) -> Vec<u16> {
    let mut ports = Vec::new();
    for key in snap.network_settings.ports.keys() {
        push_port_key(&mut ports, key);
    }
    for binding in &snap.port_bindings {
        let container_side = binding.rsplit(':').next().unwrap_or(binding.as_str());
        push_port_key(&mut ports, container_side);
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn push_port_key(out: &mut Vec<u16>, key: &str) {
    let Some(port_str) = key.split('/').next() else {
        return;
    };
    if let Ok(port) = port_str.parse::<u16>() {
        out.push(port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ContainerState, HealthState, HealthcheckSpec, LogChunk, LogOptions, NetworkSettings,
        RuntimeError, RuntimeEvent,
    };
    use async_trait::async_trait;
    use futures_util::Stream;
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::SystemTime;

    #[test]
    fn expose_label_is_reportable() {
        assert!(is_reportable_label_key("isengard.expose"));
        assert!(is_reportable_label_key("isengard.expose.port"));
        assert!(is_reportable_label_key("isengard.expose.api.port"));
    }

    #[test]
    fn policy_label_is_reportable() {
        assert!(is_reportable_label_key("isengard.policy.strategy"));
        assert!(is_reportable_label_key("isengard.policy.gate"));
        assert!(is_reportable_label_key("isengard.policy.paused_until"));
    }

    #[test]
    fn enable_label_alone_is_not_enough() {
        // `isengard.enable=true` is a separate opt-in; a container with only
        // `isengard.enable` and nothing else does not need a labels report.
        assert!(!is_reportable_label_key("isengard.enable"));
    }

    #[test]
    fn unrelated_labels_are_not_reportable() {
        assert!(!is_reportable_label_key("com.docker.compose.project"));
        assert!(!is_reportable_label_key("traefik.http.routers.api.rule"));
        assert!(!is_reportable_label_key("isengard.stack"));
    }

    /// Mock RuntimeBackend that emits a scripted event stream and serves
    /// inspect requests from a static map. Just enough to exercise
    /// [`watch`] end to end without a real daemon.
    #[derive(Debug)]
    struct ScriptedBackend {
        events: Mutex<Option<Vec<RuntimeEvent>>>,
        snapshots: std::collections::HashMap<String, ContainerSnapshot>,
        list: Vec<ContainerSnapshot>,
    }

    impl ScriptedBackend {
        fn new() -> Self {
            Self {
                events: Mutex::new(Some(Vec::new())),
                snapshots: Default::default(),
                list: Vec::new(),
            }
        }

        fn with_event(mut self, ev: RuntimeEvent) -> Self {
            self.events.get_mut().unwrap().as_mut().unwrap().push(ev);
            self
        }

        fn with_snapshot(mut self, snap: ContainerSnapshot) -> Self {
            self.snapshots.insert(snap.id.clone(), snap);
            self
        }
    }

    #[async_trait]
    impl RuntimeBackend for ScriptedBackend {
        async fn ensure_image(&self, _r: &str) -> Result<String, RuntimeError> {
            Ok(String::new())
        }
        async fn create_container(
            &self,
            _s: &crate::runtime::ContainerCreateSpec,
        ) -> Result<String, RuntimeError> {
            Ok(String::new())
        }
        async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn stop_container(&self, _id: &str, _t: u32) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn remove_container(&self, _id: &str, _f: bool) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn list_containers(
            &self,
            _f: ListFilter,
        ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
            Ok(self.list.clone())
        }
        async fn inspect_container(
            &self,
            id: &str,
        ) -> Result<Option<ContainerSnapshot>, RuntimeError> {
            Ok(self.snapshots.get(id).cloned())
        }
        async fn connect_network(&self, _c: &str, _n: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn disconnect_network(&self, _c: &str, _n: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        fn stream_logs(
            &self,
            _id: &str,
            _o: LogOptions,
        ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
            Box::pin(futures_util::stream::empty())
        }
        fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>> {
            // Hand out the scripted events once; subsequent calls return
            // empty. `watch` only ever calls stream_events one time per run.
            let evs = self.events.lock().unwrap().take().unwrap_or_default();
            Box::pin(futures_util::stream::iter(evs))
        }
        async fn run_healthcheck(
            &self,
            _id: &str,
            _hc: &HealthcheckSpec,
        ) -> Result<HealthState, RuntimeError> {
            Ok(HealthState::Healthy)
        }
        fn name(&self) -> &'static str {
            "scripted"
        }
    }

    fn snap_with_labels(id: &str, labels: &[(&str, &str)]) -> ContainerSnapshot {
        let labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ContainerSnapshot {
            id: id.into(),
            // ContainerSnapshot.name is already leading-slash-trimmed by
            // the backend's mapping helpers (BollardBackend::map_inspect
            // does the trim); snapshot_to_report passes it through as-is.
            name: format!("{id}-name"),
            image: "nginx".into(),
            state: ContainerState::Running,
            stack: None,
            service: None,
            labels,
            created_at: SystemTime::UNIX_EPOCH,
            started_at: None,
            finished_at: None,
            exit_code: None,
            restart_count: 0,
            network_settings: NetworkSettings::default(),
            env: BTreeMap::new(),
            port_bindings: Vec::new(),
            restart: None,
        }
    }

    #[test]
    fn labels_report_includes_inferred_plex_port() {
        let mut snap = snap_with_labels("plex", &[("isengard.expose", "plex.vallee.casa")]);
        snap.network_settings
            .ports
            .insert("32400/tcp".into(), Vec::new());

        let report = snapshot_to_report(&snap).expect("labels report");

        assert_eq!(report.label_route_intents.len(), 1);
        assert_eq!(report.label_route_intents[0].name, "");
        assert_eq!(report.label_route_intents[0].hostname, "plex.vallee.casa");
        assert_eq!(report.label_route_intents[0].container_port, 32400);
        assert_eq!(report.label_route_intents[0].unresolved_reason, "");
    }

    #[test]
    fn explicit_port_label_wins_over_runtime_candidates() {
        let mut snap = snap_with_labels(
            "plex",
            &[
                ("isengard.expose", "plex.vallee.casa"),
                ("isengard.expose.port", "32400"),
            ],
        );
        snap.network_settings
            .ports
            .insert("80/tcp".into(), Vec::new());

        let report = snapshot_to_report(&snap).expect("labels report");

        assert_eq!(report.label_route_intents[0].container_port, 32400);
    }

    #[test]
    fn ambiguous_runtime_ports_report_unresolved_intent() {
        let mut snap = snap_with_labels("qbittorrent", &[("isengard.expose", "qb.vallee.casa")]);
        snap.network_settings
            .ports
            .insert("8080/tcp".into(), Vec::new());
        snap.network_settings
            .ports
            .insert("6881/tcp".into(), Vec::new());

        let report = snapshot_to_report(&snap).expect("labels report");

        assert_eq!(report.label_route_intents.len(), 1);
        assert_eq!(report.label_route_intents[0].container_port, 0);
        assert!(
            report.label_route_intents[0]
                .unresolved_reason
                .contains("multiple candidate ports")
        );
    }

    #[tokio::test]
    async fn labels_watch_emits_report_on_container_start_with_isengard_labels() {
        let snap = snap_with_labels("abc123", &[("isengard.expose", "demo.test")]);
        let backend = Arc::new(
            ScriptedBackend::new()
                .with_snapshot(snap.clone())
                .with_event(RuntimeEvent {
                    container_id: "abc123".into(),
                    event_type: RuntimeEventType::Start,
                    timestamp: SystemTime::UNIX_EPOCH,
                }),
        );
        let (tx, mut rx) = mpsc::channel::<AgentMessage>(8);
        watch(backend, tx).await.unwrap();
        let msg = rx.recv().await.expect("expected one labels report");
        match msg.payload {
            Some(agent_message::Payload::ContainerLabelsReport(r)) => {
                assert_eq!(r.container_id, "abc123");
                assert_eq!(r.container_name, "abc123-name");
                assert_eq!(r.image, "nginx");
                assert_eq!(
                    r.labels.get("isengard.expose").map(String::as_str),
                    Some("demo.test")
                );
            }
            other => panic!("expected ContainerLabelsReport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn labels_watch_emits_removed_on_container_die() {
        let backend = Arc::new(ScriptedBackend::new().with_event(RuntimeEvent {
            container_id: "deadbeef".into(),
            event_type: RuntimeEventType::Die { exit_code: Some(0) },
            timestamp: SystemTime::UNIX_EPOCH,
        }));
        let (tx, mut rx) = mpsc::channel::<AgentMessage>(8);
        watch(backend, tx).await.unwrap();
        let msg = rx.recv().await.expect("expected one removed message");
        match msg.payload {
            Some(agent_message::Payload::ContainerLabelsRemoved(r)) => {
                assert_eq!(r.container_id, "deadbeef");
            }
            other => panic!("expected ContainerLabelsRemoved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn labels_watch_skips_containers_without_isengard_labels() {
        // Container labelled by some other tool: a Start event arrives but
        // the watcher should NOT push a report up the channel.
        let snap = snap_with_labels(
            "noisy",
            &[
                ("com.docker.compose.project", "stranger"),
                ("traefik.http.routers.x.rule", "Host(`example`)"),
            ],
        );
        let backend = Arc::new(ScriptedBackend::new().with_snapshot(snap).with_event(
            RuntimeEvent {
                container_id: "noisy".into(),
                event_type: RuntimeEventType::Start,
                timestamp: SystemTime::UNIX_EPOCH,
            },
        ));
        let (tx, mut rx) = mpsc::channel::<AgentMessage>(8);
        watch(backend, tx).await.unwrap();
        // Channel should be empty; rx.recv() returns None once tx is dropped
        // by `watch` returning.
        assert!(
            rx.recv().await.is_none(),
            "expected no messages for non-isengard container",
        );
    }
}
