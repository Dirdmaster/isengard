//! `isd`: the Isengard operator CLI. Talks to a controller over the same
//! REST + WebSocket surface the dashboard uses.
//!
//! v0.3a ships four subcommands:
//!  - `isd login <controller_url>`: trust + token bootstrap
//!  - `isd ps`: list stacks + services
//!  - `isd open <stack>`: open the stack's primary host in a browser
//!  - `isd logs <stack>/<svc> -f`: tail service logs over the WebSocket
//!
//! v0.3.5 adds:
//!  - `isd gateway`: DNS resolver + reverse proxy on the operator's Mac for
//!    reaching `*.isd` (or any zone) in a browser without fighting
//!    mDNS-in-Docker-on-Mac.

use clap::{Parser, Subcommand};

mod compose_cmd;
mod credentials;
mod gateway;
mod login;
mod logs;
mod open_cmd;
mod ps;
mod route;
mod secret;
mod table;

#[derive(Parser, Debug)]
#[command(
    name = "isd",
    version,
    about = "Isengard operator CLI",
    long_about = "Talk to an Isengard controller from your terminal: list \
                  stacks, open services in a browser, tail logs."
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
    /// Save controller credentials. Prompts for a token + verifies the
    /// self-signed CA fingerprint on first connect.
    Login(login::LoginArgs),
    /// List stacks + services across all hosts in the saved context.
    Ps(ps::PsArgs),
    /// Open the stack's primary `expose.host` in the OS default browser.
    Open(open_cmd::OpenArgs),
    /// Tail logs for `<stack>/<service>` over the controller WebSocket.
    Logs(logs::LogsArgs),
    /// v0.3d: stage a compose.yaml, preview the reconcile plan, then
    /// (after y/N) write it to the host's `/etc/isengard/stacks/<name>/`.
    Apply(compose_cmd::ApplyArgs),
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
        Command::Login(args) => login::run(args).await,
        Command::Ps(args) => ps::run(args, cli.context.as_deref()).await,
        Command::Open(args) => open_cmd::run(args, cli.context.as_deref()).await,
        Command::Logs(args) => logs::run(args, cli.context.as_deref()).await,
        Command::Apply(args) => compose_cmd::run_apply(args, cli.context.as_deref()).await,
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
