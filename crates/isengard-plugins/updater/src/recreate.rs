//! Recreate a running container against a new image, preserving its config.
//!
//! Phase 3c does NOT handle self-update — see `self_id.rs`. Phase 3d adds
//! rename-first ordering for the agent's own container.

use std::collections::HashMap;

use bollard::container::Config;
use bollard::models::{ContainerInspectResponse, EndpointSettings, HostConfig, MountPoint};

/// All the data we need to faithfully recreate a container against a new image.
#[derive(Debug, Clone)]
pub struct RecreateSpec {
    /// Container name (without leading slash).
    pub name: String,
    /// New image to use (e.g. "ghcr.io/foo/bar:latest").
    pub image: String,
    /// Container-level config (env, labels, cmd, working dir, etc).
    pub config: Config<String>,
    /// Networks to reconnect AFTER start (excluding the default bridge that
    /// `start_container` connects automatically).
    pub networks: HashMap<String, EndpointSettings>,
}

/// Translate an inspect response into a RecreateSpec for `new_image`.
pub fn capture_config(inspect: &ContainerInspectResponse, new_image: &str) -> RecreateSpec {
    let name = inspect
        .name
        .as_deref()
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_default();

    let mut host_config = inspect.host_config.clone().unwrap_or_default();
    dedup_mounts(&mut host_config);
    let mounts_for_options = host_config.mounts.clone();

    let cfg_in = inspect.config.clone().unwrap_or_default();
    let config: Config<String> = Config {
        image: Some(new_image.to_string()),
        cmd: cfg_in.cmd,
        entrypoint: cfg_in.entrypoint,
        env: cfg_in.env,
        labels: cfg_in.labels,
        working_dir: cfg_in.working_dir,
        user: cfg_in.user,
        exposed_ports: cfg_in.exposed_ports,
        host_config: Some(host_config),
        ..Default::default()
    };

    let mut networks = HashMap::new();
    if let Some(ns) = inspect
        .network_settings
        .as_ref()
        .and_then(|s| s.networks.as_ref())
    {
        for (net_name, settings) in ns {
            if net_name == "bridge" {
                continue;
            }
            networks.insert(net_name.clone(), settings.clone());
        }
    }

    // Suppress unused-warning for the cloned-out mounts; bollard's Config
    // already carries them via host_config but some downstream code may want
    // them separately. Phase 3d revisits.
    let _ = mounts_for_options;

    RecreateSpec {
        name,
        image: new_image.to_string(),
        config,
        networks,
    }
}

/// When both `binds` and `mounts` target the same destination, drop the bind.
/// `mounts` is the newer API and may carry richer flags (read-only, propagation).
fn dedup_mounts(host_config: &mut HostConfig) {
    let Some(mounts) = host_config.mounts.as_ref() else {
        return;
    };
    let mount_targets: std::collections::HashSet<&String> =
        mounts.iter().filter_map(|m| m.target.as_ref()).collect();

    if let Some(binds) = host_config.binds.as_mut() {
        binds.retain(|b| {
            // Bind format: "<src>:<dst>" or "<src>:<dst>:<opts>"
            let dst = b.split(':').nth(1);
            match dst {
                Some(d) => !mount_targets.iter().any(|t| t.as_str() == d),
                None => true,
            }
        });
        if binds.is_empty() {
            host_config.binds = None;
        }
    }
}

