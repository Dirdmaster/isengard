//! v0.3d compose apply: take a [`ReconcilePlan`] + the desired compose,
//! drive bollard to bring the running containers in line.
//!
//! Scope: v0.3d implements `Start | Recreate | Stop` for the smoke-test
//! shape (image, env, ports, restart, labels). Networks, volumes,
//! healthchecks, and depends_on are passed through best-effort but not
//! reconciled per-service. Follow-up work tracked in the v0.3 status
//! note.

use std::collections::HashMap;
use std::sync::Arc;

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use bollard::secret::{
    ContainerInspectResponse, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};

use crate::compose_reconciler::{
    DesiredCompose, DesiredService, ReconcilePlan, RunningService, ServiceOp, build_plan,
    parse_compose,
};

/// Outcome of applying a [`ReconcilePlan`]. Each entry mirrors the
/// matching plan op; failures attach an error string but do not abort
/// the whole sweep.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub op: ServiceOp,
    pub error: Option<String>,
}

/// Bring the running containers for `stack` in line with `compose_yaml`.
/// Returns the plan that was attempted plus per-op outcomes.
pub async fn reconcile_stack(
    docker: &Docker,
    stack_name: &str,
    compose_yaml: &str,
) -> anyhow::Result<(ReconcilePlan, Vec<ApplyOutcome>)> {
    let desired = parse_compose(compose_yaml)?;
    let running = list_running_for_stack(docker, stack_name).await?;
    let plan = build_plan(stack_name, &desired, &running);
    let outcomes = apply_plan(docker, stack_name, &desired, &plan).await;
    Ok((plan, outcomes))
}

/// List the running containers belonging to `stack_name`. Filters by
/// either `com.docker.compose.project=<stack>` or
/// `isengard.stack=<stack>`, projecting each into a [`RunningService`].
pub async fn list_running_for_stack(
    docker: &Docker,
    stack_name: &str,
) -> anyhow::Result<Vec<RunningService>> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![
            format!("com.docker.compose.project={stack_name}"),
            format!("isengard.stack={stack_name}"),
        ],
    );
    let containers = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: false,
            filters,
            ..Default::default()
        }))
        .await
        .map_err(|e| anyhow::anyhow!("list_containers for stack {stack_name}: {e}"))?;

    let mut out = Vec::with_capacity(containers.len());
    for c in containers {
        let Some(id) = c.id else { continue };
        match docker.inspect_container(&id, None).await {
            Ok(inspect) => {
                if let Some(rs) = RunningService::from_inspect(&inspect) {
                    out.push(rs);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, container_id = %id, "compose_apply: inspect failed")
            }
        }
    }
    Ok(out)
}

/// Apply each op in `plan`. Returns one outcome per op, in the same
/// order. Failures don't short-circuit: the operator gets the full
/// summary at the end.
pub async fn apply_plan(
    docker: &Docker,
    stack_name: &str,
    desired: &DesiredCompose,
    plan: &ReconcilePlan,
) -> Vec<ApplyOutcome> {
    let mut outcomes = Vec::with_capacity(plan.ops.len());
    for op in &plan.ops {
        let result = match op {
            ServiceOp::NoChange { .. } => Ok(()),
            ServiceOp::Start { service, .. } => {
                let Some(svc) = desired.services.get(service) else {
                    outcomes.push(ApplyOutcome {
                        op: op.clone(),
                        error: Some(format!("service {service} missing from compose")),
                    });
                    continue;
                };
                ensure_container_started(docker, stack_name, svc, None).await
            }
            ServiceOp::Recreate { service, .. } => {
                let Some(svc) = desired.services.get(service) else {
                    outcomes.push(ApplyOutcome {
                        op: op.clone(),
                        error: Some(format!("service {service} missing from compose")),
                    });
                    continue;
                };
                // Find the existing container by service name and stop+remove
                // before starting fresh. Best-effort: if the container is gone
                // we still attempt the start.
                let existing = list_running_for_stack(docker, stack_name)
                    .await
                    .unwrap_or_default();
                let prior = existing.iter().find(|r| &r.service_name == service);
                if let Some(prior) = prior {
                    let _ = stop_and_remove(docker, &prior.container_id).await;
                }
                ensure_container_started(
                    docker,
                    stack_name,
                    svc,
                    prior.map(|r| r.container_id.as_str()),
                )
                .await
            }
            ServiceOp::Stop { container_id, .. } => stop_and_remove(docker, container_id).await,
        };
        outcomes.push(ApplyOutcome {
            op: op.clone(),
            error: result.err().map(|e| format!("{e:#}")),
        });
    }
    outcomes
}

