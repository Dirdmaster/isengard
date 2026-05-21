//! Container lifecycle commands: stop, start, restart, kill, rm.
//!
//! All five share the same shape: take positional args (IDs, names, or
//! index selectors), resolve them via [`crate::index_resolve`], iterate
//! over the resolved targets, and call the matching `DockerBackend`
//! method. `rm` and `kill` additionally prompt for confirmation when
//! any arg was resolved through an index (see [`crate::confirm`]).
//!
//! Adds a pre-execute protection guard: any resolved target
//! whose `io.isengard.role` label is in
//! [`isd_runtime::discovery_labels::ROLE_VALUES_PROTECTED`] is refused
//! unless `--force-system` is passed. The guard inspects each target's
//! labels via `DockerBackend::inspect_labels` before any docker
//! mutation runs, so a partial failure can never leave the operator
//! looking at a half-destroyed cluster.

use anyhow::{Context, Result};
use clap::Args;
use isd_runtime::discovery_labels::{ROLE_LABEL, is_protected_label_value};

use crate::confirm;
use crate::index_resolve::{self, ResolvedTarget};
use crate::ps;

/// Pure protection check for one (name, role) pair. Returns `Ok(())`
/// when `force_system` is true, when the role is `None`, or when the
/// role is not in [`isd_runtime::discovery_labels::ROLE_VALUES_PROTECTED`].
/// Otherwise returns an actionable `anyhow` error pointing at the two
/// override paths: `isd uninit` (deliberate teardown) and
/// `isd <verb> --force-system <name>` (one-off escape hatch).
fn check_one_target_protection(
    name: &str,
    role: Option<&str>,
    force_system: bool,
    verb: &str,
) -> Result<()> {
    if force_system {
        return Ok(());
    }
    if let Some(role) = role
        && is_protected_label_value(role)
    {
        anyhow::bail!(
            "refused: {name} is protected (io.isengard.role={role}). \
             Use `isd uninit` to tear down the cluster, or \
             `isd {verb} --force-system {name}` to override."
        );
    }
    Ok(())
}

/// Live protection check for the resolved-target slice. Fetches each
/// target's labels via the backend's `inspect_labels`, then delegates
/// to [`check_one_target_protection`]. Bails on the first protected
/// target so no docker mutation runs.
async fn check_protection(
    targets: &[ResolvedTarget],
    backend: &isd_runtime::DockerBackend,
    force_system: bool,
    verb: &str,
) -> Result<()> {
    if force_system {
        return Ok(());
    }
    for t in targets {
        let labels = backend
            .inspect_labels(&t.container_id)
            .await
            .with_context(|| format!("inspecting {} ({})", t.name, t.container_id))?;
        let role = labels.get(ROLE_LABEL).map(String::as_str);
        check_one_target_protection(&t.name, role, force_system, verb)?;
    }
    Ok(())
}

/// CLI flags for `isd stop`.
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
    /// Override the system-container protection (refuses on
    /// io.isengard.role=controller|agent without this flag).
    #[arg(long)]
    pub force_system: bool,
}

/// Stop one or more containers. Resolves args, checks the protection
/// guard, then iterates [`isd_runtime::DockerBackend::stop_container`].
///
/// # Errors
///
/// Returns `Err` on any resolution, protection, or docker failure.
pub async fn run_stop(args: StopArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    let backend = ps::open_docker_backend(context).await?;
    check_protection(&targets, &backend, args.force_system, "stop").await?;
    for t in &targets {
        backend
            .stop_container(&t.container_id, args.time)
            .await
            .with_context(|| format!("stop {} ({})", t.name, t.container_id))?;
        println!("{} stopped", t.name);
    }
    Ok(())
}

/// CLI flags for `isd start`.
#[derive(Debug, Args)]
pub struct StartArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
}

/// Start one or more containers. No protection guard: starting an
/// already-running container is a no-op, and starting an isd-controller
/// the operator deliberately stopped is intentional.
///
/// # Errors
///
/// Returns `Err` on any resolution or docker failure.
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

/// CLI flags for `isd restart`.
#[derive(Debug, Args)]
pub struct RestartArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Grace period in seconds before SIGKILL. Matches `docker restart --time`.
    #[arg(short = 't', long, default_value_t = 10)]
    pub time: i64,
    /// Override the system-container protection (refuses on
    /// io.isengard.role=controller|agent without this flag).
    #[arg(long)]
    pub force_system: bool,
}

/// Restart one or more containers (stop + start under a guard).
///
/// # Errors
///
/// Returns `Err` on any resolution, protection, or docker failure.
pub async fn run_restart(args: RestartArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    let backend = ps::open_docker_backend(context).await?;
    check_protection(&targets, &backend, args.force_system, "restart").await?;
    for t in &targets {
        backend
            .restart_container(&t.container_id, args.time)
            .await
            .with_context(|| format!("restart {} ({})", t.name, t.container_id))?;
        println!("{} restarted", t.name);
    }
    Ok(())
}

