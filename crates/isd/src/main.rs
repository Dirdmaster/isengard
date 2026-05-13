//! `isd`: the Isengard operator CLI. Talks to a controller over the same
//! REST + WebSocket surface the dashboard uses.
//!
//! Reach a controller via `isd context create <name> --ssh <target>` (the
//! canonical homelab path: SSH tunnels the dashboard port per command,
//! the operator's `~/.ssh/config` handles auth) or
//! `--http <url>` (local dev, direct-reachable dashboards). `isd context
//! use <name>` selects the default; `--context <name>` overrides per
//! invocation.
//!
//! Subcommands:
//!  - `isd context create | use | list | rm | show`
//!  - `isd ps`: list stacks + services
//!  - `isd open <stack>`: open the stack's primary host in a browser
//!  - `isd logs <stack>/<svc> -f`: tail service logs over the WebSocket
//!  - `isd deploy | diff | edit`: stack-level compose-as-truth
//!  - `isd gateway`: dev DNS + reverse proxy bridging the operator's Mac
//!  - `isd secret put | list | rm`: managed-secret CRUD
//!  - `isd route create | list | rm`: routing-rule CRUD
//!  - `isd hosts list`: enumerate enrolled hosts (ULID, hostname, fleet)
//!  - `isd update`: self-replace the operator binary from a GitHub Release

use clap::{Parser, Subcommand};

mod compose_cmd;
mod context;
mod credentials;
mod gateway;
mod hosts_cmd;
mod logs;
mod manifest_cmd;
mod open_cmd;
mod ps;
mod route;
mod secret;
mod session;
mod ssh_tunnel;
mod table;
mod update_cmd;
mod watch;

#[derive(Parser, Debug)]
#[command(
    name = "isd",
    // Overridden to use the build-script-resolved version (git tag for
    // CI builds, `git describe` for dev). The default `version` macro
    // reads CARGO_PKG_VERSION, which is pinned at `0.1.0-alpha` in the
    // workspace manifest and never bumped on release; that meant every
    // `isd --version` printed the wrong thing after `isd update`.
    version = env!("ISENGARD_BUILD_VERSION"),
    about = "Isengard operator CLI",
    long_about = "Talk to an Isengard controller from your terminal: list \
                  stacks, open services in a browser, tail logs. Configure \
                  reachability via `isd context create --ssh <target>` (or \
                  `--http <url>` for local dev)."
)]
struct Cli {
    /// Logging filter (e.g. "info", "debug,isd=trace"). Read from
    /// `ISD_LOG` if not set.
    #[arg(long, global = true, env = "ISD_LOG")]
    log: Option<String>,

    /// Use a specific saved context (defaults to the credentials file's
    /// `default_context`). Useful when you have multiple controllers.
    #[arg(long, global = true)]
    context: Option<String>,

