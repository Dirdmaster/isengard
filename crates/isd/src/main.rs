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
mod help_render;
mod hosts_cmd;
mod index_cache;
mod index_resolve;
mod lifecycle_cmd;
mod logs;
mod manifest_cmd;
mod open_cmd;
mod output;
mod ps;
mod render;
mod route;
mod secret;
mod selector;
mod service_cmd;
mod session;
mod ssh_tunnel;
mod stack_cmd;
mod table;
mod update_cmd;
mod watch;

#[derive(Parser, Debug)]
#[command(
    name = "isd",
    version = env!("ISENGARD_BUILD_VERSION"),
    about = "Operate Docker fleets from your terminal",
)]
pub(crate) struct Cli {
    /// Logging filter (e.g. "info", "debug,isd=trace").
    #[arg(long, global = true, env = "ISD_LOG")]
    log: Option<String>,

    /// Use a specific saved context.
    #[arg(long, global = true)]
    context: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage saved controller contexts.
    #[command(subcommand)]
    Context(context::ContextCommand),
    /// List containers.
    Ps(ps::PsArgs),
    /// Open a stack in the browser.
    Open(open_cmd::OpenArgs),
    /// Tail container logs.
    Logs(logs::LogsArgs),
    /// Stop one or more containers by ID, name, or index from `isd ps`.
    Stop(lifecycle_cmd::StopArgs),
    /// Manage secrets.
    #[command(subcommand)]
    Secret(secret::SecretCommand),
    /// Manage routing rules.
    #[command(subcommand)]
    Route(route::RouteCommand),
    /// Inspect enrolled hosts.
    #[command(subcommand)]
    Hosts(hosts_cmd::HostsCommand),
    /// Self-update isd.
    Update(update_cmd::UpdateArgs),
    /// Deploy, diff, edit, inspect stacks.
    #[command(subcommand)]
    Stack(stack_cmd::StackCommand),
    /// List services across stacks.
    #[command(subcommand)]
    Service(service_cmd::ServiceCommand),
}

#[tokio::main]
async fn main() {
    use clap::CommandFactory;

    // Intercept the root help flow: bare `isd` or `-h`/`--help` at the
    // root prints the grouped help, then exits.
    let raw: Vec<String> = std::env::args().collect();
    let wants_root_help =
        raw.len() == 1 || (raw.len() == 2 && matches!(raw[1].as_str(), "-h" | "--help"));
    if wants_root_help {
        let cmd = Cli::command();
        println!("{}", help_render::render(&cmd));
        return;
    }

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
        Command::Stop(args) => lifecycle_cmd::run_stop(args, cli.context.as_deref()).await,
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
        Command::Update(args) => update_cmd::run(args).await,
        Command::Stack(cmd) => {
            stack_cmd::run(
                stack_cmd::StackArgs { command: cmd },
                cli.context.as_deref(),
            )
            .await
        }
        Command::Service(cmd) => {
            service_cmd::run(
                service_cmd::ServiceArgs { command: cmd },
                cli.context.as_deref(),
            )
            .await
        }
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
        format: output::Format::Table,
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
