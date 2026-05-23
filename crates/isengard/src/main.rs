//! Isengard binary entry point. Parses subcommand, sets up tracing, dispatches
//! to either the controller or agent runner.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
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
    /// Mint an enrollment token for a new agent (alias for
    /// `controller token mint`, mirroring `docker swarm join-token`).
    JoinToken {
        /// Role to mint for. Only "agent" is supported today.
        #[arg(default_value = "agent")]
        role: String,
        /// State directory holding the controller's CA + inventory.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
        /// Token TTL (e.g. "15m", "1h", "24h").
        #[arg(long, default_value = "15m")]
        ttl: humantime::Duration,
        /// Public host:port for agents to dial. Falls back to
        /// $ISENGARD_PUBLIC_ADDR, then `controller.local:9417`.
        #[arg(long, env = "ISENGARD_PUBLIC_ADDR")]
        public_addr: Option<String>,
        /// Agent image reference to embed in the join command.
        #[arg(long, default_value = "ghcr.io/dirdmaster/isengard:next")]
        image: String,
        /// Output format. `text` prints the legacy join block, `token`
        /// prints just the packed token, `joincmd` prints a single-line
        /// `isd join --controller ... --token ...` invocation.
        #[arg(long, value_enum, default_value_t = MintFormat::Text)]
        format: MintFormat,
    },
    /// Run in controller mode. With no subcommand the controller boots
    /// and serves; subcommands provide operator tooling.
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
    /// Not for day-to-day ops: use `isd secret set|ls|rm` against a
    /// running dashboard once the stack is up.
    Secret {
        #[command(subcommand)]
        op: SecretOp,
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
        /// ignored once the agent has a persisted cert bundle. The packed
        /// `TK<bytes>.<fingerprint>` shape is mandatory; the agent verifies
        /// the controller CA fingerprint before enrolment.
        #[arg(long, env = "ISENGARD_ENROLL_TOKEN")]
        enroll_token: Option<String>,
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
    /// Mint enrollment tokens. Prefer the top-level `isengard
    /// join-token` shortcut.
    Token {
        #[command(subcommand)]
        op: TokenOp,
    },
    /// Revoke certs, list enrolled agents.
    Agent {
        #[command(subcommand)]
        op: AgentOp,
    },
    /// Export the controller's CA root cert PEM.
    Ca {
        #[command(subcommand)]
        op: CaOp,
    },
}

#[derive(Debug, Subcommand)]
enum TokenOp {
    /// Mint a one-time enrollment token. Prints a copy-pasteable
    /// `docker run` join block by default; `--format token` prints
    /// only the packed token; `--format joincmd` prints a single-line
    /// `isd join --controller ... --token ...` invocation.
    Mint {
        /// Role to mint for. Only "agent" is supported today.
        #[arg(long, default_value = "agent")]
        role: String,
        /// Token TTL (e.g. "15m", "1h", "24h").
        #[arg(long, default_value = "15m")]
        ttl: humantime::Duration,
        /// Public host:port for agents to dial. Falls back to
        /// $ISENGARD_PUBLIC_ADDR, then `controller.local:9417`.
        #[arg(long, env = "ISENGARD_PUBLIC_ADDR")]
        public_addr: Option<String>,
        /// Agent image reference to embed in the join command.
        #[arg(long, default_value = "ghcr.io/dirdmaster/isengard:next")]
        image: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = MintFormat::Text)]
        format: MintFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MintFormat {
    /// Copy-pasteable `docker run` join block (legacy, kept for one
    /// release while scripts migrate). Embeds the packed token so newer
    /// agents still verify the CA fingerprint.
    Text,
    /// Single-line packed token: `TK<base32(bytes)>.<base32(sha256(ca))>`.
    Token,
    /// Single-line `isd join --controller ... --token ...` invocation
    /// (recommended; what `isd join-token` prints).
    Joincmd,
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
        /// Secret name (matches `isd secret set`'s rules:
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

    let (mode, is_daemon) = match &cli.command {
        // Controller is a daemon only when no inner action is supplied; the
        // subcommands (`token mint`, `agent list`, `ca export`) are one-shot
        // CLI flows whose stdout is parsed by other tools.
        Command::Controller { action: None, .. } => ("controller", true),
        Command::Controller { .. } => ("controller", false),
        Command::Agent { .. } => ("agent", true),
        Command::Secret { .. } => ("secret", false),
        Command::JoinToken { .. } => ("join-token", false),
    };
    tracing_init::init(mode, cli.log.as_deref(), is_daemon);

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
        Command::JoinToken {
            role,
            state_dir,
            ttl,
            public_addr,
            image,
            format,
        } => run_token_mint(state_dir, role, ttl, public_addr, image, format).await,
        Command::Agent {
            controller,
            state_dir,
            enroll_token,
            advertise_iface,
        } => run_agent_mode(controller, state_dir, enroll_token, advertise_iface).await,
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
    }
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

    // Migration: earlier installs put the controller's
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
/// admin operations (mint / revoke / list). Auto-applies the
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