// Suppress dead_code on the helper while pull_image / update_container land
// in the next task.
#[allow(dead_code)]
fn _unused_marker(_: &MountPoint) {}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerConfig, ContainerInspectResponse, EndpointSettings, HostConfig, Mount,
        NetworkSettings, RestartPolicy,
    };

    fn make_inspect() -> ContainerInspectResponse {
        let mut labels = HashMap::new();
        labels.insert("isengard.enable".into(), "true".into());
        labels.insert("traefik.enable".into(), "true".into());

        let cfg = ContainerConfig {
            image: Some("nginx:1.24".into()),
            env: Some(vec!["FOO=bar".into(), "BAZ=qux".into()]),
            labels: Some(labels),
            cmd: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
            working_dir: Some("/etc/nginx".into()),
            ..Default::default()
        };

        let host_cfg = HostConfig {
            restart_policy: Some(RestartPolicy {
                name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                ..Default::default()
            }),
            binds: Some(vec!["/host/data:/data".into()]),
            mounts: Some(vec![Mount {
                target: Some("/etc/nginx/conf.d".into()),
                source: Some("/host/conf".into()),
                typ: Some(bollard::models::MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut networks = HashMap::new();
        networks.insert("bridge".into(), EndpointSettings::default());
        networks.insert(
            "my-app-net".into(),
            EndpointSettings {
                aliases: Some(vec!["nginx-server".into()]),
                ..Default::default()
            },
        );

        ContainerInspectResponse {
            name: Some("/web".into()),
            image: Some("sha256:oldimagehash".into()),
            config: Some(cfg),
            host_config: Some(host_cfg),
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn name_strips_leading_slash() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(spec.name, "web");
    }

    #[test]
    fn image_replaced_with_new() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(spec.config.image.as_deref(), Some("nginx:1.25"));
    }

    #[test]
    fn env_preserved() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(
            spec.config.env.as_ref().unwrap(),
            &vec!["FOO=bar".to_string(), "BAZ=qux".to_string()]
        );
    }

    #[test]
    fn labels_preserved_including_third_party() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        let labels = spec.config.labels.as_ref().unwrap();
        assert_eq!(
            labels.get("isengard.enable").map(|s| s.as_str()),
            Some("true")
        );
        assert_eq!(
            labels.get("traefik.enable").map(|s| s.as_str()),
            Some("true")
        );
    }

    #[test]
    fn cmd_and_workdir_preserved() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert_eq!(spec.config.cmd.as_ref().unwrap().len(), 3);
        assert_eq!(spec.config.working_dir.as_deref(), Some("/etc/nginx"));
    }

    #[test]
    fn restart_policy_preserved() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        let policy = host_cfg.restart_policy.as_ref().unwrap();
        assert_eq!(
            policy.name,
            Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED)
        );
    }

    #[test]
    fn bridge_network_filtered_out() {
        let inspect = make_inspect();
        let spec = capture_config(&inspect, "nginx:1.25");
        assert!(!spec.networks.contains_key("bridge"));
        assert!(spec.networks.contains_key("my-app-net"));
    }

    #[test]
    fn mounts_kept_when_only_mounts_set() {
        let mut inspect = make_inspect();
        if let Some(hc) = inspect.host_config.as_mut() {
            hc.binds = None;
        }
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        assert_eq!(host_cfg.mounts.as_ref().unwrap().len(), 1);
        assert!(host_cfg.binds.is_none());
    }

    #[test]
    fn binds_kept_when_only_binds_set() {
        let mut inspect = make_inspect();
        if let Some(hc) = inspect.host_config.as_mut() {
            hc.mounts = None;
        }
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        assert_eq!(
            host_cfg.binds.as_ref().unwrap(),
            &vec!["/host/data:/data".to_string()]
        );
    }

    #[test]
    fn bind_dropped_when_mount_targets_same_destination() {
        // Add a bind whose destination matches an existing mount target.
        let mut inspect = make_inspect();
        if let Some(hc) = inspect.host_config.as_mut() {
            hc.binds = Some(vec!["/elsewhere:/etc/nginx/conf.d".into()]);
            // mounts already targets /etc/nginx/conf.d
        }
        let spec = capture_config(&inspect, "nginx:1.25");
        let host_cfg = spec.config.host_config.as_ref().unwrap();
        // bind is dropped
        assert!(host_cfg.binds.is_none());
        // mount survives
        assert_eq!(host_cfg.mounts.as_ref().unwrap().len(), 1);
    }
}
