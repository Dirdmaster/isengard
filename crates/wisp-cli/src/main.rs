//! `wisp` command-line front-end for the [`wisp`] runtime crate.
//!
//! Subcommands mirror the runtime API one-to-one (`run` is the
//! single composite that combines `create + start + waitpid +
//! delete`), plus an `image` subcommand group for managing the
//! local OCI image cache (Phase 0.2). The binary is deliberately small:
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
use wisp_image::{BundleBuilder, Client, ConfigOverrides, ImageRef, PulledImage};

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
    /// Manage cached OCI images.
    Image(ImageArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to the OCI bundle directory (`config.json` + `rootfs/`).
    /// Mutually exclusive with `--image`.
    bundle: Option<PathBuf>,
    /// Pull and assemble a bundle from this image ref. Mutually
    /// exclusive with the positional bundle arg.
    #[arg(long, conflicts_with = "bundle")]
    image: Option<String>,
    /// Container ID. If omitted, derived from the bundle dir basename
    /// (positional bundle) or from the image ref's repo + tag (--image).
    #[arg(long)]
    id: Option<String>,
    /// Detach: print the container ID and return immediately.
    /// Without this, the CLI waits for PID 1 to exit, prints its
    /// exit status, and removes the container.
    #[arg(long)]
    detach: bool,
    /// With `--image`: extra args appended to the image's entrypoint.
    /// Ignored when running a positional bundle.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
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

#[derive(Debug, Args)]
struct ImageArgs {
    #[command(subcommand)]
    cmd: ImageCmd,
}

#[derive(Debug, Subcommand)]
enum ImageCmd {
    /// Pull an image from a registry (anonymous; public registries only).
    Pull {
        /// Image reference (e.g. `alpine:3.19`, `ghcr.io/foo/bar:tag`,
        /// or `<repo>@sha256:<hex>`).
        reference: String,
    },
    /// List cached images.
    List,
    /// Remove a cached image's tag pointer. Layer blobs are not
    /// directly deleted; they go away on the next `gc` if no other
    /// image / bundle still references them.
    Rm {
        /// Image reference. Currently must be tag-based; digest-only
        /// removal is deferred (the registry never wrote a tag pointer).
        reference: String,
    },
    /// Run garbage collection over the layer store.
    Gc,
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
        Cmd::Image(args) => cmd_image(&cli.state_dir, args),
    }
}

fn cmd_run(state_dir: &Path, args: RunArgs) -> Result<()> {
    match (&args.bundle, &args.image) {
        (Some(_), None) => cmd_run_bundle(state_dir, args),
        (None, Some(_)) => cmd_run_image(state_dir, args),
        (Some(_), Some(_)) => {
            // clap's `conflicts_with` should already block this; defensive
            // guard in case the attribute is ever weakened.
            Err(anyhow!(
                "--image and a positional bundle are mutually exclusive"
            ))
        }
        (None, None) => Err(anyhow!(
            "either a positional bundle path or `--image <ref>` is required"
        )),
    }
}

fn cmd_run_bundle(state_dir: &Path, args: RunArgs) -> Result<()> {
    let bundle = args
        .bundle
        .as_ref()
        .expect("cmd_run_bundle invariant: bundle is Some");
    let id = args
        .id
        .clone()
        .unwrap_or_else(|| derive_id_from_bundle(bundle));
    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;

    let handle = rt
        .create(&id, bundle)
        .with_context(|| format!("create container {id:?}"))?;
    rt.start(&handle.id)
        .with_context(|| format!("start container {id:?}"))?;

    if args.detach {
        println!("{}", handle.id);
        return Ok(());
    }

    let live = rt
        .state(&handle.id)
        .with_context(|| format!("state {id:?}"))?;
    let pid = live
        .pid
        .ok_or_else(|| anyhow!("container {id:?} has no pid after start"))?;

    let exit = wait_for_pid(pid)?;

    rt.delete(&handle.id, true)
        .with_context(|| format!("delete container {id:?}"))?;

    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}

