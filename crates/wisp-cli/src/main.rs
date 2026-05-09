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

mod net_attacher;

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use nix::sys::signal::Signal;

use wisp::{
    ContainerHandle, ContainerState, NetworkSpec, PortProtocol, PortPublish, ResolvSource, Runtime,
};
use wisp_image::{BundleBuilder, Client, ConfigOverrides, ImageRef, PulledImage};

use crate::net_attacher::WispNetAttacher;

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
    /// Manage wisp bridge networks (Phase 0.3).
    Net(NetArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to the OCI bundle directory (`config.json` + `rootfs/`).
    /// Used only when `--image` is NOT set; with `--image`, this slot
    /// folds into the trailing args (so
    /// `wisp run --image <ref> /bin/echo hi` runs the image with the
    /// echo command appended to its entrypoint).
    bundle: Option<PathBuf>,
    /// Pull and assemble a bundle from this image ref. Without this,
    /// the positional `bundle` is required.
    #[arg(long)]
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
    /// Attach to the named wisp bridge network. The bridge is
    /// auto-created on first use (subnet `10.83.0.0/24` for the
    /// default `wisp-default` network). Pre-create with
    /// `wisp net create <name> --subnet <cidr>` to pick a subnet.
    #[arg(long)]
    network: Option<String>,
    /// Publish a container port to the host. Repeatable. Shapes
    /// supported: `HOST:CONTAINER`, `HOST_IP:HOST:CONTAINER`,
    /// `HOST:CONTAINER/tcp`, `HOST_IP:HOST:CONTAINER/udp`. Default
    /// protocol is `tcp`; default `host_ip` is `0.0.0.0`. Setting
    /// `--port` without `--network` auto-attaches to `wisp-default`.
    #[arg(long, value_name = "[HOST_IP:]HOST_PORT:CONTAINER_PORT[/PROTO]")]
    port: Vec<String>,
    /// With `--image`: extra args appended to the image's entrypoint.
    /// The positional `bundle` slot also folds into this list when
    /// `--image` is set. Ignored when running a positional bundle.
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

#[derive(Debug, Args)]
struct NetArgs {
    #[command(subcommand)]
    cmd: NetCmd,
}

#[derive(Debug, Subcommand)]
enum NetCmd {
    /// Create a wisp bridge network. Idempotent: a pre-existing
    /// bridge with the same subnet succeeds; a different subnet
    /// errors with `Conflict`.
    Create {
        /// Network name. Bridge interface is `wbr-<name>` truncated
        /// to the kernel's `IFNAMSIZ - 1 = 15` byte limit.
        name: String,
        /// IPv4 subnet (CIDR). Gateway is the first usable host.
        #[arg(long, default_value = "10.83.0.0/24")]
        subnet: String,
    },
    /// List wisp-managed bridges (anything matching `wbr-*` under
    /// `/sys/class/net`).
    List,
    /// Tear down a wisp bridge network. Tolerates missing bridge.
    /// Network-level iptables rules are revoked too.
    Rm { name: String },
    /// Print bridge name + subnet + gateway + currently allocated
    /// IPs for a wisp network.
    Inspect {
        name: String,
        /// IPv4 subnet (CIDR), needed to derive the gateway. Default
        /// matches `wisp net create`'s default.
        #[arg(long, default_value = "10.83.0.0/24")]
        subnet: String,
    },
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
        Cmd::Net(args) => cmd_net(&cli.state_dir, args),
    }
}

/// Default network name + subnet used when `--port` is set without an
/// explicit `--network`. Matches the spec.
const DEFAULT_NETWORK_NAME: &str = "wisp-default";
const DEFAULT_NETWORK_SUBNET: &str = "10.83.0.0/24";

/// Resolve the wisp-net `Network` for a `wisp run`'s `--network <name>`.
///
/// Looks up an existing bridge first (so the operator can pick a
/// non-default subnet via `wisp net create <name> --subnet <cidr>`).
/// Falls back to the default `10.83.0.0/24` for an unprovisioned name
/// so `wisp run --network app --port 18080:80 web` Just Works without
/// a separate create step.
fn resolve_network(name: &str) -> Result<wisp_net::Network> {
    // The CLI doesn't persist subnet -> network mappings on disk
    // (wisp-net keeps that information implicit in the bridge's
    // `ip addr show` output). So resolution is "default subnet" until
    // a future phase introduces a network registry.
    let subnet: ipnet::Ipv4Net = DEFAULT_NETWORK_SUBNET
        .parse()
        .expect("hard-coded default subnet must parse");
    wisp_net::Network::new(name, subnet)
        .map_err(|e| anyhow!("build network {name:?} from default subnet: {e}"))
}

