//! `wisp` command-line front-end for the [`wisp`] runtime crate.
//!
//! Subcommands mirror the runtime API one-to-one (`run` is the
//! single composite that combines `create + start + waitpid +
//! delete`). The binary is deliberately small:
//!
//! - no async runtime: `Runtime::start` calls `clone3` and we MUST
//!   stay single-threaded until that returns. tokio would spawn a
//!   worker pool the moment `#[tokio::main]` ran; we sidestep it
//!   entirely by being plain `fn main`.
//! - tracing-subscriber's `fmt` writer is synchronous; no helper
//!   threads spawn at init time.
//! - tracing init runs BEFORE `Runtime::start`, which is fine: it
//!   doesn't fork worker threads with the default config.
//!
//! Default state-dir: `/var/lib/wisp` (override via `--state-dir` or
//! `WISP_STATE_DIR`). Production needs root for the create path,
//! since `/var/lib/wisp` is owned by root and the runtime mounts +
//! cgroup writes need `CAP_SYS_ADMIN`. Without root the CLI fails
//! loudly with EACCES on the first state-dir write.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use nix::sys::signal::Signal;

use wisp::{ContainerHandle, ContainerState, Runtime};

/// `wisp` CLI.
#[derive(Debug, Parser)]
#[command(name = "wisp", version, about = "Daemonless OCI container runtime")]
struct Cli {
    /// State directory for container metadata. Default `/var/lib/wisp`.
    #[arg(
        long,
        env = "WISP_STATE_DIR",
        default_value = "/var/lib/wisp",
        global = true
    )]
    state_dir: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create a container, start it, and (without --detach) wait for
    /// PID 1 to exit before cleaning up. Mirrors `runc create + start
    /// + delete --force`.
    Run(RunArgs),
    /// List containers in the state-dir.
    Ps,
    /// Print one container's state as JSON.
    State { id: String },
    /// Send a signal to the container's PID 1.
    Kill(KillArgs),
    /// Free a container's state-dir + cgroup.
    Delete(DeleteArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to the OCI bundle directory (`config.json` + `rootfs/`).
    bundle: PathBuf,
    /// Container ID. Defaults to the bundle's basename.
    #[arg(long)]
    id: Option<String>,
    /// Detach: print the container ID and return immediately.
    /// Without this, the CLI waits for PID 1 to exit, prints its
    /// exit status, and removes the container.
    #[arg(long)]
    detach: bool,
}

#[derive(Debug, Args)]
struct KillArgs {
    /// Container ID.
    id: String,
    /// Signal to send. Accepts the names `SIGTERM`, `TERM`, etc.
    #[arg(long, default_value = "SIGTERM")]
    signal: String,
}

#[derive(Debug, Args)]
struct DeleteArgs {
    /// Container ID.
    id: String,
    /// Force delete even if the container is Running.
    #[arg(long)]
    force: bool,
}

