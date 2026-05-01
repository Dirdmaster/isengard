//! Reads the local Docker container list, groups containers into stacks
//! based on the `com.docker.compose.project` label (or `isengard.stack=` override),
//! and converts to the wire-format `StackInfo`.

use std::collections::{BTreeMap, HashMap};

use bollard::Docker;
use bollard::container::ListContainersOptions;
use isengard_proto::pb::{ServiceInfo, StackInfo};
use tracing::warn;

/// Lightweight snapshot of one container — only the bits we need to derive stacks + services.
#[derive(Debug, Clone)]
pub struct ContainerSnapshot {
    pub name: String,
    pub image: String,
    pub state: String,
    pub labels: HashMap<String, String>,
}

/// Query Docker for all containers (running + stopped) and return ContainerSnapshots.
/// Returns an empty Vec on Docker error (logged at warn level — heartbeat should still send).
pub async fn list_container_snapshots() -> Vec<ContainerSnapshot> {
    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "container_snapshot: failed to connect to Docker");
            return Vec::new();
        }
    };

    let opts = ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    };

    let containers = match docker.list_containers(Some(opts)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "container_snapshot: list_containers failed");
            return Vec::new();
        }
    };

    containers
        .into_iter()
        .map(|c| {
            // bollard returns names with leading slash, take the first and strip it.
            let name = c
                .names
                .as_ref()
                .and_then(|ns| ns.first().cloned())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();
            let image = c.image.clone().unwrap_or_default();
            let state = c.state.clone().unwrap_or_else(|| "unknown".to_string());
            let labels = c.labels.unwrap_or_default();
            ContainerSnapshot {
                name,
                image,
                state,
                labels,
            }
        })
        .collect()
}

/// Build per-container ServiceInfo entries with stack association.
/// Mirrors derive_stacks naming: same precedence (isengard.stack > compose label > inferred).
pub fn derive_services(containers: &[ContainerSnapshot]) -> Vec<ServiceInfo> {
    containers
        .iter()
        .map(|c| {
            let stack = c
                .labels
                .get("isengard.stack")
                .or_else(|| c.labels.get("com.docker.compose.project"))
                .cloned()
                .unwrap_or_else(|| c.name.clone());

            ServiceInfo {
                name: c.name.clone(),
                image: c.image.clone(),
                state: c.state.clone(),
                stack: Some(stack),
            }
        })
        .collect()
}

/// Group container snapshots into stacks based on Docker Compose labels,
/// the optional `isengard.stack` override, or fall back to single-service
/// inferred stacks.
pub fn derive_stacks(containers: &[ContainerSnapshot]) -> Vec<StackInfo> {
    // BTreeMap for deterministic output ordering (helps tests + diffs).
    let mut grouped: BTreeMap<(String, &'static str), Vec<String>> = BTreeMap::new();

    for c in containers {
        let (name, source) = if let Some(n) = c.labels.get("isengard.stack") {
            (n.clone(), "manual")
        } else if let Some(n) = c.labels.get("com.docker.compose.project") {
            (n.clone(), "compose")
        } else {
            (c.name.clone(), "inferred")
        };

        grouped
            .entry((name, source))
            .or_default()
            .push(c.name.clone());
    }

    grouped
        .into_iter()
        .map(|((name, source), mut services)| {
            services.sort();
            StackInfo {
                name,
                source: source.to_string(),
                services,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, labels: &[(&str, &str)]) -> ContainerSnapshot {
        ContainerSnapshot {
            name: name.into(),
            image: format!("{name}:latest"),
            state: "running".into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        }
    }

    #[test]
    fn derives_stack_info_from_compose_label() {
        let containers = vec![
            snap("web", &[("com.docker.compose.project", "wordpress")]),
            snap("db", &[("com.docker.compose.project", "wordpress")]),
            snap("homer", &[]),
        ];

        let stacks = derive_stacks(&containers);
        assert_eq!(stacks.len(), 2);

        let wp = stacks.iter().find(|s| s.name == "wordpress").unwrap();
        assert_eq!(wp.source, "compose");
        assert_eq!(wp.services.len(), 2);
        assert!(wp.services.contains(&"db".to_string()));
        assert!(wp.services.contains(&"web".to_string()));

        let homer = stacks.iter().find(|s| s.name == "homer").unwrap();
        assert_eq!(homer.source, "inferred");
        assert_eq!(homer.services, vec!["homer".to_string()]);
    }

    #[test]
    fn isengard_stack_label_overrides_compose_label() {
        let containers = vec![snap(
            "x",
            &[
                ("com.docker.compose.project", "default-name"),
                ("isengard.stack", "override-name"),
            ],
        )];

        let stacks = derive_stacks(&containers);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].name, "override-name");
        assert_eq!(stacks[0].source, "manual");
    }

    #[test]
    fn derives_service_info_with_state_and_stack_link() {
        let mut compose_label = std::collections::HashMap::new();
        compose_label.insert("com.docker.compose.project".to_string(), "blog".to_string());

        let containers = vec![
            ContainerSnapshot {
                name: "web".into(),
                image: "nginx:1.25-alpine".into(),
                state: "running".into(),
                labels: compose_label.clone(),
            },
            ContainerSnapshot {
                name: "homer".into(),
                image: "b4bz/homer:latest".into(),
                state: "stopped".into(),
                labels: std::collections::HashMap::new(),
            },
        ];

        let services = derive_services(&containers);
        assert_eq!(services.len(), 2);

        let web = services.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.image, "nginx:1.25-alpine");
        assert_eq!(web.state, "running");
        assert_eq!(web.stack.as_deref(), Some("blog"));

        let homer = services.iter().find(|s| s.name == "homer").unwrap();
        assert_eq!(homer.state, "stopped");
        assert_eq!(homer.stack.as_deref(), Some("homer"));
    }
}
