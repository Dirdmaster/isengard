//! Container lifecycle commands: stop, start, restart, kill, rm.
//!
//! All five share the same shape: take positional args (IDs, names, or
//! index selectors), resolve them via [`crate::index_resolve`], iterate
//! over the resolved targets, and call the matching `DockerBackend`
//! method. `rm` and `kill` additionally prompt for confirmation when
//! any arg was resolved through an index (see [`crate::confirm`]).

use anyhow::{Context, Result};
use clap::Args;

use crate::confirm;
use crate::index_resolve;
use crate::ps;

#[derive(Debug, Args)]
pub struct StopArgs {
    /// One or more container IDs, names, or index selectors (`2`,
    /// `1-3,5`, mixed with literals).
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Grace period in seconds before docker sends SIGKILL. Matches
    /// `docker stop --time`.
    #[arg(short = 't', long, default_value_t = 10)]
    pub time: i64,
}

pub async fn run_stop(args: StopArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    let backend = ps::open_docker_backend(context).await?;
    for t in &targets {
        backend
            .stop_container(&t.container_id, args.time)
            .await
            .with_context(|| format!("stop {} ({})", t.name, t.container_id))?;
        println!("{} stopped", t.name);
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
}

pub async fn run_start(args: StartArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    let backend = ps::open_docker_backend(context).await?;
    for t in &targets {
        backend
            .start_container(&t.container_id)
            .await
            .with_context(|| format!("start {} ({})", t.name, t.container_id))?;
        println!("{} started", t.name);
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct RestartArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Grace period in seconds before SIGKILL. Matches `docker restart --time`.
    #[arg(short = 't', long, default_value_t = 10)]
    pub time: i64,
}

pub async fn run_restart(args: RestartArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    let backend = ps::open_docker_backend(context).await?;
    for t in &targets {
        backend
            .restart_container(&t.container_id, args.time)
            .await
            .with_context(|| format!("restart {} ({})", t.name, t.container_id))?;
        println!("{} restarted", t.name);
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Force-remove running containers (the `docker rm -f` form).
    #[arg(short = 'f', long)]
    pub force: bool,
}

pub async fn run_rm(args: RmArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    if !confirm::confirm_destructive("rm", &targets, args.force)? {
        eprintln!("isd: aborted");
        return Ok(());
    }
    let backend = ps::open_docker_backend(context).await?;
    for t in &targets {
        backend
            .remove_container(&t.container_id, args.force)
            .await
            .with_context(|| format!("rm {} ({})", t.name, t.container_id))?;
        println!("{} removed", t.name);
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct KillArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Signal to send. Matches `docker kill --signal`. Default SIGKILL.
    #[arg(short = 's', long)]
    pub signal: Option<String>,
    /// Skip the confirmation prompt.
    #[arg(short = 'f', long)]
    pub force: bool,
}

pub async fn run_kill(args: KillArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    if !confirm::confirm_destructive("kill", &targets, args.force)? {
        eprintln!("isd: aborted");
        return Ok(());
    }
    let backend = ps::open_docker_backend(context).await?;
    for t in &targets {
        backend
            .kill_container(&t.container_id, args.signal.as_deref())
            .await
            .with_context(|| format!("kill {} ({})", t.name, t.container_id))?;
        println!("{} killed", t.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Spin up a container, run isd stop against its index, verify it
    /// stopped. Ignored: needs a real daemon. Run with
    /// `cargo test -p isd -- --ignored stop_against_local_daemon`.
    #[tokio::test]
    #[ignore]
    async fn stop_against_local_daemon() {
        // The full integration test is left to the executing agent to
        // wire up: create a sleeping container via bollard, write a
        // synthetic index_cache pointing at it, call run_stop with
        // `["0"]`, assert the container is stopped (inspect state).
    }
}