/// Pull (or reuse the cache for) `args.image`, synthesise a bundle
/// under `<state-dir>/bundles/<id>/`, and run the resulting container.
/// Cleans up the bundle dir + drops the layer ref on foreground exit.
fn cmd_run_image(state_dir: &Path, args: RunArgs) -> Result<()> {
    let image_str = args
        .image
        .as_ref()
        .expect("cmd_run_image invariant: image is Some");
    let image_ref: ImageRef = image_str
        .parse()
        .with_context(|| format!("parse image ref {image_str:?}"))?;
    let id = args
        .id
        .clone()
        .unwrap_or_else(|| derive_id_from_image(&image_ref));

    // The image cache lives next to the bundle store under the same
    // state-dir; this keeps a single path for cleanup and `--state-dir`
    // overrides naturally affect both.
    let images_dir = state_dir.join("images");
    let client = Client::new(&images_dir).context("open image cache")?;
    let pulled = client
        .pull(&image_ref)
        .with_context(|| format!("pull image {image_ref}"))?;

    let bundle_dir = state_dir.join("bundles").join(&id);
    if bundle_dir.exists() {
        return Err(anyhow!(
            "bundle directory already exists: {bundle_dir:?} (delete container {id:?} first)"
        ));
    }
    std::fs::create_dir_all(&bundle_dir).with_context(|| format!("mkdir {bundle_dir:?}"))?;

    let builder = BundleBuilder::new(&pulled, client.store(), &bundle_dir);
    builder
        .assemble_rootfs()
        .with_context(|| format!("assemble rootfs for {id:?}"))?;

    let overrides = ConfigOverrides {
        args: if args.args.is_empty() {
            None
        } else {
            Some(args.args.clone())
        },
        ..Default::default()
    };
    builder
        .write_config(overrides)
        .with_context(|| format!("write config.json for {id:?}"))?;

    // Pin the layer set so a concurrent `wisp image gc` doesn't
    // pull blobs out from under the running container.
    let layer_digests: Vec<String> = pulled.layers.iter().map(|l| l.digest.clone()).collect();
    client
        .store()
        .add_ref(&id, &layer_digests)
        .with_context(|| format!("add layer ref for bundle {id:?}"))?;

    // Cleanup contract: we own the bundle dir and the layer ref. On
    // every error path past here we must drop both. Foreground runs
    // also drop them on a clean exit; detached runs leave them in
    // place until a future `wisp delete` (currently the operator must
    // also remove the bundle dir + ref by hand for detached image runs).
    let cleanup = |client: &Client, builder: &BundleBuilder, id: &str| {
        if let Err(e) = builder.cleanup() {
            eprintln!("wisp: warning: bundle cleanup failed: {e}");
        }
        if let Err(e) = client.store().drop_ref(id) {
            eprintln!("wisp: warning: drop_ref failed: {e}");
        }
        // Best-effort remove the bundle dir itself (config.json +
        // anything else write_config touched).
        if let Err(e) = std::fs::remove_dir_all(&bundle_dir) {
            // "not found" after rootfs cleanup is fine; warn on others.
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("wisp: warning: remove bundle dir failed: {e}");
            }
        }
    };

    let rt = Runtime::new(state_dir).context("initialise wisp runtime")?;

    let handle = match rt.create(&id, &bundle_dir) {
        Ok(h) => h,
        Err(e) => {
            cleanup(&client, &builder, &id);
            return Err(anyhow::Error::from(e)).with_context(|| format!("create container {id:?}"));
        }
    };

    if let Err(e) = rt.start(&handle.id) {
        let _ = rt.delete(&handle.id, true);
        cleanup(&client, &builder, &id);
        return Err(anyhow::Error::from(e)).with_context(|| format!("start container {id:?}"));
    }

    if args.detach {
        println!("{}", handle.id);
        // Detached mode keeps the bundle + ref alive; a later
        // `wisp delete` removes the runtime state but does NOT drop
        // the layer ref. Operator must `wisp image gc` after.
        return Ok(());
    }

    // Foreground: block until PID 1 exits, surface its status, clean up.
    let live = match rt.state(&handle.id) {
        Ok(s) => s,
        Err(e) => {
            let _ = rt.delete(&handle.id, true);
            cleanup(&client, &builder, &id);
            return Err(anyhow::Error::from(e)).with_context(|| format!("state {id:?}"));
        }
    };
    let pid = match live.pid {
        Some(p) => p,
        None => {
            let _ = rt.delete(&handle.id, true);
            cleanup(&client, &builder, &id);
            return Err(anyhow!("container {id:?} has no pid after start"));
        }
    };

    let exit = wait_for_pid(pid)?;

    if let Err(e) = rt.delete(&handle.id, true) {
        cleanup(&client, &builder, &id);
        return Err(anyhow::Error::from(e)).with_context(|| format!("delete container {id:?}"));
    }
    cleanup(&client, &builder, &id);

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

fn cmd_image(state_dir: &Path, args: ImageArgs) -> Result<()> {
    let images_dir = state_dir.join("images");
    let client = Client::new(&images_dir).context("open image cache")?;
    match args.cmd {
        ImageCmd::Pull { reference } => {
            let r: ImageRef = reference
                .parse()
                .with_context(|| format!("parse image ref {reference:?}"))?;
            let pulled = client.pull(&r).with_context(|| format!("pull image {r}"))?;
            print_pull_summary(&pulled);
            Ok(())
        }
        ImageCmd::List => {
            let images = client.list().context("list cached images")?;
            print_image_table(&images);
            Ok(())
        }
        ImageCmd::Rm { reference } => {
            let r: ImageRef = reference
                .parse()
                .with_context(|| format!("parse image ref {reference:?}"))?;
            remove_image_tag(&client, &r).with_context(|| format!("remove cached image {r}"))?;
            println!("removed: {r}");
            Ok(())
        }
        ImageCmd::Gc => {
            let report = client.gc().context("gc layer store")?;
            println!("gc: removed {}, kept {}", report.removed.len(), report.kept);
            Ok(())
        }
    }
}

