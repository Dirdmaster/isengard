//! Pure helpers that lift fields off bollard's `ContainerInspectResponse`
//! into the cross-crate `UpdateTriggerInfo` shape. Each function is small,
//! independently testable, and side-effect free.
//!
//! The updater calls these once per `needs_update`
//! container before consulting the [`isengard_core::UpdateDispatcher`].

use bollard::models::{ContainerInspectResponse, ContainerSummary};
use std::collections::HashMap;

/// Compose's service-name label. Set on every compose-managed
/// container.
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

/// Isengard label that selects the deploy strategy.
const ISENGARD_STRATEGY_LABEL: &str = "isengard.deploy.strategy";

/// Resolve a service name. Compose label wins; otherwise strip the leading
/// `/` and any trailing `-<index>` suffix from the container name.
pub fn service_name(summary: &ContainerSummary, inspect: &ContainerInspectResponse) -> String {
    if let Some(s) = inspect_label(inspect, COMPOSE_SERVICE_LABEL) {
        return s.to_string();
    }
    if let Some(labels) = summary.labels.as_ref() {
        if let Some(s) = labels.get(COMPOSE_SERVICE_LABEL) {
            return s.clone();
        }
    }
    let raw = summary
        .names
        .as_ref()
        .and_then(|n| n.first())
        .map(|s| s.trim_start_matches('/').to_string())
        .or_else(|| {
            inspect
                .name
                .as_deref()
                .map(|s| s.trim_start_matches('/').to_string())
        })
        .unwrap_or_default();
    strip_index_suffix(&raw)
}

/// Compose names containers as `<project>-<service>-<n>`. Strip the trailing
/// `-<digits>` so callers see the bare service name. If there's no numeric
/// suffix, return the input unchanged.
fn strip_index_suffix(name: &str) -> String {
    if let Some(idx) = name.rfind('-') {
        let (head, tail) = name.split_at(idx);
        let tail_digits = &tail[1..];
        if !tail_digits.is_empty() && tail_digits.chars().all(|c| c.is_ascii_digit()) {
            return head.to_string();
        }
    }
    name.to_string()
}

/// First (lowest-numbered) container port from `network_settings.ports` keys.
/// Bollard renders keys as `"8080/tcp"`; we parse the integer prefix.
pub fn first_container_port(inspect: &ContainerInspectResponse) -> Option<u16> {
    let ports = inspect.network_settings.as_ref()?.ports.as_ref()?;
    let mut best: Option<u16> = None;
    for key in ports.keys() {
        let n = key.split_once('/').map(|(p, _)| p).unwrap_or(key);
        if let Ok(p) = n.parse::<u16>() {
            best = Some(match best {
                Some(b) => b.min(p),
                None => p,
            });
        }
    }
    best
}

/// True when the container has a Docker healthcheck configured OR Docker is
/// reporting a health state for it (some images bake `HEALTHCHECK` into the
/// image and `inspect.config.healthcheck` reflects it; others only show up in
/// `inspect.state.health`).
pub fn has_healthcheck(inspect: &ContainerInspectResponse) -> bool {
    let cfg_has = inspect
        .config
        .as_ref()
        .and_then(|c| c.healthcheck.as_ref())
        .is_some();
    let state_has = inspect
        .state
        .as_ref()
        .and_then(|s| s.health.as_ref())
        .is_some();
    cfg_has || state_has
}

/// Read-write bind mounts and named volumes. Prefers `host_config.mounts`
/// (newer API; carries explicit `read_only` flags). Falls back to the
/// classic `host_config.binds` strings, parsing the `:ro` suffix.
///
/// Returned strings are container-side destination paths (best effort —
/// for `mounts` we use `target`; for `binds` we parse out the destination
/// component). The eligibility classifier only cares about the count, not
/// the values, but keeping paths makes diagnostics readable.
pub fn rw_volume_mounts(inspect: &ContainerInspectResponse) -> Vec<String> {
    let mut out = Vec::new();
    let host_cfg = match inspect.host_config.as_ref() {
        Some(h) => h,
        None => return out,
    };
    if let Some(mounts) = host_cfg.mounts.as_ref() {
        for m in mounts {
            let read_only = m.read_only.unwrap_or(false);
            if !read_only {
                if let Some(t) = m.target.as_deref() {
                    out.push(t.to_string());
                }
            }
        }
        // If `mounts` is present we trust it — Compose puts everything into
        // `mounts` when using the new spec.
        return out;
    }
    if let Some(binds) = host_cfg.binds.as_ref() {
        for b in binds {
            // Bind format: "<src>:<dst>" or "<src>:<dst>:<opts>"
            let parts: Vec<&str> = b.splitn(3, ':').collect();
            let dst = match parts.get(1) {
                Some(d) => *d,
                None => continue,
            };
            let opts = parts.get(2).copied().unwrap_or("");
            let read_only = opts.split(',').any(|o| o == "ro");
            if !read_only {
                out.push(dst.to_string());
            }
        }
    }
    out
}

