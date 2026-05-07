//! v0.3c compose import: orchestrate discovery -> inspect -> build YAML
//! -> write to disk -> report to controller.
//!
//! Spawned as a long-running task by `run_agent`. On a configurable cadence
//! it lists running containers, groups them by stack, skips stacks that
//! aren't `isengard.enable=true`, runs the import for any stack that lacks
//! a `compose.yaml`, and pushes the YAML to the controller via a
//! `StackComposeReport` AgentMessage.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bollard::Docker;
use bollard::container::ListContainersOptions;
use bollard::secret::ContainerInspectResponse;
use isengard_proto::pb::{AgentMessage, StackComposeReport, agent_message};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::compose_export::build_compose;
use crate::compose_writer::{ManualEditDetected, WriteOutcome, sha256_hex, write_compose};

/// Default cadence between import sweeps. Same order as the heartbeat
/// interval so a stack discovered on the first heartbeat ends up imported
/// before the second.
pub const DEFAULT_IMPORT_INTERVAL: Duration = Duration::from_secs(15);

/// Default mount point inside the agent container; bind-mounted to the
/// host's `/etc/isengard/stacks/`. Resolved at boot so tests can override
/// via `import_root`.
pub const DEFAULT_IMPORT_ROOT: &str = "/etc/isengard/stacks";

/// Spawn the import sweeper. Returns immediately; the work happens in a
/// background task that lives for the agent's lifetime.
///
/// `out` is the same `agent_msg_tx` the labels watcher uses. The reports
/// are pushed onto the sync stream by the existing loop.
pub fn spawn(
    docker: Arc<Docker>,
    out: mpsc::Sender<AgentMessage>,
    host_id: String,
    import_root: PathBuf,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the first immediate tick so we don't race the agent's other
        // boot-time work; the labels watcher and the sync stream have a
        // chance to settle first.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = run_once(&docker, &out, &host_id, &import_root).await {
                warn!(error = %e, "compose_import: sweep failed");
            }
        }
    });
}

/// Single sweep of the import logic. Public so the v0.3d `isd compose
/// import <stack>` action can call it directly without restarting the agent.
pub async fn run_once(
    docker: &Docker,
    out: &mpsc::Sender<AgentMessage>,
    host_id: &str,
    import_root: &std::path::Path,
) -> anyhow::Result<()> {
    let containers = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: false,
            ..Default::default()
        }))
        .await?;

    let mut stacks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in &containers {
        let labels = c.labels.as_ref();
        // Only `isengard.enable=true` stacks are in scope (per spec
        // non-goal: don't auto-import system containers).
        let enabled = labels
            .and_then(|m| m.get("isengard.enable"))
            .map(|v| v == "true")
            .unwrap_or(false);
        if !enabled {
            continue;
        }
        let stack_name = labels
            .and_then(|m| {
                m.get("isengard.stack")
                    .or_else(|| m.get("com.docker.compose.project"))
            })
            .cloned();
        let Some(stack_name) = stack_name else {
            continue;
        };
        if let Some(id) = c.id.as_ref() {
            stacks.entry(stack_name).or_default().push(id.clone());
        }
    }

    for (stack_name, container_ids) in stacks {
        if let Err(e) = import_stack(
            docker,
            out,
            host_id,
            import_root,
            &stack_name,
            &container_ids,
        )
        .await
        {
            warn!(error = %e, stack = %stack_name, "compose_import: stack import failed");
        }
    }
    Ok(())
}

async fn import_stack(
    docker: &Docker,
    out: &mpsc::Sender<AgentMessage>,
    host_id: &str,
    import_root: &std::path::Path,
    stack_name: &str,
    container_ids: &[String],
) -> anyhow::Result<()> {
    let mut inspects: Vec<ContainerInspectResponse> = Vec::with_capacity(container_ids.len());
    for id in container_ids {
        match docker.inspect_container(id, None).await {
            Ok(i) => inspects.push(i),
            Err(e) => warn!(error = %e, cid = %id, "compose_import: inspect failed"),
        }
    }
    if inspects.is_empty() {
        return Ok(());
    }

    let yaml = build_compose(stack_name, &inspects, Some(host_id))?;
    let stack_dir = import_root.join(stack_name);
    let now = chrono::Utc::now().to_rfc3339();

    let outcome = match write_compose(&stack_dir, &yaml, host_id, &now, false) {
        Ok(o) => o,
        Err(e) if e.downcast_ref::<ManualEditDetected>().is_some() => {
            // Spec: refuse without --force. v0.3c surfaces this as a warn;
            // v0.3d will fold the override into a dashboard CTA.
            warn!(stack = %stack_name, error = %e, "compose_import: manual edit detected, skipping");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let sha = sha256_hex(&yaml);
    debug!(
        stack = %stack_name,
        outcome = ?outcome,
        sha256 = %sha,
        path = %stack_dir.join("compose.yaml").display(),
        "compose_import: write outcome",
    );

    match outcome {
        WriteOutcome::Written => {
            info!(stack = %stack_name, "compose_import: wrote compose.yaml");
        }
        WriteOutcome::Unchanged => {
            // Still report so the controller can populate its column on
            // first agent connect. The sha lets the controller dedupe.
        }
    }

    let msg = AgentMessage {
        payload: Some(agent_message::Payload::StackComposeReport(
            StackComposeReport {
                stack_name: stack_name.to_string(),
                compose_yaml: yaml,
                sha256: sha,
                imported_at: now,
            },
        )),
    };
    if out.send(msg).await.is_err() {
        debug!("compose_import: agent_msg channel closed; stopping sweep");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Note: integration with the docker daemon is exercised manually per the
    // sprint's "Validate before opening PR" step. Unit-level coverage of the
    // mapping + writer is in compose_export and compose_writer.

    use super::DEFAULT_IMPORT_INTERVAL;
    use super::DEFAULT_IMPORT_ROOT;

    #[test]
    fn defaults_match_spec() {
        assert_eq!(DEFAULT_IMPORT_ROOT, "/etc/isengard/stacks");
        assert_eq!(DEFAULT_IMPORT_INTERVAL.as_secs(), 15);
    }
}
