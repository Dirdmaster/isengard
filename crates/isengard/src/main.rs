//! Isengard binary entry point. Parses subcommand, sets up tracing, dispatches
//! to either the controller or agent runner.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::{RevocationSet, revoke_agent};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;

#[cfg(feature = "dev")]
mod dev_plugin;

// Force-link the notifier plugin so its `inventory::submit!` registration is
// picked up at controller startup. The `as _` import keeps the symbol live
// without polluting the main namespace.
#[allow(unused_imports)]
use isengard_plugin_dashboard as _;
#[allow(unused_imports)]
#[cfg(feature = "cf-tunnel")]
use isengard_plugin_networking_cf_tunnel as _;
#[allow(unused_imports)]
use isengard_plugin_networking_none as _;
#[allow(unused_imports)]
#[cfg(feature = "tailscale")]
use isengard_plugin_networking_tailscale as _;
#[allow(unused_imports)]
use isengard_plugin_notifier as _;

#[derive(Debug, Parser)]
#[command(
    name = "isengard",
    version,
    about = "Isengard container management platform"
)]
struct Cli {
    /// Logging filter (e.g. "info", "debug,isengard=trace"). Read from
    /// `RUST_LOG` env var if not set.
    #[arg(long, global = true, env = "ISENGARD_LOG")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run in controller mode: aggregates agent state, hosts the dashboard
    /// and notifier plugins, distributes config. With no subcommand the
    /// controller boots and serves; subcommands provide operator tooling
    /// (mint enrollment tokens, revoke / list agent certs).
    Controller {
        /// HTTP/gRPC listen address.
        #[arg(long, env = "ISENGARD_LISTEN", default_value = "0.0.0.0:9417")]
        listen: String,
        /// Directory where the controller stores mutable state (SQLite db, etc).
        /// Created if missing.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
        #[command(subcommand)]
        action: Option<ControllerAction>,
    },
    /// Run in agent mode: registers with a controller, runs agent-side plugins
    /// (updater).
    Agent {
        /// URL of the controller, e.g. `http://controller.example.com:9417`.
        #[arg(long, env = "ISENGARD_CONTROLLER")]
        controller: String,
        /// Directory where the agent persists state (agent.json, etc).
        /// Created if missing.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
        /// One-time enrollment token. Required for first-time enrollment;
        /// ignored once the agent has a persisted cert bundle.
        #[arg(long, env = "ISENGARD_ENROLL_TOKEN")]
        enroll_token: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ControllerAction {
    /// Token management (mint enrollment tokens for new agents).
    Token {
        #[command(subcommand)]
        op: TokenOp,
    },
    /// Agent management (revoke certs, list enrolled agents).
    Agent {
        #[command(subcommand)]
        op: AgentOp,
    },
}

#[derive(Debug, Subcommand)]
enum TokenOp {
    /// Mint a new one-time-use enrollment token. Prints the plaintext token
    /// on stdout (shown once — only the SHA-256 hash is persisted).
    Mint {
        /// Role for the minted token. Currently only "agent" is supported.
        #[arg(long, default_value = "agent")]
        role: String,
        /// TTL in human-readable form (e.g. "15m", "1h", "24h").
        #[arg(long, default_value = "15m")]
        ttl: humantime::Duration,
    },
}

#[derive(Debug, Subcommand)]
enum AgentOp {
    /// Revoke the active cert for an agent. `host_id` is the ULID string
    /// printed by `agent list`.
    Revoke {
        host_id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// List enrolled agents along with their active cert info.
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = cli.log.as_deref().map(EnvFilter::new).unwrap_or_else(|| {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    });
    let use_ansi = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    match cli.command {
        Command::Controller {
            listen,
            state_dir,
            action,
        } => match action {
            None => run_controller(listen, state_dir).await,
            Some(ControllerAction::Token {
                op: TokenOp::Mint { role, ttl },
            }) => run_token_mint(state_dir, role, ttl).await,
            Some(ControllerAction::Agent {
                op: AgentOp::Revoke { host_id, reason },
            }) => run_agent_revoke(state_dir, host_id, reason).await,
            Some(ControllerAction::Agent { op: AgentOp::List }) => run_agent_list(state_dir).await,
        },
        // TODO: fix in Task 11 — the agent crate currently does not compile
        // (its enroll module references stale proto field names). The CLI
        // surface lands here so tests / docs can already reference it.
        Command::Agent {
            controller,
            state_dir,
            enroll_token,
        } => run_agent_mode(controller, state_dir, enroll_token).await,
    }
}

async fn run_controller(listen: String, state_dir: std::path::PathBuf) -> Result<()> {
    let listen_addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|e| anyhow!("invalid --listen address {listen:?}: {e}"))?;

    // Create state dir if missing (controller can't run without somewhere to put the db).
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| anyhow!("creating state dir {state_dir:?}: {e}"))?;

    tracing::info!(%listen_addr, ?state_dir, "controller mode");
    isengard_controller::run_controller(isengard_controller::ControllerOptions {
        listen: listen_addr,
        state_dir,
        config: serde_json::Value::Object(Default::default()),
    })
    .await
}

/// Open the inventory at the same path the controller boot path uses
/// (`<state_dir>/isengard.db`). Subcommands need direct access for one-shot
/// admin operations (mint / revoke / list).
async fn open_inventory(state_dir: &std::path::Path) -> Result<Inventory> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| anyhow!("creating state dir {state_dir:?}: {e}"))?;
    let db_path = state_dir.join("isengard.db");
    Inventory::open(&db_path)
        .await
        .with_context(|| format!("opening inventory at {db_path:?}"))
}