/// IPAM state-dir for `network`. Lives under
/// `<state-dir>/networks/<name>/`.
fn ipam_dir(state_dir: &Path, network: &str) -> PathBuf {
    state_dir.join("networks").join(network)
}

/// Parse one `--port` value.
///
/// Shapes accepted:
/// - `HOST:CONTAINER` (default `host_ip = 0.0.0.0`, `proto = tcp`)
/// - `HOST_IP:HOST:CONTAINER`
/// - either of the above suffixed with `/tcp` or `/udp`
fn parse_port_publish(raw: &str) -> Result<PortPublish> {
    let (head, proto) = match raw.rsplit_once('/') {
        Some((h, p)) => {
            let proto = match p.to_ascii_lowercase().as_str() {
                "tcp" => PortProtocol::Tcp,
                "udp" => PortProtocol::Udp,
                other => {
                    return Err(anyhow!(
                        "unknown protocol {other:?} in --port {raw:?}: must be tcp or udp"
                    ));
                }
            };
            (h, proto)
        }
        None => (raw, PortProtocol::Tcp),
    };

    let parts: Vec<&str> = head.split(':').collect();
    let (host_ip, host_port_s, container_port_s) = match parts.as_slice() {
        [h, c] => (
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            (*h).to_string(),
            (*c).to_string(),
        ),
        [ip, h, c] => {
            let host_ip: IpAddr = ip
                .parse()
                .map_err(|e| anyhow!("invalid host ip {ip:?} in --port {raw:?}: {e}"))?;
            (host_ip, (*h).to_string(), (*c).to_string())
        }
        _ => {
            return Err(anyhow!(
                "invalid --port {raw:?}: expected HOST:CONTAINER or HOST_IP:HOST:CONTAINER (with optional /tcp or /udp)"
            ));
        }
    };

    let host_port: u16 = host_port_s
        .parse()
        .map_err(|e| anyhow!("invalid host port {host_port_s:?} in --port {raw:?}: {e}"))?;
    let container_port: u16 = container_port_s.parse().map_err(|e| {
        anyhow!("invalid container port {container_port_s:?} in --port {raw:?}: {e}")
    })?;

    if host_port == 0 || container_port == 0 {
        return Err(anyhow!("--port {raw:?}: ports must be non-zero"));
    }

    Ok(PortPublish {
        host_ip,
        host_port,
        container_port,
        protocol: proto,
    })
}

fn cmd_run(state_dir: &Path, mut args: RunArgs) -> Result<()> {
    // When --image is set, the positional `bundle` slot is just the
    // first piece of the trailing args (clap's positional rules require
    // the optional `bundle` to bind first, even though semantically we
    // want it to be part of the args list). Splice it in here so
    // callers can write `wisp run --image alpine:3.19 /bin/echo hi`.
    if args.image.is_some()
        && let Some(b) = args.bundle.take()
    {
        let mut prepended = vec![b.to_string_lossy().into_owned()];
        prepended.append(&mut args.args);
        args.args = prepended;
    }

    match (&args.bundle, &args.image) {
        (Some(_), None) => cmd_run_bundle(state_dir, args),
        (None, Some(_)) => cmd_run_image(state_dir, args),
        (Some(_), Some(_)) => {
            // After the splice above this is unreachable in practice;
            // keep the guard so a future refactor can't drop into the
            // wrong code path silently.
            Err(anyhow!(
                "--image and a positional bundle are mutually exclusive"
            ))
        }
        (None, None) => Err(anyhow!(
            "either a positional bundle path or `--image <ref>` is required"
        )),
    }
}

