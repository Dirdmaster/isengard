//! Isengard binary entry point. Parses subcommand, sets up tracing, dispatches
//! to either the controller or agent runner.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use clap::{Parser, Subcommand, ValueEnum};

use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::{RevocationSet, revoke_agent};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;

#[cfg(feature = "dev")]
mod dev_plugin;
mod tracing_init;

// Force-link the notifier plugin so its `inventory::submit!` registration is
// picked up at controller startup. The `as _` import keeps the symbol live
// without polluting the main namespace.
#[allow(unused_imports)]
use isengard_plugin_backup as _;
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
#[allow(unused_imports)]
use isengard_plugin_webhooks as _;

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
        /// Path to a PEM file holding the controller's CA root cert. Pinned
        /// as the bootstrap channel's trust anchor for the Enroll RPC.
        /// REQUIRED when the controller serves a self-signed cert (the
        /// default for an Isengard-managed CA). For a publicly-signed
        /// controller cert, leave unset to fall back to the platform's
        /// native root store. Get the PEM via
        /// `isengard controller ca export` on the controller host.
        #[arg(long, env = "ISENGARD_CONTROLLER_CA_PEM_PATH")]
        controller_ca_pem_path: Option<std::path::PathBuf>,
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
    /// Internal CA management (export the root cert PEM).
    Ca {
        #[command(subcommand)]
        op: CaOp,
    },
}

