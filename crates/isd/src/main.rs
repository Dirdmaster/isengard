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

use clap::{Parser, Subcommand};

mod compose_cmd;
mod context;
mod credentials;
mod gateway;
mod logs;
mod open_cmd;
mod ps;
mod route;
mod secret;
mod session;
mod ssh_tunnel;
mod table;

#[derive(Parser, Debug)]
#[command(
    name = "isd",
    version,
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

    #[command(subcommand)]
    command: Command,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref());

    let result = match cli.command {
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
    };

    if let Err(e) = result {
        eprintln!("isd: {e:#}");
        std::process::exit(1);
    }
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
