//! Operator CLI for the Isengard cluster.
//!
//! `isd` talks to a controller over the REST + WebSocket surface the
//! dashboard uses, and to docker daemons directly via the operator's
//! docker context store at `~/.docker/contexts/`. There is no parallel
//! `credentials.toml`: contexts come from docker. `--context <name>`
//! overrides the active context per invocation.
//!
//! Subcommand groups (see [`help_render::GROUPS`] for the full layout):
//!
//!  - Containers: `ps`, `logs`, `stop`, `start`, `restart`, `rm`, `kill`.
//!  - Stacks: `stack ls | ps | deploy | diff | edit | manifest`, `open`.
//!  - Cluster: `hosts`, `service ls`, `route`, `secret`, `placement`,
//!    `join`, `join-token`.
//!  - Setup: `init`, `uninit`, `upgrade`, `context`, `update`.
//!  - Backup: `backup`, `restore`.
//!  - Access: `ssh` (dial, mint, status, hosts, ca).
//!  - Editor: `lsp`, `mcp`.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use clap::{Parser, Subcommand};

mod backup_cmd;
mod backup_credentials;
mod backup_crypto;
mod backup_storage;
mod compose_cmd;
mod confirm;
mod context;
mod docker_context;
mod help_render;
mod hosts_cmd;
mod index_cache;
mod index_resolve;
mod init_cmd;
mod join_cmd;
mod join_token_cmd;
mod lifecycle_cmd;
mod logs;
mod manifest_cmd;
mod open_cmd;
mod output;
mod placement_cmd;
mod ps;
mod render;
mod restore_cmd;
mod route;
mod secret;
mod selector;
mod service_cmd;
mod session;
mod ssh;
mod ssh_tunnel;
mod stack_cmd;
mod uninit_cmd;
mod update_cmd;
mod upgrade_cmd;
mod watch;

/// Top-level clap parser for `isd`. Holds the global flags (`--log`,
/// `--context`) and the resolved subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "isd",
    version = env!("ISENGARD_BUILD_VERSION"),
    about = "Operate Docker clusters from your terminal",
)]
pub(crate) struct Cli {
    /// Logging filter (e.g. "info", "debug,isd=trace").
    #[arg(long, global = true, env = "ISD_LOG")]
    log: Option<String>,

    /// Use a specific saved context.
    #[arg(long, global = true)]
    context: Option<String>,

    /// Resolved subcommand. `None` defaults to `Ps` (see [`default_command`]).
    #[command(subcommand)]
    command: Option<Command>,
}

/// One subcommand variant per top-level verb. Headlines (the `///` lines)
/// drive both `clap`'s auto-help and the grouped renderer in
/// [`help_render`]. Adding a variant means slotting it into
/// [`help_render::GROUPS`] (a unit test enforces that).
#[derive(Subcommand, Debug)]
enum Command {
    /// Manage saved controller contexts.
    #[command(subcommand)]
    Context(context::ContextCommand),
    /// Bootstrap a controller (and first agent) on this host. Swarm-style.
    Init(init_cmd::InitArgs),
    /// Tear down a cluster (stops + removes isd-controller and isd-agent).
    Uninit(uninit_cmd::UninitArgs),
    /// Join this host to an existing cluster.
    Join(join_cmd::JoinArgs),
    /// List containers.
    Ps(ps::PsArgs),
    /// Open a stack in the browser.
    Open(open_cmd::OpenArgs),
    /// Tail container logs.
    Logs(logs::LogsArgs),
    /// Stop one or more containers by ID, name, or index from `isd ps`.
    Stop(lifecycle_cmd::StopArgs),
    /// Start one or more containers.
    Start(lifecycle_cmd::StartArgs),
    /// Restart one or more containers.
    Restart(lifecycle_cmd::RestartArgs),
    /// Remove one or more containers. Confirms when targets came from
    /// indices; `-f` skips the prompt.
    Rm(lifecycle_cmd::RmArgs),
    /// Send a signal to one or more containers. Confirms when targets
    /// came from indices.
    Kill(lifecycle_cmd::KillArgs),
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
    /// Show the placement grid (one row per replica).
    #[command(subcommand)]
    Placement(placement_cmd::PlacementCommand),
    /// Mint a join-token (run against the controller-host context).
    JoinToken(join_token_cmd::JoinTokenArgs),
    /// Snapshot the controller state volume to fs / volume / s3 (encrypted).
    Backup(backup_cmd::BackupArgs),
    /// Restore the controller state volume from an encrypted backup.
    Restore(restore_cmd::RestoreArgs),
    /// Upgrade controller + agent to a new image tag (auto-backup first).
    Upgrade(upgrade_cmd::UpgradeArgs),
    /// Connect to a fleet host over SSH using a short-lived cert. Also
    /// hosts `mint`, `status`, `hosts`, and `ca pubkey` sub-verbs.
    /// Bare `isd ssh` opens the interactive host picker.
    Ssh(ssh::SshArgs),
    /// Run the Isengard language server over stdio. Editors invoke this
    /// via `isd lsp`; filetype detection + auto-attach are configured
    /// by the editor plugin (`isengard.nvim` for Neovim, VSCode extension
    /// for VSCode).
    Lsp,
    /// Run the Model Context Protocol server over stdio. AI hosts
    /// (Claude Code, any MCP-capable LLM) invoke this via `isd mcp`
    /// and consume the embedded operator docs, per-crate API reference,
    /// and AI playbooks under `skills/`.
    Mcp,
}