fn parse_role(role: &str) -> Result<TokenRole> {
    match role {
        "agent" => Ok(TokenRole::Agent),
        other => Err(anyhow!(
            "unknown token role {other:?} (only 'agent' supported)"
        )),
    }
}

async fn run_token_mint(
    state_dir: std::path::PathBuf,
    role: String,
    ttl: humantime::Duration,
) -> Result<()> {
    let inv = Arc::new(open_inventory(&state_dir).await?);
    let ca = Arc::new(
        Authority::load_or_init(&inv)
            .await
            .context("loading or initializing CA")?,
    );
    let enr = EnrollmentService::new(inv, ca);

    let role = parse_role(&role)?;
    let std_ttl: std::time::Duration = ttl.into();
    let chrono_ttl =
        chrono::Duration::from_std(std_ttl).map_err(|e| anyhow!("invalid ttl {ttl}: {e}"))?;

    let token = enr.mint(role, chrono_ttl).await?;
    println!("{token}");
    Ok(())
}

async fn run_agent_revoke(
    state_dir: std::path::PathBuf,
    host_id: String,
    reason: String,
) -> Result<()> {
    let inv = open_inventory(&state_dir).await?;
    let revoc = RevocationSet::load_from_inventory(&inv)
        .await
        .context("hydrating revocation set")?;

    let ulid = ulid::Ulid::from_string(&host_id)
        .map_err(|e| anyhow!("invalid host_id {host_id:?}: {e}"))?;
    let host_id = HostId::from(ulid);

    revoke_agent(&inv, &revoc, host_id, &reason).await?;
    println!("revoked");
    Ok(())
}

async fn run_agent_list(state_dir: std::path::PathBuf) -> Result<()> {
    let inv = open_inventory(&state_dir).await?;
    let hosts = inv.list_hosts().await.context("list hosts")?;
    for host in hosts {
        match inv.active_cert_for_host(host.id).await? {
            Some(cert) => {
                let serial_hex = hex::encode(&cert.serial);
                let prefix = &serial_hex[..serial_hex.len().min(16)]; // first 8 bytes
                println!(
                    "{}\t{}\tserial={}\texpires={}",
                    host.id,
                    host.hostname,
                    prefix,
                    cert.expires_at.to_rfc3339()
                );
            }
            None => {
                println!("{}\t{}\tno cert", host.id, host.hostname);
            }
        }
    }
    Ok(())
}

#[allow(unused_variables)]
async fn run_agent_mode(
    controller: String,
    state_dir: std::path::PathBuf,
    enroll_token: Option<String>,
) -> Result<()> {
    // TODO: fix in Task 11 — the isengard-agent crate currently does not
    // compile (its enroll module references stale proto fields). Once Task
    // 11 lands, plumb `enroll_token` into the agent's enroll bootstrap and
    // restore the proxy / TLS option wiring that previously lived here.
    Err(anyhow!(
        "agent mode is temporarily disabled while Task 11 refactors the enroll path"
    ))
}
