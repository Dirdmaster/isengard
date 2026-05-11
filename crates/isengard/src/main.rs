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
mod init;
mod tracing_init;
mod update;

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
    // `version = env!("ISENGARD_BUILD_VERSION")` overrides clap's default
    // which would otherwise pick up CARGO_PKG_VERSION (a stale workspace
    // value the release pipeline never bumps). The build script bakes in
    // the git tag for CI builds and `git describe` output for dev builds,
    // so `isengard --version` matches the binary that's actually running.
    version = env!("ISENGARD_BUILD_VERSION"),
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
    /// Phase 0.10: bootstrap a fresh host. Generates the master key,
    /// bootstraps secrets, writes the systemd units + env files, brings
    /// up the controller and the local agent. Replaces the legacy
    /// `install/install.sh` bash flow. Interactive when stdin is a TTY,
    /// flag-driven (`--non-interactive`) for scripted installs.
    Init(init::InitArgs),
    /// Phase 0.10: enrol THIS host as an agent against an existing
    /// controller running elsewhere. Smaller than `init`: no controller
    /// boot, no master key, just installs the agent unit and starts it
    /// with the operator-supplied token + CA.
    Join(init::JoinArgs),
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
        /// v0.3b: zone served by the embedded DNS resolver (e.g. `iso`).
        /// Empty string disables the resolver entirely (default).
        #[arg(long, env = "ISENGARD_DNS_ZONE", default_value = "")]
        dns_zone: String,
        /// v0.3b: UDP address the embedded DNS resolver binds to. Default
        /// `0.0.0.0:5300` for dev; production compose can map :53 with
        /// `cap_add: NET_BIND_SERVICE`. Ignored when `--dns-zone` is empty.
        #[arg(long, env = "ISENGARD_DNS_LISTEN", default_value = "0.0.0.0:5300")]
        dns_listen: String,
        /// ACME contact email for the LE account. Required to enable
        /// DNS-01 wildcard cert issuance.
        #[arg(long, env = "ISENGARD_ACME_EMAIL")]
        acme_email: Option<String>,
        /// Cloudflare API token with `Zone:DNS:Edit`. Required to enable
        /// DNS-01 wildcard cert issuance via the CF provider.
        #[arg(long, env = "ISENGARD_CF_DNS_API_TOKEN")]
        cf_dns_api_token: Option<String>,
        /// Comma-separated list of domains to obtain certs for, e.g.
        /// `*.vallee.casa,vallee.casa`. The wildcard and its apex are
        /// auto-paired into a single LE order.
        #[arg(long, env = "ISENGARD_ACME_DOMAINS", default_value = "")]
        acme_domains: String,
        /// ACME directory URL. Defaults to LE production. Set to the LE
        /// staging URL for dry-runs:
        ///   https://acme-staging-v02.api.letsencrypt.org/directory
        #[arg(long, env = "ISENGARD_ACME_DIRECTORY", default_value = "")]
        acme_directory: String,
        #[command(subcommand)]
        action: Option<ControllerAction>,
    },
    /// Bootstrap-time secrets management. Used by the installer to seed
    /// the encrypted secrets store before the controller boots; reads the
    /// master key directly from the host filesystem and writes the
    /// ciphertext to the same SQLite database the running controller uses.
    /// Not for day-to-day ops: use `isd secret put|list|rm` against a
    /// running dashboard once the stack is up.
    Secret {
        #[command(subcommand)]
        op: SecretOp,
    },
    /// Phase 0.18 friendly auto-update. Zero-args: queries GitHub
    /// Releases for the latest tag, downloads + verifies the binary
    /// for this host's target triple, atomic-renames onto
    /// `/usr/local/bin/isengard`, and restarts both
    /// `iso-controller.service` and `iso-agent.service`.
    ///
    /// For scripted / pinned upgrades use `--version vX.Y.Z`. For
    /// dry-run discovery use `--check`. For unattended use `--yes`.
    /// The low-level `self-update` subcommand is still available for
    /// callers that already know the URL + sha256.
    Update {
        /// Dry-run mode: print "current vX, latest vY" and exit
        /// without downloading or restarting anything.
        #[arg(long)]
        check: bool,
        /// Pin to a specific release tag (e.g. `v0.5.1`). When unset
        /// the latest release is resolved from the GitHub API.
        #[arg(long)]
        version: Option<String>,
        /// Skip the y/N confirmation prompt. Useful for cron / CI.
        #[arg(long)]
        yes: bool,
    },
    /// Phase 0.8 binary self-update. Downloads a new isengard binary,
    /// verifies its sha256 against `--sha256`, atomically replaces the
    /// running binary on disk, and triggers `systemctl restart` on the
    /// named unit (default: `iso-agent.service` for the agent updating
    /// itself; pass `--no-restart` to skip and orchestrate manually).
    ///
    /// Designed for the systemd-native install. The legacy
    /// docker-compose path uses the rename-and-recreate flow in
    /// `crates/isengard-plugins/updater/src/self_update.rs` instead.
    /// For zero-args operator UX use the `update` subcommand instead.
    SelfUpdate {
        /// HTTPS URL of the new binary. Typically a GitHub Releases
        /// asset URL (e.g.
        /// https://github.com/Weavers-Engineering/Isengard/releases/download/vX.Y.Z/isengard-x86_64-unknown-linux-musl).
        #[arg(long)]
        url: String,
        /// Lowercase-hex sha256 of the expected bytes. Matches the
        /// format the release pipeline writes to `<asset>.sha256`.
        #[arg(long)]
        sha256: String,
        /// systemd unit to restart after the rename. Default
        /// `iso-agent.service`. Set to an empty string or pass
        /// `--no-restart` to skip the restart entirely.
        #[arg(long, default_value = "iso-agent.service")]
        unit: String,
        /// Skip the post-install systemctl restart. The new binary is
        /// in place but the old process keeps running until the next
        /// manual restart.
        #[arg(long)]
        no_restart: bool,
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
        /// Network interface name the mDNS responder advertises on (v0.3a).
        /// Defaults to the first non-loopback IPv4 interface. Pass this on
        /// hosts with multiple NICs where the wrong one would be picked
        /// (e.g. a docker bridge ahead of the LAN interface).
        #[arg(long, env = "ISENGARD_ADVERTISE_IFACE")]
        advertise_iface: Option<String>,
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
enum SecretOp {
    /// Seed an encrypted secret in the controller's SQLite store. The
    /// installer pipes the value on stdin, then the controller boots
    /// and reads the same DB. Plaintext NEVER touches a file on the
    /// host: stdin only.
    Bootstrap {
        /// Secret name (matches `isd secret put`'s rules:
        /// `[A-Za-z0-9._-]{1,64}`).
        name: String,
        /// Path to the raw 32-byte master key file. The installer
        /// generates this via `openssl rand 32 > /etc/isengard/master.key`.
        #[arg(long)]
        master_key_file: std::path::PathBuf,
        /// Where the controller's SQLite database lives. Must match the
        /// `--state-dir` the controller will boot with.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
    },
    /// List the names of secrets seeded so far. No values are read; the
    /// installer uses this to confirm what's been bootstrapped.
    ListBootstrap {
        /// Path to the raw 32-byte master key file. Required so the
        /// listing path uses the same boot guard the controller does.
        #[arg(long)]
        master_key_file: std::path::PathBuf,
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
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
async fn main() {
    // Install a process-level rustls CryptoProvider before any TLS session is
    // built. Required since instant-acme 0.8 transitively pulls in a
    // rustls-platform-verifier path that panics if the default provider is
    // unset and Rustls cannot pick one from compile-time features. We want
    // exactly one provider for the whole binary; do it once, here, at the top.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    let mode = match &cli.command {
        Command::Controller { .. } => "controller",
        Command::Agent { .. } => "agent",
        Command::Secret { .. } => "secret",
        Command::SelfUpdate { .. } => "self-update",
        Command::Update { .. } => "update",
        Command::Init(_) => "init",
        Command::Join(_) => "join",
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
            dns_zone,
            dns_listen,
            acme_email,
            cf_dns_api_token,
            acme_domains,
            acme_directory,
            action,
        } => match action {
            None => {
                run_controller(
                    listen,
                    state_dir,
                    dns_zone,
                    dns_listen,
                    acme_email,
                    cf_dns_api_token,
                    acme_domains,
                    acme_directory,
                )
                .await
            }
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
            advertise_iface,
        } => {
            run_agent_mode(
                controller,
                state_dir,
                enroll_token,
                controller_ca_pem_path,
                advertise_iface,
            )
            .await
        }
        Command::Secret { op } => match op {
            SecretOp::Bootstrap {
                name,
                master_key_file,
                state_dir,
            } => run_secret_bootstrap(name, master_key_file, state_dir).await,
            SecretOp::ListBootstrap {
                master_key_file,
                state_dir,
            } => run_secret_list_bootstrap(master_key_file, state_dir).await,
        },
        Command::SelfUpdate {
            url,
            sha256,
            unit,
            no_restart,
        } => run_self_update(url, sha256, unit, no_restart).await,
        Command::Update {
            check,
            version,
            yes,
        } => {
            update::run(update::UpdateArgs {
                check,
                version,
                yes,
            })
            .await
        }
        Command::Init(args) => init::run(args).await,
        Command::Join(args) => init::run_join(args).await,
    }
}

/// Phase 0.8 binary self-update entry point. Thin wrapper that picks
/// the right `restart_unit` argument based on the CLI flags and
/// delegates to [`isengard_agent::self_update::run_self_update`].
async fn run_self_update(
    url: String,
    sha256: String,
    unit: String,
    no_restart: bool,
) -> Result<()> {
    let unit_ref = unit.as_str();
    let units: &[&str] = if no_restart || unit_ref.is_empty() {
        &[]
    } else {
        std::slice::from_ref(&unit_ref)
    };
    isengard_agent::self_update::run_self_update(&url, &sha256, units).await
}

/// Read the master key from `master_key_file`, open the controller's
/// SQLite at `state_dir`, read the operator-supplied secret value from
/// stdin, encrypt with the master key, and upsert the row.
///
/// The plaintext lives only on stdin and in process memory. We
/// deliberately do not log the value's length: even that is metadata
/// the operator might prefer not to leak.
async fn run_secret_bootstrap(
    name: String,
    master_key_file: std::path::PathBuf,
    state_dir: std::path::PathBuf,
) -> Result<()> {
    use std::io::Read;

    if name.is_empty() {
        return Err(anyhow!("secret name must be non-empty"));
    }

    // Read raw bytes from stdin. `read_to_end` so the operator can pipe
    // arbitrary binary (a private key PEM, a JSON blob, whatever).
    let mut value = Vec::new();
    std::io::stdin()
        .read_to_end(&mut value)
        .context("reading secret value from stdin")?;
    if value.is_empty() {
        return Err(anyhow!(
            "stdin was empty; pipe the value (printf '%s' \"$VAL\" | isengard secret bootstrap {name})"
        ));
    }
    // Strip exactly one trailing newline if it crept in via heredoc /
    // `echo` so the operator doesn't have to remember `printf '%s'`.
    if value.last() == Some(&b'\n') {
        value.pop();
    }

    let key = isengard_controller::secrets::read_master_key(&master_key_file)
        .with_context(|| format!("reading master key from {master_key_file:?}"))?;

    let inv = Arc::new(open_inventory(&state_dir).await?);
    let store = isengard_controller::secrets::SecretsStore::new(inv, key);

    store
        .put(&name, &value, Some("bootstrap"))
        .await
        .with_context(|| format!("storing secret {name:?}"))?;

    println!("ok");
    Ok(())
}

/// List the names + timestamps of bootstrapped secrets. Verifies the
/// master key file is readable so the installer fails loud on the same
/// guard the controller uses, even though listing names doesn't actually
/// require decryption.
async fn run_secret_list_bootstrap(
    master_key_file: std::path::PathBuf,
    state_dir: std::path::PathBuf,
) -> Result<()> {
    let key = isengard_controller::secrets::read_master_key(&master_key_file)
        .with_context(|| format!("reading master key from {master_key_file:?}"))?;

    let inv = Arc::new(open_inventory(&state_dir).await?);
    let store = isengard_controller::secrets::SecretsStore::new(inv, key);

    let names = store.list().await.context("listing bootstrap secrets")?;
    if names.is_empty() {
        println!("(no secrets bootstrapped)");
    } else {
        for meta in names {
            println!("{}\t{}", meta.name, meta.updated_at.to_rfc3339());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_controller(
    listen: String,
    state_dir: std::path::PathBuf,
    dns_zone: String,
    dns_listen: String,
    acme_email: Option<String>,
    cf_dns_api_token: Option<String>,
    acme_domains: String,
    acme_directory: String,
) -> Result<()> {
    let listen_addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|e| anyhow!("invalid --listen address {listen:?}: {e}"))?;

    let dns_listen_addr: std::net::SocketAddr = dns_listen
        .parse()
        .map_err(|e| anyhow!("invalid --dns-listen address {dns_listen:?}: {e}"))?;

    // Phase 0.16 migration: pre-Phase-0.16 installs put the controller's
    // SQLite + CA under `<state_dir>/controller/`. The new layout writes
    // them at `<state_dir>` directly. If we see the legacy DB and the new
    // location is empty, fall back to the legacy path so an in-place
    // upgrade doesn't lose data. The CLI's matching subcommands
    // (token mint / agent list / ca export) default to the same
    // `--state-dir` value, so they pick up the migrated path here too via
    // the boot-side log line.
    let state_dir = resolve_controller_state_dir(state_dir);

    // Create state dir if missing (controller can't run without somewhere to put the db).
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| anyhow!("creating state dir {state_dir:?}: {e}"))?;

    let acme = isengard_controller::acme::AcmeConfig {
        email: acme_email,
        cf_api_token: cf_dns_api_token,
        domains: acme_domains,
        directory_url: acme_directory,
    };

    tracing::info!(%listen_addr, ?state_dir, dns_zone = %dns_zone, %dns_listen_addr, "controller mode");
    isengard_controller::run_controller(isengard_controller::ControllerOptions {
        listen: listen_addr,
        state_dir,
        config: serde_json::Value::Object(Default::default()),
        dns_zone,
        dns_listen: dns_listen_addr,
        acme,
    })
    .await
}

/// Open the inventory at the same path the controller boot path uses
/// (`<state_dir>/isengard.db`). Subcommands need direct access for one-shot
/// admin operations (mint / revoke / list). Auto-applies the Phase 0.16
/// state-dir migration so `isengard controller token mint` works on
/// pre-0.16 hosts (data under `<state_dir>/controller/`) without
/// requiring the operator to pass `--state-dir /var/lib/isengard/controller`.
async fn open_inventory(state_dir: &std::path::Path) -> Result<Inventory> {
    let state_dir = resolve_controller_state_dir(state_dir.to_path_buf());
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| anyhow!("creating state dir {state_dir:?}: {e}"))?;
    let db_path = state_dir.join("isengard.db");
    Inventory::open(&db_path)
        .await
        .with_context(|| format!("opening inventory at {db_path:?}"))
}

/// Phase 0.16 state-dir migration shim. Previously the systemd-native
/// install rooted the controller at `<state_dir>/controller/`; Phase 0.16
/// drops the suffix so the CLI's `--state-dir /var/lib/isengard` default
/// matches the running unit.
///
/// Behaviour:
///   * If `<state_dir>/isengard.db` exists, use the new layout (return
///     `state_dir` unchanged).
///   * Else if `<state_dir>/controller/isengard.db` exists, log a one-line
///     hint and return the legacy path. Operators can move the data up one
///     level at their leisure; nothing forces a flag-day migration.
///   * Else (greenfield install) return `state_dir` unchanged so the new
///     directory is created at the new path.
fn resolve_controller_state_dir(state_dir: std::path::PathBuf) -> std::path::PathBuf {
    let new_db = state_dir.join("isengard.db");
    if new_db.exists() {
        return state_dir;
    }
    let legacy = state_dir.join("controller");
    let legacy_db = legacy.join("isengard.db");
    if legacy_db.exists() {
        tracing::warn!(
            ?legacy,
            "Phase 0.16: controller state still at legacy `<state_dir>/controller/`; \
             using it as-is. Move data up one level (`mv {legacy}/* {new}/`) when convenient.",
            legacy = legacy.display(),
            new = state_dir.display(),
        );
        return legacy;
    }
    state_dir
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
    advertise_iface: Option<String>,
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
        advertise_iface,
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