    /// Subcommand. Optional in Phase 0.18: bare `isd` invokes
    /// `Ps` with default flags so the operator gets a docker-style
    /// container list with zero ceremony. Override default-and-document
    /// table entry in the spec.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage saved controller contexts. SSH-backed contexts tunnel the
    /// dashboard port via `~/.ssh/config`; HTTP contexts target a
    /// directly-reachable URL.
    #[command(subcommand)]
    Context(context::ContextCommand),
    /// List stacks + services across all hosts in the saved context.
    Ps(ps::PsArgs),
    /// Open the stack's primary `expose.host` in the OS default browser.
    Open(open_cmd::OpenArgs),
    /// Tail logs for `<stack>/<service>` over the controller WebSocket.
    Logs(logs::LogsArgs),
    /// Ship a stack to the controller. On first run for a name, creates
    /// the stack from the compose.yaml; subsequent runs preview the
    /// reconcile plan vs the current YAML, prompt y/N, then write.
    Deploy(compose_cmd::DeployArgs),
    /// v0.3d: show the reconcile plan for a proposed compose.yaml. Read-only.
    Diff(compose_cmd::DiffArgs),
    /// v0.3d: open the stack's compose.yaml in `$EDITOR`, then apply on save.
    Edit(compose_cmd::EditArgs),
    /// v0.3.5: DNS + reverse proxy bridging the operator's Mac to
    /// containerized stacks. Single foreground command; Ctrl+C tears down.
    Gateway(gateway::GatewayArgs),
    /// v0.3.6: manage Isengard-managed secrets. `put` upserts, `list`
    /// shows names only (no values), `rm` deletes.
    #[command(subcommand)]
    Secret(secret::SecretCommand),
    /// Manage routing rules. `list` / `create` / `rm`. Most rules come from
    /// stack compose.yaml `expose.host` annotations; this surface is for
    /// non-stack routes (e.g., the controller dashboard) and ad-hoc edits.
    #[command(subcommand)]
    Route(route::RouteCommand),
    /// Inspect enrolled hosts. `list` prints a kubectl-style table with
    /// host ULID, hostname, enrolled timestamp, and fleet. Without this
    /// the canonical Crockford ULID for a host was only recoverable by
    /// dumping the controller's SQLite by hand: wave 5.B polish.
    #[command(subcommand)]
    Hosts(hosts_cmd::HostsCommand),
    /// View + edit a deployed stack's `stack.toml` from the controller.
    /// `cat` prints the persisted body to stdout; `export` writes it to
    /// a local file; `edit` opens it in `$EDITOR` and PUTs the result
    /// back with optimistic concurrency (sha256 If-Match).
    #[command(subcommand)]
    Manifest(manifest_cmd::ManifestCommand),
    /// v0.5.2: self-update the operator CLI. Detects the latest GitHub
    /// release for the host triple (macOS aarch64/x86_64, Linux musl
    /// x86_64/aarch64), verifies sha256 against the release manifest,
    /// and atomic-renames the new binary onto the running executable.
    /// Doesn't restart anything: isd is one-shot, the next invocation
    /// picks up the new binary.
    Update(update_cmd::UpdateArgs),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref());

    // Phase 0.18: bare `isd` (no subcommand) defaults to `isd ps`
    // with the clap-resolved arg defaults. This matches docker's
    // `docker ps` convenience and replaces the pre-0.18 behaviour of
    // erroring with the help text.
    let command = cli.command.unwrap_or_else(default_command);

    let result = match command {
        Command::Context(cmd) => context::run(context::ContextArgs { command: cmd }).await,
        Command::Ps(args) => ps::run(args, cli.context.as_deref()).await,
        Command::Open(args) => open_cmd::run(args, cli.context.as_deref()).await,
        Command::Logs(args) => logs::run(args, cli.context.as_deref()).await,
        Command::Deploy(args) => compose_cmd::run_deploy(args, cli.context.as_deref()).await,
        Command::Diff(args) => compose_cmd::run_diff(args, cli.context.as_deref()).await,
        Command::Edit(args) => compose_cmd::run_edit(args, cli.context.as_deref()).await,
        Command::Gateway(args) => gateway::run(args, cli.context.as_deref()).await,
        Command::Secret(cmd) => {
            secret::run(secret::SecretArgs { command: cmd }, cli.context.as_deref()).await
        }
        Command::Route(cmd) => {
            route::run(route::RouteArgs { command: cmd }, cli.context.as_deref()).await
        }
        Command::Hosts(cmd) => {
            hosts_cmd::run(
                hosts_cmd::HostsArgs { command: cmd },
                cli.context.as_deref(),
            )
            .await
        }
        Command::Manifest(cmd) => {
            manifest_cmd::run(
                manifest_cmd::ManifestArgs { command: cmd },
                cli.context.as_deref(),
            )
            .await
        }
        Command::Update(args) => update_cmd::run(args).await,
    };

    if let Err(e) = result {
        eprintln!("isd: {e:#}");
        std::process::exit(1);
    }
}

/// Phase 0.18: when the operator runs bare `isd` with no subcommand,
/// route through `Ps` with the same defaults clap would pick for an
/// explicit `isd ps`. Spec entry: bare-isd default-and-document.
fn default_command() -> Command {
    Command::Ps(ps::PsArgs {
        all: false,
        no_trunc: false,
        filters: Vec::new(),
        format: "table".into(),
        legacy: false,
        json: false,
        fleet: None,
    })
}

fn init_tracing(filter: Option<&str>) {
    let env = std::env::var("ISD_LOG")
        .ok()
        .or_else(|| filter.map(str::to_owned))
        .unwrap_or_else(|| "warn".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(env))
        .with_writer(std::io::stderr)
        .try_init();
}
