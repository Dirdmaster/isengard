//! `isd uninit`: tear down the cluster created by `isd init`.
//!
//! Stops + removes isd-controller and isd-agent. Preserves the
//! isd-controller-state, isd-agent-state, isd-stacks docker volumes by
//! default so a subsequent `isd init` (idempotent) or `isd restore <backup>`
//! brings the cluster back with all data intact. Pass `--wipe-state` to
//! also delete the volumes: UNRECOVERABLE without a prior `isd backup`.
//!
//! This is the deliberate teardown path for the system containers
//! protected by the lifecycle guard. The guard is not invoked
//! here: `uninit` calls `docker remove_container` directly via bollard,
//! the same override the `--force-system` flag exposes on `isd rm` /
//! `isd stop` / etc.

use anyhow::{Context, Result};
use bollard::container::RemoveContainerOptions;
use clap::Args;

/// CLI flags for `isd uninit`.
#[derive(Debug, Args)]
pub struct UninitArgs {
    /// Skip the y/N prompt.
    #[arg(long)]
    pub yes: bool,
    /// Also remove the cluster's state volumes (UNRECOVERABLE).
    ///
    /// Wipes `isd-controller-state`, `isd-agent-state`, `isd-stacks`.
    /// UNRECOVERABLE without a prior `isd backup`.
    #[arg(long)]
    pub wipe_state: bool,
    /// Take an encrypted backup before tearing down.
    #[arg(long)]
    pub backup_first: bool,
}

/// Named volumes the cluster's state lives in. Preserved by default;
/// removed only when `--wipe-state` is set.
const VOLUMES: &[&str] = &["isd-controller-state", "isd-agent-state", "isd-stacks"];

/// System containers the cluster runs. Removed unconditionally as part
/// of teardown.
const CONTAINERS: &[&str] = &["isd-controller", "isd-agent"];

/// Tear down the cluster on the resolved docker context.
///
/// Optionally takes an encrypted backup first (`--backup-first`),
/// prompts the operator (skipped with `--yes`), then removes the
/// isd-controller and isd-agent containers. Preserves the named state
/// volumes by default; `--wipe-state` deletes them too.
///
/// Calls `remove_container` directly via bollard, bypassing the
/// lifecycle guard that normally protects `io.isengard.role=controller`
/// / `=agent` containers. This is the deliberate teardown path.
///
/// # Errors
///
/// Returns `Err` when the docker context cannot be resolved, the
/// confirmation prompt cannot read stdin, or (with `--backup-first`)
/// the pre-teardown backup fails.
pub async fn run(args: UninitArgs, context: Option<&str>) -> Result<()> {
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;

    if args.backup_first {
        eprintln!("isd uninit: taking backup before teardown...");
        crate::backup_cmd::run(crate::backup_cmd::BackupArgs::default_for_uninit(), context)
            .await?;
    }

    if !args.yes {
        eprintln!("isd uninit: will stop + remove isd-controller and isd-agent on {docker_uri}.");
        if args.wipe_state {
            eprintln!(
                "isd uninit: --wipe-state WILL DELETE volumes: {}",
                VOLUMES.join(", ")
            );
            eprintln!("isd uninit: this is UNRECOVERABLE without a prior `isd backup`.");
        } else {
            eprintln!("isd uninit: volumes will be PRESERVED.");
        }
        eprint!("Continue? [y/N]: ");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("reading confirm")?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            eprintln!("isd uninit: aborted.");
            return Ok(());
        }
    }

    let docker = isd_runtime::DockerBackend::from_uri(&docker_uri).await?;
    for name in CONTAINERS {
        let _ = docker
            .client()
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await; // ignore not-found
        eprintln!("isd uninit: removed {name}");
    }

    if args.wipe_state {
        for vol in VOLUMES {
            let _ = docker.client().remove_volume(vol, None).await;
            eprintln!("isd uninit: removed volume {vol}");
        }
    }

    println!("Cluster torn down on {docker_uri}.");
    if args.wipe_state {
        println!("State volumes wiped. Restart from scratch: `isd init`.");
    } else {
        println!("State preserved. Restore with `isd restore <backup>` or `isd init`.");
    }
    Ok(())
}