/// CLI flags for `isd rm`.
#[derive(Debug, Args)]
pub struct RmArgs {
    /// One or more container IDs, names, or index selectors.
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Force-remove running containers (the `docker rm -f` form).
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Override the system-container protection (refuses on
    /// io.isengard.role=controller|agent without this flag). Different
    /// from `-f` / `--force` which only force-removes a running
    /// container; this flag bypasses the role-label guard.
    #[arg(long)]
    pub force_system: bool,
}

/// Remove one or more containers, prompting before any
/// index-resolved deletion.
///
/// # Errors
///
/// Returns `Err` on any resolution, protection, or docker failure.
/// Operator aborting at the confirm prompt returns `Ok` (no-op).
pub async fn run_rm(args: RmArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    if !confirm::confirm_destructive("rm", &targets, args.force)? {
        eprintln!("isd: aborted");
        return Ok(());
    }
    let backend = ps::open_docker_backend(context).await?;
    check_protection(&targets, &backend, args.force_system, "rm").await?;
    for t in &targets {
        backend
            .remove_container(&t.container_id, args.force)
            .await
            .with_context(|| format!("rm {} ({})", t.name, t.container_id))?;
        println!("{} removed", t.name);
    }
    Ok(())
}

/// CLI flags for `isd kill`.
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
    /// Override the system-container protection (refuses on
    /// io.isengard.role=controller|agent without this flag). Different
    /// from `-f` / `--force` which only skips the confirm prompt; this
    /// flag bypasses the role-label guard.
    #[arg(long)]
    pub force_system: bool,
}

/// Send a signal to one or more containers, prompting before any
/// index-resolved kill.
///
/// # Errors
///
/// Returns `Err` on any resolution, protection, or docker failure.
/// Operator aborting at the confirm prompt returns `Ok` (no-op).
pub async fn run_kill(args: KillArgs, context: Option<&str>) -> Result<()> {
    let targets = index_resolve::resolve(&args.targets)?;
    if !confirm::confirm_destructive("kill", &targets, args.force)? {
        eprintln!("isd: aborted");
        return Ok(());
    }
    let backend = ps::open_docker_backend(context).await?;
    check_protection(&targets, &backend, args.force_system, "kill").await?;
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
    use super::*;

    /// `isd rm <protected>` without `--force-system` refuses
    /// with an actionable error naming both override paths.
    #[test]
    fn rm_refuses_protected_container_without_force_system() {
        let err = check_one_target_protection("isd-controller", Some("controller"), false, "rm")
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("refused"), "got: {msg}");
        assert!(msg.contains("isd-controller"), "got: {msg}");
        assert!(msg.contains("io.isengard.role=controller"), "got: {msg}");
        assert!(msg.contains("isd uninit"), "got: {msg}");
        assert!(
            msg.contains("isd rm --force-system isd-controller"),
            "got: {msg}"
        );
    }

    /// `--force-system` bypasses the guard so deliberate
    /// teardown (`isd uninit` internals) can target system containers.
    #[test]
    fn rm_allows_protected_with_force_system() {
        check_one_target_protection("isd-controller", Some("controller"), true, "rm")
            .expect("force_system bypasses the guard");
    }

    /// The same guard is wired into `stop`. Error mentions the
    /// `stop` verb so the override hint copy-pastes cleanly.
    #[test]
    fn stop_refuses_protected_container_without_force_system() {
        let err =
            check_one_target_protection("isd-agent", Some("agent"), false, "stop").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("refused"), "got: {msg}");
        assert!(msg.contains("io.isengard.role=agent"), "got: {msg}");
        assert!(
            msg.contains("isd stop --force-system isd-agent"),
            "got: {msg}"
        );
    }

    /// Same guard for `restart`. Verb threads through.
    #[test]
    fn restart_refuses_protected_container_without_force_system() {
        let err =
            check_one_target_protection("isd-controller", Some("controller"), false, "restart")
                .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("isd restart --force-system isd-controller"),
            "got: {msg}"
        );
    }

    /// Same guard for `kill`. Verb threads through.
    #[test]
    fn kill_refuses_protected_container_without_force_system() {
        let err =
            check_one_target_protection("isd-agent", Some("agent"), false, "kill").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("isd kill --force-system isd-agent"),
            "got: {msg}"
        );
    }

    /// Containers without `io.isengard.role` are unaffected.
    #[test]
    fn unlabelled_container_is_not_protected() {
        check_one_target_protection("bazarr", None, false, "rm")
            .expect("unlabelled containers pass through");
    }

    /// A role outside the protected set (e.g. a hypothetical
    /// `registry` role) is not protected. Mirrors
    /// `discovery_labels::tests::other_roles_are_not_protected`.
    #[test]
    fn non_protected_role_is_not_blocked() {
        check_one_target_protection("iso-registry", Some("registry"), false, "rm")
            .expect("only controller/agent are protected");
    }

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
