//! Self-update via rename-then-replace.
//!
//! When the agent's own container is `needs_update`, naive stop+remove kills
//! the orchestrator mid-update. The fix: rename the old container out of the
//! way, create the replacement under the original name, start it, then exit
//! cleanly. Docker's restart policies won't restart a container that exited 0.
//!
//! On startup, `cleanup_replaced_siblings` removes any leftover `-replaced-*`
//! containers — that's how the new agent (us, after a self-update) cleans up
//! after the old one.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::container::{
    CreateContainerOptions, ListContainersOptions, RemoveContainerOptions, RenameContainerOptions,
};
use bollard::image::RemoveImageOptions;
use bollard::network::ConnectNetworkOptions;
use tracing::{info, warn};

use crate::image_ref::ImageRef;
use crate::recreate::{capture_config, pull_image};

/// Self-update orchestration. Inspect → pull → rename → create → start →
/// reconnect networks → schedule clean exit.
///
/// On success, this function returns `Ok(())` AND schedules the current
/// process to exit ~200ms later (giving log flush a moment). The caller
/// should not perform further work after this returns Ok.
pub async fn update_self(
    docker: &Docker,
    self_id: &str,
    new_image_ref: &ImageRef,
) -> anyhow::Result<()> {
    let inspect = docker
        .inspect_container(self_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("inspect self {self_id}: {e}"))?;

    let old_image_id = inspect.image.clone();

    let original_name = inspect
        .name
        .as_deref()
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_default();
    if original_name.is_empty() {
        return Err(anyhow::anyhow!("self container has no name"));
    }

    pull_image(docker, new_image_ref).await?;

    let new_image_str = format!(
        "{}/{}:{}",
        new_image_ref.registry, new_image_ref.repository, new_image_ref.tag
    );
    let spec = capture_config(&inspect, &new_image_str);

    let renamed = renamed_self_name(&original_name);
    info!(
        from = %original_name,
        to = %renamed,
        "renaming self before replacement"
    );
    docker
        .rename_container(
            self_id,
            RenameContainerOptions {
                name: renamed.clone(),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("rename self {self_id}: {e}"))?;

    info!(name = %original_name, image = %spec.image, "creating replacement under original name");
    let create = docker
        .create_container(
            Some(CreateContainerOptions::<String> {
                name: original_name.clone(),
                platform: None,
            }),
            spec.config,
        )
        .await
        .map_err(|e| {
            // Best-effort rollback: rename ourselves back so the next cycle
            // can try again. If this fails too, we're in a stuck state but
            // still alive.
            let docker_c = docker.clone();
            let renamed_c = renamed.clone();
            let original_c = original_name.clone();
            let self_id_c = self_id.to_string();
            tokio::spawn(async move {
                let _ = docker_c
                    .rename_container(
                        &self_id_c,
                        RenameContainerOptions {
                            name: original_c.clone(),
                        },
                    )
                    .await
                    .map_err(
                        |e| warn!(error = %e, "failed to rename self back after create error"),
                    );
                let _ = renamed_c; // keep moved
            });
            anyhow::anyhow!("create replacement {original_name}: {e}")
        })?;

    info!(name = %original_name, new_id = %create.id, "starting replacement");
    docker
        .start_container::<String>(&create.id, None)
        .await
        .map_err(|e| anyhow::anyhow!("start replacement {original_name}: {e}"))?;

    for (net_name, settings) in &spec.networks {
        let opts = ConnectNetworkOptions {
            container: create.id.clone(),
            endpoint_config: settings.clone(),
        };
        if let Err(e) = docker.connect_network(net_name, opts).await {
            warn!(network = %net_name, error = %e, "network reattach failed for replacement");
        }
    }

    info!("self-update complete; exiting current process in 200ms");
    let docker_exit = docker.clone();
    tokio::spawn(async move {
        if let Some(old) = old_image_id {
            match docker_exit
                .remove_image(
                    &old,
                    Some(RemoveImageOptions {
                        force: false,
                        noprune: false,
                    }),
                    None,
                )
                .await
            {
                Ok(_) => info!(old_image = %old, "removed old self image"),
                Err(e) => info!(old_image = %old, error = %e, "old self image not removed"),
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    });

    Ok(())
}

/// Generate the name to rename our own container to before replacing it.
/// Uses a unix-second timestamp suffix so multiple replacements don't collide.
pub fn renamed_self_name(original: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{original}-replaced-{ts}")
}

/// Best-effort: find any container whose name starts with
/// `<my_name>-replaced-` and remove it. Called from updater `init`.
pub async fn cleanup_replaced_siblings(docker: &Docker, my_name: &str) {
    let prefix = format!("{my_name}-replaced-");

    let mut filters = HashMap::new();
    // bollard's filter expects a substring/prefix match for "name"
    filters.insert("name".to_string(), vec![prefix.clone()]);

    let opts = ListContainersOptions::<String> {
        all: true,
        filters,
        ..Default::default()
    };

    let containers = match docker.list_containers(Some(opts)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "list_containers failed during replaced-sibling cleanup");
            return;
        }
    };

    for c in containers {
        let Some(id) = c.id else { continue };
        // Bollard returns names with leading "/"; verify the prefix match
        // didn't false-positive on substring (it shouldn't, but defensive).
        let name_matches = c
            .names
            .as_ref()
            .map(|ns| {
                ns.iter()
                    .any(|n| n.trim_start_matches('/').starts_with(&prefix))
            })
            .unwrap_or(false);
        if !name_matches {
            continue;
        }

        info!(container = %id, "removing replaced sibling");
        if let Err(e) = docker
            .remove_container(
                &id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            )
            .await
        {
            warn!(container = %id, error = %e, "failed to remove replaced sibling");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_self_name_includes_original_and_replaced_marker() {
        let r = renamed_self_name("isengard-agent");
        assert!(r.starts_with("isengard-agent-replaced-"));
    }

    #[test]
    fn renamed_self_name_timestamp_is_numeric() {
        let r = renamed_self_name("foo");
        let suffix = r.strip_prefix("foo-replaced-").unwrap();
        assert!(suffix.chars().all(|c| c.is_ascii_digit()));
        assert!(!suffix.is_empty());
    }

    #[test]
    fn renamed_self_name_handles_dashes_in_original() {
        let r = renamed_self_name("my-app-agent");
        assert!(r.starts_with("my-app-agent-replaced-"));
    }
}
