//! Docker event subscription. On `start` / `update`, snapshots a container's
//! labels and pushes a `ContainerLabelsReport` up the sync stream. On `stop` /
//! `die` / `destroy`, pushes a `ContainerLabelsRemoved`.
//!
//! The watcher also runs an initial scan at startup so a freshly-connected
//! agent reports the world it sees, not just deltas.
//!
//! Containers without an `isengard.expose*` label are filtered out at the
//! report site — the controller only cares about ones we'd actually route.

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
    if !labels.keys().any(|k| k.starts_with("isengard.expose")) {
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