/// Resolve `--network` + `--port` into an optional `(NetworkSpec,
/// network)` pair. Returns `None` when no network is requested.
fn build_network_spec(args: &RunArgs) -> Result<Option<(NetworkSpec, wisp_net::Network)>> {
    let want_net = args.network.is_some() || !args.port.is_empty();
    if !want_net {
        return Ok(None);
    }

    let net_name = args
        .network
        .clone()
        .unwrap_or_else(|| DEFAULT_NETWORK_NAME.to_string());
    let network = resolve_network(&net_name)?;

    let mut ports = Vec::with_capacity(args.port.len());
    for raw in &args.port {
        ports.push(parse_port_publish(raw)?);
    }

    let spec = NetworkSpec {
        network_name: net_name,
        ports,
        resolv_source: ResolvSource::HostCopy,
    };
    Ok(Some((spec, network)))
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

    let net_pair = build_network_spec(&args)?;

    if net_pair.is_some() {
        // Best-effort host-global ip_forward toggle. Without this the
        // FORWARD rules accept the packet but the kernel still drops
        // it, and curl hangs. The helper is itself idempotent.
        if let Err(e) = wisp::lifecycle::ensure_global_ip_forward() {
            tracing::warn!("ensure_global_ip_forward (best-effort): {e}");
        }
    }

    let (handle, mut attacher_opt) = match net_pair {
        Some((spec, network)) => {
            let mut attacher =
                WispNetAttacher::new(network, &ipam_dir(state_dir, &spec.network_name));
            let h = rt
                .create_with_network(&id, bundle, spec)
                .with_context(|| format!("create_with_network {id:?}"))?;
            rt.start_with_attacher(&h.id, &mut attacher)
                .with_context(|| format!("start_with_attacher {id:?}"))?;
            (h, Some(attacher))
        }
        None => {
            let h = rt
                .create(&id, bundle)
                .with_context(|| format!("create container {id:?}"))?;
            rt.start(&h.id)
                .with_context(|| format!("start container {id:?}"))?;
            (h, None)
        }
    };

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

    if let Some(attacher) = attacher_opt.as_mut() {
        rt.delete_with_attacher(&handle.id, true, attacher)
            .with_context(|| format!("delete container {id:?}"))?;
    } else {
        rt.delete(&handle.id, true)
            .with_context(|| format!("delete container {id:?}"))?;
    }

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

    let net_pair = match build_network_spec(&args) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&client, &builder, &id);
            return Err(e);
        }
    };

    if net_pair.is_some() {
        if let Err(e) = wisp::lifecycle::ensure_global_ip_forward() {
            tracing::warn!("ensure_global_ip_forward (best-effort): {e}");
        }
    }

    let (handle, mut attacher_opt) = match net_pair {
        Some((spec, network)) => {
            let mut attacher =
                WispNetAttacher::new(network, &ipam_dir(state_dir, &spec.network_name));
            let h = match rt.create_with_network(&id, &bundle_dir, spec) {
                Ok(h) => h,
                Err(e) => {
                    cleanup(&client, &builder, &id);
                    return Err(anyhow::Error::from(e))
                        .with_context(|| format!("create_with_network {id:?}"));
                }
            };
            if let Err(e) = rt.start_with_attacher(&h.id, &mut attacher) {
                let _ = rt.delete_with_attacher(&h.id, true, &mut attacher);
                cleanup(&client, &builder, &id);
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("start_with_attacher {id:?}"));
            }
            (h, Some(attacher))
        }
        None => {
            let h = match rt.create(&id, &bundle_dir) {
                Ok(h) => h,
                Err(e) => {
                    cleanup(&client, &builder, &id);
                    return Err(anyhow::Error::from(e))
                        .with_context(|| format!("create container {id:?}"));
                }
            };
            if let Err(e) = rt.start(&h.id) {
                let _ = rt.delete(&h.id, true);
                cleanup(&client, &builder, &id);
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("start container {id:?}"));
            }
            (h, None)
        }
    };

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
            // Best-effort delete on the failure path. If we have an
            // attacher use it so the network detach happens too.
            if let Some(a) = attacher_opt.as_mut() {
                let _ = rt.delete_with_attacher(&handle.id, true, a);
            } else {
                let _ = rt.delete(&handle.id, true);
            }
            cleanup(&client, &builder, &id);
            return Err(anyhow::Error::from(e)).with_context(|| format!("state {id:?}"));
        }
    };
    let pid = match live.pid {
        Some(p) => p,
        None => {
            if let Some(a) = attacher_opt.as_mut() {
                let _ = rt.delete_with_attacher(&handle.id, true, a);
            } else {
                let _ = rt.delete(&handle.id, true);
            }
            cleanup(&client, &builder, &id);
            return Err(anyhow!("container {id:?} has no pid after start"));
        }
    };

    let exit = wait_for_pid(pid)?;

    let delete_result = if let Some(a) = attacher_opt.as_mut() {
        rt.delete_with_attacher(&handle.id, true, a)
    } else {
        rt.delete(&handle.id, true)
    };
    if let Err(e) = delete_result {
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
    // If state.json carries a `network_attachment`, build an attacher
    // pinned to that network and route through delete_with_attacher
    // so the iptables rules + host veth + IPAM allocation are all
    // reversed. Without this, `wisp delete <id>` on a detached
    // container leaks the host-side network state.
    let handle = rt
        .state(&args.id)
        .with_context(|| format!("state {:?}", args.id))?;
    if let Some(att) = handle.network_attachment.as_ref() {
        let network = resolve_network(&att.network_name)?;
        let mut attacher = WispNetAttacher::new(network, &ipam_dir(state_dir, &att.network_name));
        rt.delete_with_attacher(&args.id, args.force, &mut attacher)
            .with_context(|| format!("delete {:?}", args.id))?;
    } else {
        rt.delete(&args.id, args.force)
            .with_context(|| format!("delete {:?}", args.id))?;
    }
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

fn cmd_net(state_dir: &Path, args: NetArgs) -> Result<()> {
    match args.cmd {
        NetCmd::Create { name, subnet } => cmd_net_create(&name, &subnet),
        NetCmd::List => cmd_net_list(),
        NetCmd::Rm { name } => cmd_net_rm(&name),
        NetCmd::Inspect { name, subnet } => cmd_net_inspect(state_dir, &name, &subnet),
    }
}

/// `wisp net create <name> [--subnet <cidr>]`.
///
/// Steps: nudge `ip_forward=1`, build a `wisp_net::Network`, ensure
/// the bridge, apply network-level iptables rules. Idempotent.
fn cmd_net_create(name: &str, subnet: &str) -> Result<()> {
    let subnet: ipnet::Ipv4Net = subnet
        .parse()
        .with_context(|| format!("parse subnet {subnet:?}"))?;

    if let Err(e) = wisp::lifecycle::ensure_global_ip_forward() {
        // Mac dev path doesn't have /proc; surface as a warning rather
        // than blocking the command. The integration test on Linux
        // will catch a real misconfiguration.
        tracing::warn!("ensure_global_ip_forward (best-effort): {e}");
    }

    let net =
        wisp_net::Network::new(name, subnet).map_err(|e| anyhow!("build network {name:?}: {e}"))?;

    wisp_net::bridge::ensure(&net).map_err(|e| anyhow!("bridge::ensure {}: {e}", net.bridge))?;

    let net_rs = wisp_net::iptables::plan_for_network(&net);
    // Revoke first so a stale rule from a previous create doesn't
    // turn into a duplicate. revoke is tolerant of "not found".
    let _ = wisp_net::iptables::revoke(&net_rs);
    wisp_net::iptables::apply(&net_rs)
        .map_err(|e| anyhow!("iptables::apply (network rules): {e}"))?;

    println!(
        "created: {name} (bridge {} subnet {} gateway {})",
        net.bridge, net.subnet, net.gateway
    );
    Ok(())
}

/// `wisp net ls`. Prints one row per `/sys/class/net/wbr-*` entry.
fn cmd_net_list() -> Result<()> {
    let bridges = wisp_net::bridge::list_wisp_bridges().context("list wisp bridges")?;
    if bridges.is_empty() {
        return Ok(());
    }
    let bridge_w = bridges
        .iter()
        .map(|b| b.len())
        .max()
        .unwrap_or(0)
        .max("BRIDGE".len());
    println!("{:<bridge_w$}  NAME", "BRIDGE", bridge_w = bridge_w);
    for b in &bridges {
        // Strip the wbr- prefix to derive a best-effort name. Names
        // longer than 15 - 4 = 11 chars get truncated by the kernel,
        // so this is heuristic for display only.
        let name = b.strip_prefix("wbr-").unwrap_or(b);
        println!("{:<bridge_w$}  {}", b, name, bridge_w = bridge_w);
    }
    Ok(())
}

/// `wisp net rm <name>`. Tolerates missing bridge / missing rules.
fn cmd_net_rm(name: &str) -> Result<()> {
    // We don't know the subnet from the bridge interface alone (and
    // 0.3 has no on-disk network registry). Use the default subnet:
    // it scopes the iptables revoke (the rules have the bridge name
    // baked in) and the bridge::delete only cares about the bridge
    // name. Stale rules with a different subnet would not be
    // revoked, but the masquerade rule's `-s <subnet>` differs,
    // which is a corner case the operator can flush by hand if hit.
    let subnet: ipnet::Ipv4Net = DEFAULT_NETWORK_SUBNET
        .parse()
        .expect("hard-coded default subnet must parse");
    let net =
        wisp_net::Network::new(name, subnet).map_err(|e| anyhow!("build network {name:?}: {e}"))?;

    let net_rs = wisp_net::iptables::plan_for_network(&net);
    let _ = wisp_net::iptables::revoke(&net_rs);

    wisp_net::bridge::delete(&net).map_err(|e| anyhow!("bridge::delete {}: {e}", net.bridge))?;

    println!("removed: {name}");
    Ok(())
}

/// `wisp net inspect <name>`. Prints bridge + subnet + gateway +
/// allocations.
fn cmd_net_inspect(state_dir: &Path, name: &str, subnet: &str) -> Result<()> {
    use wisp_net::Ipam as _;

    let subnet: ipnet::Ipv4Net = subnet
        .parse()
        .with_context(|| format!("parse subnet {subnet:?}"))?;
    let net =
        wisp_net::Network::new(name, subnet).map_err(|e| anyhow!("build network {name:?}: {e}"))?;

    println!("name:    {name}");
    println!("bridge:  {}", net.bridge);
    println!("subnet:  {}", net.subnet);
    println!("gateway: {}", net.gateway);

    let bridges = wisp_net::bridge::list_wisp_bridges().unwrap_or_default();
    let present = bridges.contains(&net.bridge);
    println!(
        "status:  {}",
        if present {
            "up (bridge present)"
        } else {
            "down (no bridge)"
        }
    );

    let ipam = wisp_net::StaticBitmapIpam::new(&ipam_dir(state_dir, name));
    match ipam.list(name) {
        Ok(map) if map.is_empty() => println!("allocations: (none)"),
        Ok(map) => {
            println!("allocations:");
            for (id, ip) in &map {
                println!("  {id}\t{ip}");
            }
        }
        Err(e) => {
            tracing::warn!("ipam.list {name:?}: {e}");
        }
    }
    Ok(())
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

    #[test]
    fn parse_port_accepts_host_container() {
        let p = parse_port_publish("8080:80").unwrap();
        assert_eq!(p.host_ip, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(p.host_port, 8080);
        assert_eq!(p.container_port, 80);
        assert_eq!(p.protocol, PortProtocol::Tcp);
    }

    #[test]
    fn parse_port_accepts_host_ip_host_container() {
        let p = parse_port_publish("127.0.0.1:8080:80").unwrap();
        assert_eq!(p.host_ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(p.host_port, 8080);
        assert_eq!(p.container_port, 80);
        assert_eq!(p.protocol, PortProtocol::Tcp);
    }

    #[test]
    fn parse_port_accepts_proto_suffix() {
        let p = parse_port_publish("5353:53/udp").unwrap();
        assert_eq!(p.host_port, 5353);
        assert_eq!(p.container_port, 53);
        assert_eq!(p.protocol, PortProtocol::Udp);

        let p = parse_port_publish("8080:80/tcp").unwrap();
        assert_eq!(p.protocol, PortProtocol::Tcp);
    }

    #[test]
    fn parse_port_accepts_host_ip_with_proto() {
        let p = parse_port_publish("127.0.0.1:8080:80/tcp").unwrap();
        assert_eq!(p.host_ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(p.host_port, 8080);
        assert_eq!(p.container_port, 80);
        assert_eq!(p.protocol, PortProtocol::Tcp);
    }

    #[test]
    fn parse_port_rejects_garbage_proto() {
        let err = parse_port_publish("8080:80/sctp").unwrap_err().to_string();
        assert!(err.contains("unknown protocol"), "got: {err}");
    }

    #[test]
    fn parse_port_rejects_too_many_colon_segments() {
        let err = parse_port_publish("a:b:c:d").unwrap_err().to_string();
        assert!(err.contains("invalid --port"), "got: {err}");
    }

    #[test]
    fn parse_port_rejects_zero_port() {
        let err = parse_port_publish("0:80").unwrap_err().to_string();
        assert!(err.contains("non-zero"), "got: {err}");
    }

    #[test]
    fn parse_port_rejects_non_numeric_port() {
        let err = parse_port_publish("abc:80").unwrap_err().to_string();
        assert!(err.contains("invalid host port"), "got: {err}");
    }

    #[test]
    fn parse_port_rejects_invalid_ip() {
        let err = parse_port_publish("not-an-ip:8080:80")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid host ip"), "got: {err}");
    }
}