/// Print a pull summary: manifest digest, layer count, total size.
fn print_pull_summary(pulled: &PulledImage) {
    let total_size: u64 = pulled.layers.iter().map(|l| l.size).sum();
    println!("pulled: {}", pulled.r);
    println!("  manifest: {}", pulled.manifest_digest);
    println!("  layers:   {}", pulled.layers.len());
    println!("  size:     {} bytes", total_size);
}

/// Print a 3-column table (REF / MANIFEST / LAYERS). Best-effort
/// formatting: when no images are cached, prints nothing.
fn print_image_table(images: &[PulledImage]) {
    if images.is_empty() {
        return;
    }
    let ref_w = images
        .iter()
        .map(|p| p.r.to_string().len())
        .max()
        .unwrap_or(3)
        .max("REF".len());
    println!(
        "{:<ref_w$}  {:<14}  LAYERS",
        "REF",
        "MANIFEST",
        ref_w = ref_w
    );
    for p in images {
        let short = short_digest(&p.manifest_digest);
        println!(
            "{:<ref_w$}  {:<14}  {}",
            p.r.to_string(),
            short,
            p.layers.len(),
            ref_w = ref_w
        );
    }
}

/// Truncate a `sha256:<hex>` digest to the conventional 12-char
/// short form. Falls back to the input unchanged if it doesn't
/// match the expected shape.
fn short_digest(digest: &str) -> String {
    if let Some(hex) = digest.strip_prefix("sha256:") {
        let take = hex.chars().take(12).collect::<String>();
        format!("sha256:{take}")
    } else {
        digest.to_string()
    }
}

/// Best-effort `image rm`: deletes the on-disk tag pointer at
/// `<images>/index/<registry>/<repo>/tag/<tag>`. The blob layers stay
/// in place; subsequent `gc` will reap them if no other ref pins them.
/// Errors only when the input ref is digest-only (we can't remove
/// what was never tagged).
fn remove_image_tag(client: &Client, r: &ImageRef) -> Result<()> {
    if r.digest.is_some() && r.tag.is_none() {
        return Err(anyhow!(
            "digest-only refs are not removable in 0.2 (no tag pointer to delete)"
        ));
    }
    let tag = r
        .tag
        .as_deref()
        .ok_or_else(|| anyhow!("image ref has no tag: {r}"))?;
    let store_root = client.store().root();
    // Layout mirrors `store::layout::tag_path`: index/<registry>/<repo>/tag/<tag>
    let mut tag_path = store_root.join("index").join(&r.registry);
    for segment in r.repo.split('/') {
        tag_path = tag_path.join(segment);
    }
    tag_path = tag_path.join("tag").join(tag);
    if !tag_path.exists() {
        return Err(anyhow!("no cached entry for {r}"));
    }
    std::fs::remove_file(&tag_path).with_context(|| format!("remove {tag_path:?}"))?;
    Ok(())
}

fn derive_id_from_bundle(bundle: &Path) -> String {
    bundle
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .unwrap_or_else(|| "wisp".to_string())
}

/// Default container ID for `wisp run --image <ref>`. Strips the
/// Docker Hub `library/` prefix (so `alpine:3.19` becomes `alpine-3-19`,
/// not `library-alpine-3-19`), replaces `/` and `.` with `-` so the
/// id is filesystem-safe, then truncates to 16 chars.
fn derive_id_from_image(r: &ImageRef) -> String {
    let repo = r.repo.trim_start_matches("library/");
    let tag = r.tag.as_deref().unwrap_or("latest");
    let raw = format!("{repo}-{tag}");
    let sanitised: String = raw
        .chars()
        .map(|c| match c {
            '/' | '.' | ':' => '-',
            _ => c,
        })
        .collect();
    sanitised.chars().take(16).collect()
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
    fn derive_id_from_image_strips_library_prefix() {
        let r: ImageRef = "alpine:3.19".parse().unwrap();
        let id = derive_id_from_image(&r);
        assert_eq!(id, "alpine-3-19");
        assert!(id.len() <= 16);
    }

    #[test]
    fn derive_id_from_image_replaces_slash_and_dot() {
        let r: ImageRef = "ghcr.io/foo/bar:1.2".parse().unwrap();
        let id = derive_id_from_image(&r);
        // foo/bar-1.2 -> foo-bar-1-2 (trimmed to 16)
        assert!(id.starts_with("foo-bar"));
        assert!(!id.contains('/'));
        assert!(!id.contains('.'));
        assert!(id.len() <= 16);
    }

    #[test]
    fn short_digest_truncates_sha256() {
        assert_eq!(
            short_digest("sha256:abcdef0123456789aaaa"),
            "sha256:abcdef012345"
        );
    }

    #[test]
    fn short_digest_passthrough_for_non_sha256() {
        assert_eq!(short_digest("weird-thing"), "weird-thing");
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
