//! Docker event subscription. On `start` / `update`, snapshots a container's
//! labels and pushes a `ContainerLabelsReport` up the sync stream. On `stop` /
//! `die` / `destroy`, pushes a `ContainerLabelsRemoved`.
//!
//! The watcher also runs an initial scan at startup so a freshly-connected
//! agent reports the world it sees, not just deltas.
//!
//! A container is reported when it carries either:
//!
//! - any `isengard.expose*` label (proxy / routing rules), OR
//! - any `isengard.policy.*` label (Phase 9b.1: container-scope policy
//!   discovery).
//!
//! Containers without either family of labels are filtered out: the
//! controller has nothing to do with them.

use std::collections::HashMap;

use bollard::Docker;
use bollard::container::ListContainersOptions;
use bollard::system::EventsOptions;
use futures_util::StreamExt;
use isengard_proto::pb::{
    AgentMessage, ContainerLabelsRemoved, ContainerLabelsReport, agent_message,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Long-running watcher: initial scan, then drain Docker's event stream.
/// Returns Err only on a fatal Docker error (the spawn site logs and exits).
pub async fn watch(docker: Docker, out: mpsc::Sender<AgentMessage>) -> anyhow::Result<()> {
    initial_scan(&docker, &out).await?;

    let mut filters = HashMap::new();
    filters.insert("type".to_string(), vec!["container".to_string()]);
    filters.insert(
        "event".to_string(),
        vec![
            "start".into(),
            "stop".into(),
            "die".into(),
            "update".into(),
            "destroy".into(),
        ],
    );

    let mut events = docker.events(Some(EventsOptions::<String> {
        since: None,
        until: None,
        filters,
    }));

    while let Some(ev) = events.next().await {
        let ev = match ev {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "labels: docker event stream error");
                continue;
            }
        };
        let Some(actor) = ev.actor else { continue };
        let Some(cid) = actor.id else { continue };
        let action = ev.action.as_deref().unwrap_or("");

        match action {
            "start" | "update" => match inspect_to_report(&docker, &cid).await {
                Ok(Some(report)) => {
                    let _ = out
                        .send(AgentMessage {
                            payload: Some(agent_message::Payload::ContainerLabelsReport(report)),
                        })
                        .await;
                }
                Ok(None) => {
                    debug!(cid = %cid, "labels: container has no isengard.expose labels, skipping");
                }
                Err(e) => {
                    warn!(error = %e, cid = %cid, "labels: inspect failed");
                }
            },
            "stop" | "die" | "destroy" => {
                let _ = out
                    .send(AgentMessage {
                        payload: Some(agent_message::Payload::ContainerLabelsRemoved(
                            ContainerLabelsRemoved { container_id: cid },
                        )),
                    })
                    .await;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn initial_scan(docker: &Docker, out: &mpsc::Sender<AgentMessage>) -> anyhow::Result<()> {
    let containers = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: false,
            ..Default::default()
        }))
        .await?;
    for c in containers {
        let Some(id) = c.id else { continue };
        match inspect_to_report(docker, &id).await {
            Ok(Some(report)) => {
                let _ = out
                    .send(AgentMessage {
                        payload: Some(agent_message::Payload::ContainerLabelsReport(report)),
                    })
                    .await;
            }
            Ok(None) => {}
            Err(e) => warn!(error = %e, cid = %id, "labels: initial inspect failed"),
        }
    }
    Ok(())
}

/// `true` if the controller wants to know about a container that carries
/// this label key. Today: any `isengard.expose*` (routing) or
/// `isengard.policy.*` (Phase 9b.1) key.
///
/// Lives here (not in `isengard-core`) because it's a coupling between the
/// agent's report filter and the two consumer subsystems on the controller.
/// Splitting it would force the agent to depend on every consumer crate.
pub(crate) fn is_reportable_label_key(key: &str) -> bool {
    key.starts_with("isengard.expose") || key.starts_with("isengard.policy.")
}

async fn inspect_to_report(
    docker: &Docker,
    cid: &str,
) -> anyhow::Result<Option<ContainerLabelsReport>> {
    let inspect = docker.inspect_container(cid, None).await?;
    let labels = inspect
        .config
        .as_ref()
        .and_then(|c| c.labels.clone())
        .unwrap_or_default();
    if !labels.keys().any(|k| is_reportable_label_key(k)) {
        return Ok(None);
    }
    let name = inspect
        .name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let image = inspect
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .unwrap_or_default();
    Ok(Some(ContainerLabelsReport {
        container_id: cid.to_string(),
        container_name: name,
        image,
        labels,
    }))
}

#[cfg(test)]
mod tests {
    use super::is_reportable_label_key;

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
}