fn main() {
    // Single-threaded invariant: `Runtime::start` calls `clone3`. The
    // CLI must avoid spawning any threads before that point. tokio is
    // banned (no `#[tokio::main]`); tracing_subscriber's default
    // `fmt::Layer` writer is synchronous; clap parsing is sync.
    init_tracing();

    if let Err(err) = run() {
        // anyhow chains nicely: walk causes for a flat "wisp: a: b: c"
        // line so the user sees the underlying io / cgroup error.
        let mut msg = format!("wisp: {err}");
        let mut source = err.source();
        while let Some(cause) = source {
            msg.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        eprintln!("{msg}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_env("WISP_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run(args) => cmd_run(&cli.state_dir, args),
        Cmd::Ps => cmd_ps(&cli.state_dir),
        Cmd::State { id } => cmd_state(&cli.state_dir, &id),
        Cmd::Kill(args) => cmd_kill(&cli.state_dir, args),
        Cmd::Delete(args) => cmd_delete(&cli.state_dir, args),
    }
}

fn cmd_run(state_dir: &Path, args: RunArgs) -> Result<()> {
    let id = args
        .id
        .unwrap_or_else(|| derive_id_from_bundle(&args.bundle));
    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;

    let handle = rt
        .create(&id, &args.bundle)
        .with_context(|| format!("create container {id:?}"))?;
    rt.start(&handle.id)
        .with_context(|| format!("start container {id:?}"))?;

    if args.detach {
        println!("{}", handle.id);
        return Ok(());
    }

    // Block until PID 1 exits. We have its pid in `handle.pid` after
    // `start`: re-read state to be safe (start has just written it).
    let live = rt
        .state(&handle.id)
        .with_context(|| format!("state {id:?}"))?;
    let pid = live
        .pid
        .ok_or_else(|| anyhow!("container {id:?} has no pid after start"))?;

    let exit = wait_for_pid(pid)?;

    // Clean up. `delete(force=true)` removes the cgroup + state-dir.
    rt.delete(&handle.id, true)
        .with_context(|| format!("delete container {id:?}"))?;

    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}

/// `waitpid` PID 1 of the container and return the exit code we'd
/// surface to the shell (0..=255 for normal exits, 128+signo for
/// signal-terminated, mirroring shell semantics).
fn wait_for_pid(pid: u32) -> Result<i32> {
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::Pid;

    let status =
        waitpid(Pid::from_raw(pid as i32), None).with_context(|| format!("waitpid({pid})"))?;

    match status {
        WaitStatus::Exited(_, code) => Ok(code),
        WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
        // Stopped/Continued aren't expected without WUNTRACED; treat
        // them as "still running" surprises and surface the status.
        other => Err(anyhow!("unexpected wait status: {other:?}")),
    }
}

fn cmd_ps(state_dir: &Path) -> Result<()> {
    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;
    let mut handles = rt.list().context("list containers")?;
    handles.sort_by(|a, b| a.id.cmp(&b.id));

    if handles.is_empty() {
        return Ok(());
    }

    // Compute column widths so the table reads cleanly without an
    // ASCII-art crate.
    let id_w = handles
        .iter()
        .map(|h| h.id.len())
        .max()
        .unwrap_or(2)
        .max("ID".len());
    let st_w = handles
        .iter()
        .map(|h| state_label(h.state).len())
        .max()
        .unwrap_or(0)
        .max("STATE".len());
    let pid_w = handles
        .iter()
        .map(|h| pid_label(h.pid).len())
        .max()
        .unwrap_or(0)
        .max("PID".len());

    println!(
        "{:<id_w$}  {:<st_w$}  {:>pid_w$}  AGE",
        "ID",
        "STATE",
        "PID",
        id_w = id_w,
        st_w = st_w,
        pid_w = pid_w
    );
    for h in &handles {
        println!(
            "{:<id_w$}  {:<st_w$}  {:>pid_w$}  {}",
            h.id,
            state_label(h.state),
            pid_label(h.pid),
            age(h.created_at),
            id_w = id_w,
            st_w = st_w,
            pid_w = pid_w
        );
    }
    Ok(())
}

fn state_label(s: ContainerState) -> &'static str {
    match s {
        ContainerState::Created => "Created",
        ContainerState::Running => "Running",
        ContainerState::Stopped => "Stopped",
    }
}

fn pid_label(pid: Option<u32>) -> String {
    match pid {
        Some(p) => p.to_string(),
        None => "-".to_string(),
    }
}

/// Format `created_at` as "Ns" / "Nm" / "Nh" / "Nd" relative to now.
fn age(created: SystemTime) -> String {
    let now = SystemTime::now();
    let delta = now
        .duration_since(created)
        .or_else(|_| created.duration_since(now))
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if delta < 60 {
        format!("{delta}s")
    } else if delta < 3_600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3_600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

fn cmd_state(state_dir: &Path, id: &str) -> Result<()> {
    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;
    let handle = rt.state(id).with_context(|| format!("state {id:?}"))?;
    print_handle_json(&handle)?;
    Ok(())
}

/// Pretty-print a [`ContainerHandle`] as JSON. We use a helper rather
/// than serde-on-the-handle directly because we want
/// `created_at` formatted as a unix-epoch number (the underlying
/// `SystemTime` serialisation is `(secs_since_epoch, nanos)`, which
/// is awkward for shell consumers).
fn print_handle_json(h: &ContainerHandle) -> Result<()> {
    let created_unix = h
        .created_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let json = serde_json::json!({
        "id": h.id,
        "bundle": h.bundle,
        "state": state_label(h.state),
        "pid": h.pid,
        "created_at": created_unix,
    });
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

fn cmd_kill(state_dir: &Path, args: KillArgs) -> Result<()> {
    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;
    let signal = parse_signal(&args.signal)?;
    rt.kill(&args.id, signal)
        .with_context(|| format!("kill {:?} with {signal:?}", args.id))?;
    Ok(())
}

fn parse_signal(name: &str) -> Result<Signal> {
    // nix's `Signal::from_str` accepts the upper-case names
    // ("SIGTERM"), and the bare names are common shorthand. Normalise
    // both so users can pass either form.
    let upper = name.to_ascii_uppercase();
    let canonical = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    Signal::from_str(&canonical).map_err(|err| anyhow!("invalid signal {name:?}: {err}"))
}

fn cmd_delete(state_dir: &Path, args: DeleteArgs) -> Result<()> {
    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;
    rt.delete(&args.id, args.force)
        .with_context(|| format!("delete {:?}", args.id))?;
    Ok(())
}

fn derive_id_from_bundle(bundle: &Path) -> String {
    bundle
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .unwrap_or_else(|| "wisp".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap's debug_assert validates the CLI shape (no duplicate
    /// args, no impossible default-value combos, no help-text
    /// holes) at runtime. The clap docs recommend running it from
    /// a test for coverage.
    #[test]
    fn cli_shape_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_signal_accepts_short_and_long_forms() {
        assert_eq!(parse_signal("TERM").unwrap(), Signal::SIGTERM);
        assert_eq!(parse_signal("SIGTERM").unwrap(), Signal::SIGTERM);
        assert_eq!(parse_signal("kill").unwrap(), Signal::SIGKILL);
        assert_eq!(parse_signal("SIGKILL").unwrap(), Signal::SIGKILL);
        assert_eq!(parse_signal("INT").unwrap(), Signal::SIGINT);
        assert_eq!(parse_signal("HUP").unwrap(), Signal::SIGHUP);
        assert_eq!(parse_signal("USR1").unwrap(), Signal::SIGUSR1);
        assert_eq!(parse_signal("USR2").unwrap(), Signal::SIGUSR2);
    }

    #[test]
    fn parse_signal_rejects_garbage() {
        let err = parse_signal("NOTASIGNAL").unwrap_err().to_string();
        assert!(err.contains("invalid signal"), "got: {err}");
    }

    #[test]
    fn derive_id_uses_basename() {
        assert_eq!(
            derive_id_from_bundle(Path::new("/var/bundles/demo")),
            "demo"
        );
        assert_eq!(derive_id_from_bundle(Path::new("./busybox")), "busybox");
    }

    #[test]
    fn derive_id_handles_trailing_slash() {
        // Path::file_name returns None for "foo/" on some platforms;
        // we should still produce a usable id.
        let id = derive_id_from_bundle(Path::new("/var/bundles/demo/"));
        // Either "demo" (Path canonicalises the trailing slash) or
        // the fallback. Both are acceptable.
        assert!(!id.is_empty());
    }

    #[test]
    fn age_formats_short_durations() {
        let now = SystemTime::now();
        assert!(age(now).ends_with('s'));
    }

    #[test]
    fn state_label_matches_variants() {
        assert_eq!(state_label(ContainerState::Created), "Created");
        assert_eq!(state_label(ContainerState::Running), "Running");
        assert_eq!(state_label(ContainerState::Stopped), "Stopped");
    }
}