/// Value of the `isengard.deploy.strategy` label, if set.
pub fn label_strategy(inspect: &ContainerInspectResponse) -> Option<String> {
    inspect_label(inspect, ISENGARD_STRATEGY_LABEL).map(str::to_string)
}

/// Reads a label off the container's `Config.Labels` map.
fn inspect_label<'a>(inspect: &'a ContainerInspectResponse, key: &str) -> Option<&'a str> {
    inspect
        .config
        .as_ref()
        .and_then(|c| c.labels.as_ref())
        .and_then(|l: &HashMap<String, String>| l.get(key))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerConfig, ContainerInspectResponse, ContainerState, ContainerSummary, HealthConfig,
        HostConfig, Mount, NetworkSettings, PortBinding,
    };
    use std::collections::HashMap;

    fn empty_inspect() -> ContainerInspectResponse {
        ContainerInspectResponse::default()
    }

    fn empty_summary() -> ContainerSummary {
        ContainerSummary::default()
    }

    #[test]
    fn service_name_prefers_compose_label() {
        let mut labels = HashMap::new();
        labels.insert("com.docker.compose.service".to_string(), "web".to_string());
        let inspect = ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(labels),
                ..Default::default()
            }),
            name: Some("/blog-web-1".into()),
            ..Default::default()
        };
        assert_eq!(service_name(&empty_summary(), &inspect), "web");
    }

    #[test]
    fn service_name_strips_compose_index_suffix() {
        let inspect = ContainerInspectResponse {
            name: Some("/blog-web-1".into()),
            ..Default::default()
        };
        assert_eq!(service_name(&empty_summary(), &inspect), "blog-web");
    }

    #[test]
    fn service_name_keeps_name_without_numeric_suffix() {
        let inspect = ContainerInspectResponse {
            name: Some("/standalone".into()),
            ..Default::default()
        };
        assert_eq!(service_name(&empty_summary(), &inspect), "standalone");
    }

    #[test]
    fn first_container_port_picks_lowest() {
        let mut ports = HashMap::new();
        ports.insert("9000/tcp".to_string(), None::<Vec<PortBinding>>);
        ports.insert("8080/tcp".to_string(), None);
        ports.insert("8443/tcp".to_string(), None);
        let inspect = ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                ports: Some(ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(first_container_port(&inspect), Some(8080));
    }

    #[test]
    fn first_container_port_none_when_no_ports() {
        assert_eq!(first_container_port(&empty_inspect()), None);
    }

    #[test]
    fn has_healthcheck_true_when_config_present() {
        let inspect = ContainerInspectResponse {
            config: Some(ContainerConfig {
                healthcheck: Some(HealthConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(has_healthcheck(&inspect));
    }

    #[test]
    fn has_healthcheck_true_when_state_health_present() {
        let inspect = ContainerInspectResponse {
            state: Some(ContainerState {
                health: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(has_healthcheck(&inspect));
    }

    #[test]
    fn has_healthcheck_false_when_neither_present() {
        assert!(!has_healthcheck(&empty_inspect()));
    }

    #[test]
    fn rw_volume_mounts_from_mounts_excludes_read_only() {
        let inspect = ContainerInspectResponse {
            host_config: Some(HostConfig {
                mounts: Some(vec![
                    Mount {
                        target: Some("/data".into()),
                        read_only: Some(false),
                        ..Default::default()
                    },
                    Mount {
                        target: Some("/etc/conf".into()),
                        read_only: Some(true),
                        ..Default::default()
                    },
                    Mount {
                        target: Some("/var/lib/x".into()),
                        read_only: None,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut got = rw_volume_mounts(&inspect);
        got.sort();
        assert_eq!(got, vec!["/data", "/var/lib/x"]);
    }

    #[test]
    fn rw_volume_mounts_from_binds_when_no_mounts() {
        let inspect = ContainerInspectResponse {
            host_config: Some(HostConfig {
                mounts: None,
                binds: Some(vec![
                    "/host/data:/data".into(),
                    "/host/conf:/etc/conf:ro".into(),
                    "/host/cache:/cache:rw".into(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut got = rw_volume_mounts(&inspect);
        got.sort();
        assert_eq!(got, vec!["/cache", "/data"]);
    }

    #[test]
    fn rw_volume_mounts_empty_when_nothing_set() {
        assert!(rw_volume_mounts(&empty_inspect()).is_empty());
    }

    #[test]
    fn label_strategy_reads_isengard_label() {
        let mut labels = HashMap::new();
        labels.insert("isengard.deploy.strategy".into(), "blue_green".into());
        let inspect = ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(labels),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(label_strategy(&inspect).as_deref(), Some("blue_green"));
    }

    #[test]
    fn label_strategy_none_when_absent() {
        assert!(label_strategy(&empty_inspect()).is_none());
    }
}