async fn ensure_container_started(
    docker: &Docker,
    stack_name: &str,
    svc: &DesiredService,
    _prior_container: Option<&str>,
) -> anyhow::Result<()> {
    let Some(image) = svc.image.as_ref() else {
        return Err(anyhow::anyhow!("service {} has no image", svc.name));
    };
    let cfg = build_create_config(stack_name, svc, image)?;
    let container_name = svc
        .container_name
        .clone()
        .unwrap_or_else(|| format!("{stack_name}-{}", svc.name));
    // Best-effort: a previous run may have left a container with the
    // same name. Stop+remove it before creating a new one. bollard's
    // create_container errors out on conflict, which is the most
    // common failure mode for the smoke test.
    let _ = stop_named(docker, &container_name).await;

    let create = docker
        .create_container(
            Some(CreateContainerOptions {
                name: container_name.clone(),
                platform: None,
            }),
            cfg,
        )
        .await
        .map_err(|e| anyhow::anyhow!("create_container {container_name}: {e}"))?;
    docker
        .start_container(&create.id, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| anyhow::anyhow!("start_container {container_name}: {e}"))?;
    Ok(())
}

fn build_create_config(
    stack_name: &str,
    svc: &DesiredService,
    image: &str,
) -> anyhow::Result<Config<String>> {
    // Environment: KEY=VALUE pairs.
    let mut env: Vec<String> = svc
        .environment
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    env.sort();

    // Labels: ensure compose project + service labels are present so
    // the next reconcile can match the container back to the service.
    let mut labels: HashMap<String, String> = HashMap::new();
    labels.insert(
        "com.docker.compose.project".to_string(),
        stack_name.to_string(),
    );
    labels.insert("com.docker.compose.service".to_string(), svc.name.clone());
    labels.insert("isengard.stack".to_string(), stack_name.to_string());
    for (k, v) in &svc.labels {
        labels.insert(k.clone(), v.clone());
    }

    // Ports.
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
    for spec in &svc.ports {
        if let Some((host, container)) = parse_port_mapping(spec) {
            let key = if container.contains('/') {
                container.clone()
            } else {
                format!("{container}/tcp")
            };
            port_bindings
                .entry(key.clone())
                .or_insert_with(|| Some(Vec::new()))
                .as_mut()
                .unwrap()
                .push(PortBinding {
                    host_ip: None,
                    host_port: Some(host),
                });
            exposed_ports.insert(key, HashMap::new());
        }
    }

    let restart_policy = svc.restart.as_deref().and_then(|s| match s {
        "no" => Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::NO),
            maximum_retry_count: None,
        }),
        "always" => Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::ALWAYS),
            maximum_retry_count: None,
        }),
        "unless-stopped" => Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        }),
        "on-failure" => Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::ON_FAILURE),
            maximum_retry_count: None,
        }),
        _ => None,
    });

    let host_config = HostConfig {
        port_bindings: if port_bindings.is_empty() {
            None
        } else {
            Some(port_bindings)
        },
        restart_policy,
        ..Default::default()
    };

    Ok(Config {
        image: Some(image.to_string()),
        cmd: svc.command.clone(),
        entrypoint: svc.entrypoint.clone(),
        env: Some(env),
        labels: Some(labels),
        exposed_ports: if exposed_ports.is_empty() {
            None
        } else {
            Some(exposed_ports)
        },
        host_config: Some(host_config),
        ..Default::default()
    })
}

