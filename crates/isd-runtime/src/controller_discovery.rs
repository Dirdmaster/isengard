//! Discover the Isengard controller container on a docker host.
//!
//! Operator-side flow (called from `isd::Session::open`):
//!
//! 1. Connect to docker via the operator's docker context URL.
//! 2. `docker ps --filter label=io.isengard.role=controller`.
//! 3. Read the matching container's published port for 9418/tcp.
//! 4. Verify the container's `io.isengard.api.version` label matches
//!    [`API_VERSION`].
//! 5. Return the reachable URL (caller handles SSH-LocalForward if needed).
//!
//! Label name + value constants live in [`crate::discovery_labels`]; this
//! module imports them rather than redeclaring so the compose recipe + the
//! discovery call site stay in lock-step.

use std::collections::HashMap;

use bollard::Docker;
use bollard::container::ListContainersOptions;
use thiserror::Error;

use crate::discovery_labels::{API_VERSION, API_VERSION_LABEL, ROLE_CONTROLLER, ROLE_LABEL};

/// Controller's REST listener port inside the container. The host-side
/// mapping is read from the bollard `Port` entries.
const CONTROLLER_REST_PORT: u16 = 9418;

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error(
        "no isengard controller found on this docker host; bring one up with `docker compose -f install/compose.yaml up -d`"
    )]
    NotFound,
    #[error(
        "multiple isengard controllers on this docker host: {0:?}; multi-controller-per-host is unsupported in v1"
    )]
    Multiple(Vec<String>),
    #[error("controller missing the {0} label; expected {1} but the container has no value")]
    MissingVersionLabel(&'static str, u32),
    #[error(
        "controller API version skew: isd speaks v{isd}, controller speaks v{controller}; upgrade {direction}"
    )]
    VersionSkew {
        isd: u32,
        controller: u32,
        direction: &'static str,
    },
    #[error(
        "controller container has no published port for 9418/tcp; ensure compose.yaml maps 127.0.0.1:9418:9418"
    )]
    NoPublishedPort,
    #[error("docker API error: {0}")]
    Docker(#[from] bollard::errors::Error),
}

/// What the caller needs to make a REST request to the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerEndpoint {
    /// Container ID (for diagnostics + cache invalidation).
    pub container_id: String,
    /// API version the controller advertises.
    pub api_version: u32,
    /// Host-side bind address (e.g. `127.0.0.1`).
    pub host_ip: String,
    /// Host-side port mapped to the controller's REST listener (9418/tcp).
    pub host_port: u16,
}

/// Find the controller container on a docker host and return its reachable
/// REST endpoint metadata. Caller is responsible for actually opening a
/// connection (and an SSH-LocalForward if the docker context is SSH-backed).
pub async fn discover(docker: &Docker) -> Result<ControllerEndpoint, DiscoveryError> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{ROLE_LABEL}={ROLE_CONTROLLER}")],
    );
    let options = ListContainersOptions::<String> {
        all: false, // running only
        filters,
        ..Default::default()
    };
    let containers = docker.list_containers(Some(options)).await?;
    match containers.len() {
        0 => Err(DiscoveryError::NotFound),
        n if n > 1 => {
            let names: Vec<String> = containers
                .iter()
                .filter_map(|c| c.names.as_ref().and_then(|v| v.first().cloned()))
                .collect();
            Err(DiscoveryError::Multiple(names))
        }
        _ => {
            let c = &containers[0];
            let container_id = c.id.clone().unwrap_or_default();
            let labels = c.labels.clone().unwrap_or_default();
            let api_version_str =
                labels
                    .get(API_VERSION_LABEL)
                    .ok_or(DiscoveryError::MissingVersionLabel(
                        API_VERSION_LABEL,
                        API_VERSION,
                    ))?;
            let api_version: u32 = api_version_str
                .parse()
                .map_err(|_| DiscoveryError::MissingVersionLabel(API_VERSION_LABEL, API_VERSION))?;
            if api_version != API_VERSION {
                let direction = if api_version > API_VERSION {
                    "isd (the operator CLI)"
                } else {
                    "the controller image"
                };
                return Err(DiscoveryError::VersionSkew {
                    isd: API_VERSION,
                    controller: api_version,
                    direction,
                });
            }
            let port = c
                .ports
                .as_ref()
                .and_then(|ports| {
                    ports.iter().find(|p| {
                        p.private_port == CONTROLLER_REST_PORT
                            && p.typ == Some(bollard::models::PortTypeEnum::TCP)
                    })
                })
                .ok_or(DiscoveryError::NoPublishedPort)?;
            let host_ip = port.ip.clone().unwrap_or_else(|| "127.0.0.1".to_string());
            let host_port = port.public_port.ok_or(DiscoveryError::NoPublishedPort)?;
            Ok(ControllerEndpoint {
                container_id,
                api_version,
                host_ip,
                host_port,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    // The bollard Docker type can't be cheaply mocked; integration tests
    // against a real docker live in the isd crate (Phase 4 covers them
    // via wiremock + a stub container around the Session::open flow).
    //
    // These tests cover the pure error mapping by exercising the
    // DiscoveryError Display impls (operator-visible strings).

    use super::*;

    #[test]
    fn not_found_error_has_actionable_message() {
        let msg = format!("{}", DiscoveryError::NotFound);
        assert!(msg.contains("no isengard controller"));
        assert!(msg.contains("docker compose"));
    }

    #[test]
    fn multiple_error_lists_names() {
        let msg = format!("{}", DiscoveryError::Multiple(vec!["a".into(), "b".into()]));
        assert!(msg.contains("\"a\""));
        assert!(msg.contains("\"b\""));
    }

    #[test]
    fn version_skew_error_includes_both_versions_and_direction() {
        let msg = format!(
            "{}",
            DiscoveryError::VersionSkew {
                isd: 1,
                controller: 2,
                direction: "isd (the operator CLI)",
            }
        );
        assert!(msg.contains("v1"));
        assert!(msg.contains("v2"));
        assert!(msg.contains("upgrade isd"));
    }

    #[test]
    fn no_published_port_message_points_to_compose() {
        let msg = format!("{}", DiscoveryError::NoPublishedPort);
        assert!(msg.contains("9418/tcp"));
        assert!(msg.contains("compose.yaml"));
    }
}