#[derive(Debug, Subcommand)]
enum TokenOp {
    /// Mint a new one-time-use enrollment token. By default prints a
    /// copy-pasteable `docker run` join command (Swarm-style); use
    /// `--format token` for the bare token (scripts, CI). Only the
    /// SHA-256 hash of the token is persisted on the controller.
    Mint {
        /// Role for the minted token. Currently only "agent" is supported.
        #[arg(long, default_value = "agent")]
        role: String,
        /// TTL in human-readable form (e.g. "15m", "1h", "24h").
        #[arg(long, default_value = "15m")]
        ttl: humantime::Duration,
        /// Public address (host:port) where agents should reach the
        /// controller. Embedded in the join-command output. Required for
        /// cross-host setups; falls back to `ISENGARD_PUBLIC_ADDR` env then
        /// to `controller.local:9417`.
        #[arg(long, env = "ISENGARD_PUBLIC_ADDR")]
        public_addr: Option<String>,
        /// Image reference to embed in the join command.
        #[arg(long, default_value = "ghcr.io/dirdmaster/isengard:next")]
        image: String,
        /// Output format. `text` (default) prints the join-command block;
        /// `token` prints just the bare token (back-compat with pre-Phase-15
        /// flows and CI scripts).
        #[arg(long, value_enum, default_value_t = MintFormat::Text)]
        format: MintFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MintFormat {
    /// Full Docker Swarm-style join block (default).
    Text,
    /// Bare token, one line, no other output.
    Token,
}

#[derive(Debug, Subcommand)]
enum CaOp {
    /// Print the controller's CA root certificate PEM to stdout. Operators
    /// pipe this into a file the agent can mount + pin via
    /// `ISENGARD_CONTROLLER_CA_PEM_PATH`.
    Export,
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
async fn main() {
    let cli = Cli::parse();

    let mode = match &cli.command {
        Command::Controller { .. } => "controller",
        Command::Agent { .. } => "agent",
    };
    tracing_init::init(mode, cli.log.as_deref());

    if let Err(err) = dispatch(cli.command).await {
        tracing_init::print_error_chain(&err);
        std::process::exit(1);
    }
}

/// Top-level dispatch. Split out of `main` so the entry point can print a
/// pretty error chain on `Err` instead of letting `anyhow`'s default debug
/// formatter dump a wall of text.
async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Controller {
            listen,
            state_dir,
            action,
        } => match action {
            None => run_controller(listen, state_dir).await,
            Some(ControllerAction::Token {
                op:
                    TokenOp::Mint {
                        role,
                        ttl,
                        public_addr,
                        image,
                        format,
                    },
            }) => run_token_mint(state_dir, role, ttl, public_addr, image, format).await,
            Some(ControllerAction::Agent {
                op: AgentOp::Revoke { host_id, reason },
            }) => run_agent_revoke(state_dir, host_id, reason).await,
            Some(ControllerAction::Agent { op: AgentOp::List }) => run_agent_list(state_dir).await,
            Some(ControllerAction::Ca { op: CaOp::Export }) => run_ca_export(state_dir).await,
        },
        Command::Agent {
            controller,
            state_dir,
            enroll_token,
            controller_ca_pem_path,
        } => run_agent_mode(controller, state_dir, enroll_token, controller_ca_pem_path).await,
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
    public_addr: Option<String>,
    image: String,
    format: MintFormat,
) -> Result<()> {
    let inv = Arc::new(open_inventory(&state_dir).await?);
    let ca = Arc::new(
        Authority::load_or_init(&inv)
            .await
            .context("loading or initializing CA")?,
    );
    // Snapshot the CA root PEM before the service consumes the Arc; it's
    // borrowed for the join-block path. `load_or_init` is a no-op on an
    // already-initialized CA, so this is one read, not two.
    let ca_pem = ca.root_cert_pem().to_string();

    let enr = EnrollmentService::new(inv, ca);
    let role_parsed = parse_role(&role)?;
    let std_ttl: std::time::Duration = ttl.into();
    let chrono_ttl =
        chrono::Duration::from_std(std_ttl).map_err(|e| anyhow!("invalid ttl {ttl}: {e}"))?;

    let minted_at = chrono::Utc::now();
    let token = enr.mint(role_parsed, chrono_ttl).await?;

    match format {
        MintFormat::Token => {
            // Legacy mode: bare token, one line. Scripts and CI rely on this.
            println!("{token}");
        }
        MintFormat::Text => {
            let host_port = resolve_public_addr(public_addr.as_deref());
            let ca_b64 = base64::engine::general_purpose::STANDARD.encode(ca_pem.as_bytes());
            let expires_at = minted_at + chrono_ttl;
            let block = render_join_command(JoinCommandArgs {
                ttl: ttl.to_string(),
                token: &token,
                ca_b64: &ca_b64,
                image: &image,
                public_addr: &host_port,
                expires_at,
            });
            println!("{block}");
        }
    }
    Ok(())
}

/// Resolve the controller's public address (host:port) for the join command.
/// Order: `--public-addr` flag (already in `arg`), `ISENGARD_PUBLIC_ADDR` env
/// (already in `arg` via clap), then the `controller.local:9417` default.
/// Listed `--listen` is intentionally *not* a fallback: bind addresses like
/// `0.0.0.0` or `127.0.0.1` are not externally dialable and would emit a
/// broken join command.
fn resolve_public_addr(public_addr: Option<&str>) -> String {
    public_addr
        .map(str::to_owned)
        .unwrap_or_else(|| "controller.local:9417".to_string())
}

struct JoinCommandArgs<'a> {
    ttl: String,
    token: &'a str,
    ca_b64: &'a str,
    image: &'a str,
    public_addr: &'a str,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Render the Swarm-style `docker run` join block.
///
/// Plain ASCII, four-space indent on the command, backslash continuations so
/// it pastes cleanly into bash/zsh/fish. The footer reminds the operator how
/// to mint a fresh token if this one expires before they paste it.
fn render_join_command(a: JoinCommandArgs<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("Token minted (expires in {}).\n\n", a.ttl));
    out.push_str("To enroll an agent, run on the host where you want it to live:\n\n");
    out.push_str("    docker run -d \\\n");
    out.push_str("      --name iso-agent \\\n");
    out.push_str("      --restart unless-stopped \\\n");
    out.push_str("      --platform linux/amd64 \\\n");
    out.push_str("      -v iso-agent-state:/var/lib/isengard \\\n");
    out.push_str("      -v /var/run/docker.sock:/var/run/docker.sock \\\n");
    out.push_str(&format!("      -e ISENGARD_ENROLL_TOKEN={} \\\n", a.token));
    out.push_str(&format!(
        "      -e ISENGARD_CONTROLLER_CA_PEM_BASE64={} \\\n",
        a.ca_b64
    ));
    out.push_str(&format!("      {} \\\n", a.image));
    out.push_str(&format!(
        "      agent --controller https://{} --state-dir /var/lib/isengard\n\n",
        a.public_addr
    ));
    out.push_str(&format!(
        "Token expires at {}. Mint a new one with:\n",
        a.expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    out.push_str("    isengard controller token mint --role agent\n");
    out
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

async fn run_agent_mode(
    controller: String,
    state_dir: std::path::PathBuf,
    enroll_token: Option<String>,
    controller_ca_pem_path: Option<std::path::PathBuf>,
) -> Result<()> {
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| anyhow!("creating state dir {state_dir:?}: {e}"))?;

    tracing::info!(%controller, ?state_dir, "agent mode");
    isengard_agent::run_agent(isengard_agent::AgentOptions {
        controller_url: controller,
        state_dir,
        config: serde_json::Value::Object(Default::default()),
        proxy_http_port: Some(8080),
        proxy_https_port: Some(8443),
        tls: Some(isengard_agent::TlsOptions {
            cert_dir: "/var/lib/isengard/tls".into(),
            acme_contact_email: std::env::var("ISENGARD_ACME_EMAIL")
                .unwrap_or_else(|_| "ops@example.com".into()),
            acme_directory_url: std::env::var("ISENGARD_ACME_DIRECTORY")
                .unwrap_or_else(|_| isengard_agent::tls::LE_STAGING_URL.to_string()),
        }),
        enroll_token,
        bootstrap_trust: isengard_agent::enroll::BootstrapTrust {
            ca_pem_path: controller_ca_pem_path,
            ca_pem: None,
        },
    })
    .await
}

/// Print the controller's CA root cert PEM to stdout. The controller's first
/// boot persists the CA in SQLite; this subcommand reuses [`Authority::load_or_init`]
/// (which is a no-op when the CA already exists) and writes the persisted PEM.
async fn run_ca_export(state_dir: std::path::PathBuf) -> Result<()> {
    let inv = open_inventory(&state_dir).await?;
    let ca = Authority::load_or_init(&inv)
        .await
        .context("loading or initializing CA")?;
    print!("{}", ca.root_cert_pem());
    Ok(())
}