/// Parse a compose port string like `"8080:80"` / `"127.0.0.1:8080:80/tcp"`
/// / `"80"`. Returns `(host_port, container_port_with_proto)` or `None`
/// for unmappable specs.
fn parse_port_mapping(spec: &str) -> Option<(String, String)> {
    // Normalize: drop optional `host_ip:` prefix when present.
    let bare = match spec.matches(':').count() {
        0 => return None, // bare container port; skip
        1 => spec.to_string(),
        _ => {
            // ip:host:container: drop the ip. v0.3d ignores host_ip.
            spec.split_once(':').map(|(_ip, rest)| rest.to_string())?
        }
    };
    let (host, container) = bare.split_once(':')?;
    Some((host.to_string(), container.to_string()))
}

async fn stop_named(docker: &Docker, name: &str) -> anyhow::Result<()> {
    // Inspect by name; if found, stop + remove.
    match docker.inspect_container(name, None).await {
        Ok(ContainerInspectResponse { id: Some(id), .. }) => stop_and_remove(docker, &id).await,
        Ok(_) => Ok(()),
        Err(_) => Ok(()), // not found
    }
}

async fn stop_and_remove(docker: &Docker, container_id: &str) -> anyhow::Result<()> {
    let _ = docker
        .stop_container(container_id, Some(StopContainerOptions { t: 10 }))
        .await;
    docker
        .remove_container(
            container_id,
            Some(RemoveContainerOptions {
                force: true,
                v: false,
                link: false,
            }),
        )
        .await
        .map_err(|e| {
            let s = e.to_string();
            if s.contains("404") || s.to_lowercase().contains("no such container") {
                anyhow::anyhow!("noop")
            } else {
                anyhow::anyhow!("remove_container {container_id}: {e}")
            }
        })
        .or_else(|e| {
            if format!("{e:#}") == "noop" {
                Ok(())
            } else {
                Err(e)
            }
        })
}

/// Convenience: same as [`reconcile_stack`] but the caller already has
/// an `Arc<Docker>` (matches the agent's main loop).
pub async fn reconcile_stack_arc(
    docker: Arc<Docker>,
    stack_name: &str,
    compose_yaml: &str,
) -> anyhow::Result<(ReconcilePlan, Vec<ApplyOutcome>)> {
    reconcile_stack(&docker, stack_name, compose_yaml).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_basic_mapping() {
        assert_eq!(
            parse_port_mapping("8080:80"),
            Some(("8080".into(), "80".into()))
        );
    }

    #[test]
    fn parse_port_with_host_ip_drops_ip() {
        assert_eq!(
            parse_port_mapping("127.0.0.1:8080:80/tcp"),
            Some(("8080".into(), "80/tcp".into()))
        );
    }

    #[test]
    fn parse_port_bare_returns_none() {
        assert_eq!(parse_port_mapping("80"), None);
    }

    #[test]
    fn build_config_includes_compose_project_labels() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        let cfg = build_create_config("hello", &svc, "nginx").unwrap();
        let labels = cfg.labels.unwrap();
        assert_eq!(labels["com.docker.compose.project"], "hello");
        assert_eq!(labels["com.docker.compose.service"], "web");
        assert_eq!(labels["isengard.stack"], "hello");
    }

    #[test]
    fn build_config_passes_environment() {
        let mut svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            ..Default::default()
        };
        svc.environment.insert("FOO".into(), "bar".into());
        let cfg = build_create_config("hello", &svc, "nginx").unwrap();
        let env = cfg.env.unwrap();
        assert!(env.iter().any(|e| e == "FOO=bar"));
    }

    #[test]
    fn build_config_translates_restart_policy() {
        let svc = DesiredService {
            name: "web".into(),
            image: Some("nginx".into()),
            restart: Some("unless-stopped".into()),
            ..Default::default()
        };
        let cfg = build_create_config("hello", &svc, "nginx").unwrap();
        let rp = cfg.host_config.unwrap().restart_policy.unwrap();
        assert!(matches!(
            rp.name,
            Some(RestartPolicyNameEnum::UNLESS_STOPPED)
        ));
    }
}