/// Process entry point. Intercepts the bare-`isd` / `isd -h` path so the
/// grouped help renderer runs instead of clap's flat list, parses the
/// CLI, initialises tracing, dispatches to the matching subcommand, and
/// turns any error into a one-line `isd:` message + exit-1.
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

    // Bare `isd` (no subcommand) defaults to `isd ps`
    // with the clap-resolved arg defaults. This matches docker's
    // `docker ps` convenience and replaces the pre-0.18 behaviour of
    // erroring with the help text.
    let command = cli.command.unwrap_or_else(default_command);

    let result = match command {
        Command::Context(cmd) => context::run(context::ContextArgs { command: cmd }).await,
        Command::Init(args) => init_cmd::run(args, cli.context.as_deref()).await,
        Command::Uninit(args) => uninit_cmd::run(args, cli.context.as_deref()).await,
        Command::Join(args) => join_cmd::run(args, cli.context.as_deref()).await,
        Command::Ps(args) => ps::run(args, cli.context.as_deref()).await,
        Command::Open(args) => open_cmd::run(args, cli.context.as_deref()).await,
        Command::Logs(args) => logs::run(args, cli.context.as_deref()).await,
        Command::Stop(args) => lifecycle_cmd::run_stop(args, cli.context.as_deref()).await,
        Command::Start(args) => lifecycle_cmd::run_start(args, cli.context.as_deref()).await,
        Command::Restart(args) => lifecycle_cmd::run_restart(args, cli.context.as_deref()).await,
        Command::Rm(args) => lifecycle_cmd::run_rm(args, cli.context.as_deref()).await,
        Command::Kill(args) => lifecycle_cmd::run_kill(args, cli.context.as_deref()).await,
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
        Command::Placement(cmd) => {
            placement_cmd::run(
                placement_cmd::PlacementArgs { command: cmd },
                cli.context.as_deref(),
            )
            .await
        }
        Command::JoinToken(args) => join_token_cmd::run(args, cli.context.as_deref()).await,
        Command::Backup(args) => backup_cmd::run(args, cli.context.as_deref()).await,
        Command::Restore(args) => restore_cmd::run(args, cli.context.as_deref()).await,
        Command::Upgrade(args) => upgrade_cmd::run(args, cli.context.as_deref()).await,
        Command::Ssh(args) => ssh::run(args, cli.context.as_deref()).await,
        Command::Lsp => isengard_lsp::run_stdio().await,
        Command::Mcp => isengard_mcp::run_stdio().await,
    };

    if let Err(e) = result {
        eprintln!("isd: {e:#}");
        std::process::exit(1);
    }
}

/// Default subcommand when the operator runs bare `isd` (no verb).
///
/// Builds a [`Command::Ps`] with the same defaults clap would pick for
/// an explicit `isd ps`, matching docker's `docker ps` convenience.
/// Pre-0.18 isd errored with the help text here; the bare-isd
/// default-and-document spec replaced that with a useful default.
fn default_command() -> Command {
    Command::Ps(ps::PsArgs {
        all: false,
        no_trunc: false,
        filters: Vec::new(),
        format: output::Format::Table,
        no_group: false,
        host: None,
    })
}

/// Install a `tracing_subscriber` writing to stderr. Filter precedence:
/// `ISD_LOG` env var, then the `--log` flag, then `warn` as the floor so
/// quiet runs stay quiet. Failures to install are swallowed (`try_init`
/// returning Err means a subscriber was already installed; the next
/// process gets one).
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