/// State-dir migration shim. Previously the systemd-native
/// install rooted the controller at `<state_dir>/controller/`; the
/// new layout drops the suffix so the CLI's `--state-dir /var/lib/isengard`
/// default matches the running unit.
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
            "controller state still at legacy `<state_dir>/controller/`; \
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

    // Decompose the legacy base32 bare token back into the raw
    // 32 bytes so we can pack a (bytes, ca_fingerprint) tuple for the
    // operator-visible string. Storage still keys on the legacy string
    // (sha256(token_b32)) so the round-trip via base32 keeps verify
    // logic unchanged when an agent presents the packed form.
    let token_bytes = decode_legacy_token_bytes(&token)?;
    let packed_token = isengard_core::join_token::pack(&token_bytes, ca_pem.as_bytes());

    match format {
        MintFormat::Token => {
            // Single-line packed token. Scripts that previously parsed the
            // bare base32 string need to migrate; the packed form is what
            // newer agents (and `isd join --token ...`) expect.
            println!("{packed_token}");
        }
        MintFormat::Joincmd => {
            // Default. One line, ready to paste on the Mac.
            let host_port = resolve_public_addr(public_addr.as_deref());
            println!("isd join --controller https://{host_port} --token {packed_token}");
        }
        MintFormat::Text => {
            // Legacy `docker run` join block. The embedded token is now the
            // packed form, so newer agents verify the CA fingerprint
            // even when the operator pasted the old-style block.
            let host_port = resolve_public_addr(public_addr.as_deref());
            let expires_at = minted_at + chrono_ttl;
            let block = render_join_command(JoinCommandArgs {
                ttl: ttl.to_string(),
                token: &packed_token,
                image: &image,
                public_addr: &host_port,
                expires_at,
            });
            println!("{block}");
        }
    }
    Ok(())
}

/// Decode the legacy base32 (RFC 4648 unpadded, uppercase) bare-token
/// string back to its raw 32 bytes. `EnrollmentService::mint` returns the
/// base32 form and persists `sha256(b32_string)`; the packed
/// shape needs the raw bytes so we can wrap them with the CA fingerprint
/// for the operator-visible string.
fn decode_legacy_token_bytes(token_b32: &str) -> Result<[u8; 32]> {
    let bytes_vec = data_encoding::BASE32_NOPAD
        .decode(token_b32.as_bytes())
        .map_err(|e| anyhow!("internal: mint returned unexpected token format; this is a bug, please report it ({e})"))?;
    bytes_vec.as_slice().try_into().map_err(|_| {
        anyhow!(
            "internal: mint returned {} bytes, expected 32; this is a bug, please report it",
            bytes_vec.len()
        )
    })
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
    out.push_str("      --name isd-agent \\\n");
    out.push_str("      --restart unless-stopped \\\n");
    out.push_str("      --platform linux/amd64 \\\n");
    out.push_str("      -v isd-agent-state:/var/lib/isengard \\\n");
    out.push_str("      -v /var/run/docker.sock:/var/run/docker.sock \\\n");
    out.push_str(&format!("      -e ISENGARD_ENROLL_TOKEN={} \\\n", a.token));
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
        bootstrap_trust: isengard_agent::enroll::BootstrapTrust::default(),
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
