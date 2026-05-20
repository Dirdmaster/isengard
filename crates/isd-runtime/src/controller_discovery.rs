#![doc = include_str!("../docs/controller-discovery.md")]

use std::collections::HashMap;

use bollard::Docker;
use bollard::container::ListContainersOptions;
use thiserror::Error;

use crate::discovery_labels::{API_VERSION, API_VERSION_LABEL, ROLE_CONTROLLER, ROLE_LABEL};

/// REST listener port the controller binds inside its container.
///
/// The host-side mapping is read from the bollard `Port` entries on
/// the matching container. The controller binds loopback only inside
/// the container; the docker port publishing surfaces a host-side
/// address (typically `127.0.0.1:9418`) for `isd` to connect to.
const CONTROLLER_REST_PORT: u16 = 9418;

/// Failures the discovery flow can surface to `isd`.
///
/// Each variant carries enough text for `isd` to render an operator-facing
/// error directly. The `Display` impls embed remediation hints (the
/// compose command to start a controller, the upgrade direction, etc.).
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// No container with `io.isengard.role=controller` is running on
    /// the docker host. The error text points the operator at the
    /// compose recipe that brings one up.
    #[error(
        "no isengard controller found on this docker host; bring one up with `docker compose -f install/compose.yaml up -d`"
    )]
    NotFound,
    /// More than one controller container is running on the host.
    /// Carries the container names so the operator can pick which to
    /// remove. Multi-controller-per-host is unsupported in v1.
    #[error(
        "multiple isengard controllers on this docker host: {0:?}; multi-controller-per-host is unsupported in v1"
    )]
    Multiple(Vec<String>),
    /// The controller container is running but missing the
    /// `io.isengard.api.version` label, or the label parses as
    /// something other than a `u32`. Names the expected label key and
    /// the version `isd` speaks.
    #[error("controller missing the {0} label; expected {1} but the container has no value")]
    MissingVersionLabel(&'static str, u32),
    /// The controller advertises a different API version than `isd`
    /// speaks. `direction` names which side to upgrade.
    #[error(
        "controller API version skew: isd speaks v{isd}, controller speaks v{controller}; upgrade {direction}"
    )]
    VersionSkew {
        /// API version `isd` speaks (the local constant [`API_VERSION`]).
        isd: u32,
        /// API version the controller advertises via
        /// [`API_VERSION_LABEL`].
        controller: u32,
        /// Human-readable hint for which side to upgrade: either
        /// `isd (the operator CLI)` or `the controller image`.
        direction: &'static str,
    },
    /// The controller container is running but `9418/tcp` is not
    /// published to the host. The compose recipe needs
    /// `127.0.0.1:9418:9418` for the discovery flow to hand `isd` a
    /// reachable address.
    #[error(
        "controller container has no published port for 9418/tcp; ensure compose.yaml maps 127.0.0.1:9418:9418"
    )]
    NoPublishedPort,
    /// The docker API call itself failed. Forwarded from `bollard`.
    #[error("docker API error: {0}")]
    Docker(#[from] bollard::errors::Error),
}

/// Reachable address of an Isengard controller on a docker host.
///
/// Built by [`discover`]. The caller uses `host_ip` + `host_port` to
/// open a REST connection; when the docker context is SSH-backed the
/// caller wraps that connection in an SSH-LocalForward so the
/// loopback-only controller is reachable from the operator's laptop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerEndpoint {
    /// Container ID. Used for diagnostics and as a cache key when the
    /// caller memoises discovery across multiple `isd` invocations.
    pub container_id: String,
    /// API version the controller advertises. Always equal to
    /// [`API_VERSION`] when `discover` returns `Ok` (a mismatch surfaces
    /// as [`DiscoveryError::VersionSkew`]).
    pub api_version: u32,
    /// Host-side bind address the controller's REST port maps to. The
    /// compose recipe binds `127.0.0.1` for security; anything else is
    /// an operator-side reconfiguration.
    pub host_ip: String,
    /// Host-side port the controller's REST listener (9418/tcp) is
    /// mapped to. Compose recipe defaults to `9418`.
    pub host_port: u16,
}

/// Finds the controller container on a docker host and returns its
/// reachable REST endpoint metadata.
///
/// The caller is responsible for opening the actual connection and for
/// wrapping it in an SSH-LocalForward when the docker context is
/// SSH-backed.
///
/// # Errors
///
/// Returns [`DiscoveryError`] on any of the failure modes described in
/// the module docs: no controller, multiple controllers, missing or
/// skewed API version, REST port not published to the host, or a raw
/// `bollard` round-trip failure.
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
    //! Coverage for the [`DiscoveryError`] `Display` impls.
    //!
    //! The bollard `Docker` type does not mock cheaply, so integration
    //! coverage for the live discovery flow lives in the `isd` crate
    //! (wiremock plus a stub container around the `Session::open` path).
    //! These tests pin the operator-visible error strings: each one
    //! contains an actionable remediation hint that the caller emits
    //! verbatim.

    use super::*;

    /// `NotFound` names the docker compose command that brings up a
    /// controller.
    #[test]
    fn not_found_error_has_actionable_message() {
        let msg = format!("{}", DiscoveryError::NotFound);
        assert!(msg.contains("no isengard controller"));
        assert!(msg.contains("docker compose"));
    }

    /// `Multiple` lists the offending container names so the operator
    /// knows which to remove.
    #[test]
    fn multiple_error_lists_names() {
        let msg = format!("{}", DiscoveryError::Multiple(vec!["a".into(), "b".into()]));
        assert!(msg.contains("\"a\""));
        assert!(msg.contains("\"b\""));
    }

    /// `VersionSkew` includes both versions and the upgrade direction.
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

    /// `NoPublishedPort` points the operator at the compose mapping
    /// they need to add.
    #[test]
    fn no_published_port_message_points_to_compose() {
        let msg = format!("{}", DiscoveryError::NoPublishedPort);
        assert!(msg.contains("9418/tcp"));
        assert!(msg.contains("compose.yaml"));
    }
}
