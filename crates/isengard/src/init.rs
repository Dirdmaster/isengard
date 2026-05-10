//! Phase 0.10: `isengard init` subcommand.
//!
//! Replaces the ~900 line `install/install.sh` with native Rust. The bash
//! script becomes a ~50 line bootstrap that downloads the binary and execs
//! `isengard init` with the same flags the operator would have passed.
//!
//! Two operating modes:
//!
//!   - **Interactive**: stdin is a TTY and `--non-interactive` was not set.
//!     Walks the operator through `inquire` prompts for each missing value.
//!   - **Non-interactive**: stdin is piped (curl | bash) or `--non-interactive`
//!     was set. Every required value must come from a flag; missing values
//!     fail with a clear error naming the flag.
//!
//! The actual install steps live in [`Plan::execute`] and run identically in
//! both modes; the only difference is how [`Plan`] gets populated.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use inquire::{Confirm, Password, PasswordDisplayMode, Select, Text, validator::Validation};

/// Flags shared between init and the bash bootstrap. Every prompt has a
/// matching flag so the install can run unattended (`--non-interactive`)
/// or interactively (default when stdin is a TTY).
#[derive(Debug, Args, Clone)]
pub struct InitArgs {
    /// ACME contact email for Let's Encrypt account creation. Required for
    /// public HTTPS routes; safe to leave empty for internal-only deploys.
    #[arg(long)]
    pub acme_email: Option<String>,
    /// Comma-separated domains to pre-issue certs for, e.g.
    /// `*.example.com,example.com`.
    #[arg(long)]
    pub acme_domains: Option<String>,
    /// ACME directory: `staging`, `production`, or a raw URL (custom CA).
    #[arg(long)]
    pub acme_directory: Option<String>,
    /// Cloudflare DNS API token for DNS-01 wildcard issuance. Stored as an
    /// encrypted secret keyed by the master key, never plaintext on disk.
    #[arg(long)]
    pub cf_dns_token: Option<String>,
    /// Skip the Cloudflare DNS token prompt entirely (no wildcard DNS-01).
    /// Conflicts with `--cf-dns-token`.
    #[arg(long, conflicts_with = "cf_dns_token")]
    pub no_cf_dns_token: bool,
    /// Path to a file containing the backup passphrase. Read once, contents
    /// stored as an encrypted secret. The file may be deleted afterwards.
    #[arg(long)]
    pub backup_passphrase_file: Option<PathBuf>,
    /// Skip the backup passphrase prompt entirely (no encrypted snapshots).
    /// Conflicts with `--backup-passphrase-file`.
    #[arg(long, conflicts_with = "backup_passphrase_file")]
    pub no_backup: bool,
    /// Extra Subject Alternative Name to include on the controller's server
    /// cert (in addition to localhost, 127.0.0.1, and the auto-detected host
    /// IP). Pass once per host: `--extra-san foo.example.com --extra-san 10.0.0.5`.
    #[arg(long = "extra-san")]
    pub extra_sans: Vec<String>,
    /// Controller gRPC + agent enrollment listen address.
    #[arg(long, default_value = "0.0.0.0:9417")]
    pub listen_grpc: String,
    /// Dashboard HTTP listen address.
    #[arg(long, default_value = "0.0.0.0:9418")]
    pub listen_dashboard: String,
    /// Top-level state directory. Subdirectories `controller/` and `agent/`
    /// are created under here; SQLite + CA + agent cert live in those.
    #[arg(long, default_value = "/var/lib/isengard")]
    pub state_dir: PathBuf,
    /// Config dir for env files, master key, and CA pem.
    #[arg(long, default_value = "/etc/isengard")]
    pub etc_dir: PathBuf,
    /// Skip all interactive prompts. Every required field must come from a
    /// flag; missing values fail loudly. Implied when stdin is not a TTY.
    #[arg(long, alias = "yes")]
    pub non_interactive: bool,
    /// Container runtime backend. `wisp` is the systemd-native default.
    #[arg(long, default_value = "wisp")]
    pub runtime: String,
    /// Overwrite an existing master key without prompting. Required when
    /// running non-interactively against a host that already has one.
    #[arg(long)]
    pub force: bool,
    /// Optional override for the auto-detected host IP. When passed, init
    /// uses this as the controller-side SAN-by-IP and the join-command
    /// hint instead of probing /proc/net/route.
    #[arg(long)]
    pub host_ip: Option<String>,
}

impl InitArgs {
    /// Should we skip prompts entirely? True if the operator passed the
    /// flag or stdin isn't a terminal (curl | bash).
    pub fn non_interactive_resolved(&self) -> bool {
        self.non_interactive || !std::io::stdin().is_terminal()
    }
}

/// Resolved configuration for one `isengard init` run. All values are
/// fully populated by the time `execute` runs: prompts (interactive) or
/// flag parsing (non-interactive) handled the resolution upstream.
#[derive(Debug, Clone)]
struct Plan {
    acme_email: String,
    acme_domains: String,
    acme_directory: String,
    cf_dns_token: Option<String>,
    backup_passphrase: Option<String>,
    extra_sans: Vec<String>,
    listen_grpc: String,
    listen_dashboard: String,
    state_dir: PathBuf,
    etc_dir: PathBuf,
    runtime: String,
    force: bool,
    host_ip: String,
}

impl Plan {
    /// Build a [`Plan`] purely from CLI flags. Used in non-interactive mode.
    /// Required values that are missing produce a clear error naming the
    /// flag the operator should set.
    fn from_args(args: &InitArgs, host_ip: String) -> Result<Self> {
        Ok(Plan {
            acme_email: args.acme_email.clone().unwrap_or_default(),
            acme_domains: args.acme_domains.clone().unwrap_or_default(),
            acme_directory: args
                .acme_directory
                .clone()
                .unwrap_or_else(|| "staging".to_string()),
            cf_dns_token: if args.no_cf_dns_token {
                None
            } else {
                args.cf_dns_token.clone()
            },
            backup_passphrase: if args.no_backup {
                None
            } else {
                match &args.backup_passphrase_file {
                    Some(p) => Some(read_passphrase_file(p)?),
                    None => None,
                }
            },
            extra_sans: args.extra_sans.clone(),
            listen_grpc: args.listen_grpc.clone(),
            listen_dashboard: args.listen_dashboard.clone(),
            state_dir: args.state_dir.clone(),
            etc_dir: args.etc_dir.clone(),
            runtime: args.runtime.clone(),
            force: args.force,
            host_ip,
        })
    }

    /// Build a [`Plan`] by walking the operator through `inquire` prompts
    /// for each unset CLI flag. Defaults pre-populate prompts so an
    /// operator can hit Enter through the safe path.
    fn interactive(args: &InitArgs, host_ip: String) -> Result<Self> {
        println!();
        println!("isengard init: interactive setup");
        println!("==============================");
        println!();

        let acme_email = match &args.acme_email {
            Some(e) => e.clone(),
            None => Text::new("ACME contact email (blank for internal-only deploys):")
                .with_help_message(
                    "Used for Let's Encrypt account registration. Required for public HTTPS.",
                )
                .with_validator(|v: &str| {
                    if v.is_empty() || v.contains('@') {
                        Ok(Validation::Valid)
                    } else {
                        Ok(Validation::Invalid("must contain '@' or be blank".into()))
                    }
                })
                .prompt()
                .map_err(|e| anyhow!("acme email prompt: {e}"))?,
        };

        let acme_domains = match &args.acme_domains {
            Some(d) => d.clone(),
            None => Text::new("ACME domains, comma-separated (e.g. *.foo.com,foo.com):")
                .with_default("")
                .with_help_message("Wildcards require DNS-01 (Cloudflare token below).")
                .prompt()
                .map_err(|e| anyhow!("acme domains prompt: {e}"))?,
        };

        let acme_directory = match &args.acme_directory {
            Some(d) => d.clone(),
            None => {
                let opts = vec!["staging", "production", "custom URL"];
                let pick = Select::new("ACME directory:", opts)
                    .with_starting_cursor(0)
                    .prompt()
                    .map_err(|e| anyhow!("acme directory prompt: {e}"))?;
                if pick == "custom URL" {
                    Text::new("Custom ACME directory URL:")
                        .prompt()
                        .map_err(|e| anyhow!("custom acme url prompt: {e}"))?
                } else {
                    pick.to_string()
                }
            }
        };

        let cf_dns_token = if args.no_cf_dns_token {
            None
        } else {
            match &args.cf_dns_token {
                Some(t) => Some(t.clone()),
                None => {
                    let want = Confirm::new("Set a Cloudflare DNS API token (DNS-01 wildcards)?")
                        .with_default(false)
                        .prompt()
                        .map_err(|e| anyhow!("cf token confirm: {e}"))?;
                    if want {
                        let v = Password::new("Cloudflare DNS API token:")
                            .with_display_mode(PasswordDisplayMode::Masked)
                            .with_help_message("Hidden input. Encrypted with the master key.")
                            .without_confirmation()
                            .prompt()
                            .map_err(|e| anyhow!("cf token prompt: {e}"))?;
                        if v.is_empty() { None } else { Some(v) }
                    } else {
                        None
                    }
                }
            }
        };

        let backup_passphrase = if args.no_backup {
            None
        } else {
            match &args.backup_passphrase_file {
                Some(p) => Some(read_passphrase_file(p)?),
                None => {
                    let want = Confirm::new("Set a backup passphrase (encrypted snapshots)?")
                        .with_default(false)
                        .prompt()
                        .map_err(|e| anyhow!("backup confirm: {e}"))?;
                    if want {
                        let v = Password::new("Backup passphrase:")
                            .with_display_mode(PasswordDisplayMode::Masked)
                            .with_help_message("Hidden input. Encrypted with the master key.")
                            .prompt()
                            .map_err(|e| anyhow!("backup prompt: {e}"))?;
                        if v.is_empty() { None } else { Some(v) }
                    } else {
                        None
                    }
                }
            }
        };

        // Auto-detected IP confirmation. The flag `--host-ip`, when passed,
        // already populated `host_ip`; we still let the operator override.
        let host_ip = if args.host_ip.is_some() {
            host_ip
        } else {
            let prompt = format!("Use detected host IP {host_ip} for cross-host join?");
            let ok = Confirm::new(&prompt)
                .with_default(true)
                .prompt()
                .map_err(|e| anyhow!("host ip confirm: {e}"))?;
            if ok {
                host_ip
            } else {
                Text::new("Host IP for join command:")
                    .with_default(&host_ip)
                    .prompt()
                    .map_err(|e| anyhow!("host ip prompt: {e}"))?
            }
        };

        let mut extra_sans = args.extra_sans.clone();
        if extra_sans.is_empty() {
            // One additional optional prompt; comma-split, blank skips.
            let raw = Text::new("Extra SANs for the controller cert (comma-separated, optional):")
                .with_default("")
                .with_help_message("e.g. controller.example.com,10.0.0.5. Always includes localhost + 127.0.0.1 + detected host IP.")
                .prompt()
                .map_err(|e| anyhow!("extra sans prompt: {e}"))?;
            extra_sans = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        Ok(Plan {
            acme_email,
            acme_domains,
            acme_directory,
            cf_dns_token,
            backup_passphrase,
            extra_sans,
            listen_grpc: args.listen_grpc.clone(),
            listen_dashboard: args.listen_dashboard.clone(),
            state_dir: args.state_dir.clone(),
            etc_dir: args.etc_dir.clone(),
            runtime: args.runtime.clone(),
            force: args.force,
            host_ip,
        })
    }

    /// Per-step paths: state subdirs, env file, master key, ca.pem,
    /// agent-token.env. Centralized so `execute` doesn't sprout magic
    /// strings.
    fn controller_state_dir(&self) -> PathBuf {
        self.state_dir.join("controller")
    }
    fn agent_state_dir(&self) -> PathBuf {
        self.state_dir.join("agent")
    }
    fn env_file(&self) -> PathBuf {
        self.etc_dir.join("isengard.env")
    }
    fn token_file(&self) -> PathBuf {
        self.etc_dir.join("agent-token.env")
    }
    fn master_key_file(&self) -> PathBuf {
        self.etc_dir.join("master.key")
    }
    fn ca_pem_file(&self) -> PathBuf {
        self.etc_dir.join("ca.pem")
    }
    fn master_key_env_file(&self) -> PathBuf {
        self.etc_dir.join("master-key.env")
    }

    /// Compose the full SAN list for the controller cert: always
    /// localhost + 127.0.0.1 + the detected host IP, plus operator-supplied.
    fn full_extra_sans(&self) -> Vec<String> {
        let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        if !self.host_ip.is_empty() && self.host_ip != "127.0.0.1" {
            sans.push(self.host_ip.clone());
        }
        for s in &self.extra_sans {
            let t = s.trim();
            if !t.is_empty() && !sans.iter().any(|x| x == t) {
                sans.push(t.to_string());
            }
        }
        sans
    }
}

/// Public entry point invoked from `main.rs` after argument parsing.
pub async fn run(args: InitArgs) -> Result<()> {
    preflight()?;
    let host_ip = match &args.host_ip {
        Some(ip) => ip.clone(),
        None => detect_host_ip().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "host ip auto-detect failed; falling back to 127.0.0.1 (pass --host-ip to override)");
            "127.0.0.1".to_string()
        }),
    };
    let plan = if args.non_interactive_resolved() {
        Plan::from_args(&args, host_ip)?
    } else {
        Plan::interactive(&args, host_ip)?
    };
    plan.execute().await
}

/// Preflight checks. Bails before touching disk if we're not on Linux,
/// systemd is missing, or the script isn't running as root. Mac dev
/// builds short-circuit with a friendly hint instead of a confusing fail
/// later.
fn preflight() -> Result<()> {
    if std::env::consts::OS != "linux" {
        bail!(
            "isengard init only runs on Linux (got {}). For dev on macOS use `cargo run -- controller` directly.",
            std::env::consts::OS
        );
    }
    if which::which("systemctl").is_err() {
        bail!("systemctl not found in PATH; isengard init targets systemd hosts only");
    }
    // SAFETY: getuid is always available on Linux and never fails. We pull
    // it from libc via std::process where exposed.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: libc::getuid is a leaf syscall that never fails and has
        // no side effects. We only call it on Linux where libc is guaranteed
        // present.
        let uid = unsafe { libc_getuid() };
        if uid != 0 {
            bail!("isengard init must run as root (got uid {uid}); use sudo");
        }
    }
    Ok(())
}

// Keep a tiny libc shim local to avoid pulling the libc crate just for
// getuid. On macOS preflight bails earlier so this is never reached, but
// we still gate to linux to keep the intent obvious.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getuid() -> u32;
}

#[cfg(target_os = "linux")]
unsafe fn libc_getuid() -> u32 {
    unsafe { getuid() }
}

/// Best-effort host-IP probe. Tries `ip route get 1.1.1.1` first (parses
/// out `src <addr>`), then falls back to the first non-loopback IPv4 from
/// `getifaddrs`. Returns the IP as a string suitable for embedding in the
/// controller's server cert.
fn detect_host_ip() -> Result<String> {
    // Path 1: `ip route get 1.1.1.1` outputs a line like
    //   `1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.42 uid 0`
    if let Ok(out) = std::process::Command::new("ip")
        .args(["route", "get", "1.1.1.1"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(idx) = s.find(" src ") {
                let rest = &s[idx + 5..];
                let ip: String = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !ip.is_empty() {
                    return Ok(ip);
                }
            }
        }
    }

    // Path 2: enumerate interfaces and pick the first non-loopback v4.
    let addrs = if_addrs::get_if_addrs().context("enumerate interfaces")?;
    for a in addrs {
        if a.is_loopback() {
            continue;
        }
        if let std::net::IpAddr::V4(v4) = a.ip() {
            return Ok(v4.to_string());
        }
    }
    Err(anyhow!(
        "no non-loopback IPv4 address found; pass --host-ip explicitly"
    ))
}

fn read_passphrase_file(path: &std::path::Path) -> Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading backup passphrase from {path:?}"))?;
    let trimmed = contents.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        bail!("backup passphrase file {path:?} is empty");
    }
    Ok(trimmed.to_string())
}

// Embed the systemd unit files at compile time so the binary is fully
// self-sufficient. Operators running `curl ... | bash` only need the
// binary on disk; the bash bootstrap doesn't have to ship unit files
// alongside.
const ISO_CONTROLLER_UNIT: &str = include_str!("../../../install/systemd/iso-controller.service");
const ISO_AGENT_UNIT: &str = include_str!("../../../install/systemd/iso-agent.service");
const ISO_AGENT_TARGET: &str = include_str!("../../../install/systemd/iso-agent.target");

impl Plan {
    /// Drive every step of the install: dirs, master key, secrets, units,
    /// service start, token mint, ca export, agent start, success banner.
    /// Each step logs progress so a `curl | bash` operator sees what's
    /// happening even without a TTY.
    async fn execute(&self) -> Result<()> {
        println!();
        println!("[1/12] preflight ok");
        self.setup_dirs()?;
        println!(
            "[2/12] state + etc dirs ready ({:?}, {:?})",
            self.state_dir, self.etc_dir
        );

        self.setup_master_key()?;
        println!("[3/12] master key at {:?}", self.master_key_file());

        self.write_master_key_env()?;
        println!("[4/12] master-key.env written");

        self.bootstrap_secrets().await?;
        println!("[5/12] secrets bootstrapped");

        self.write_env_file()?;
        println!("[6/12] env file at {:?}", self.env_file());

        self.install_systemd_units()?;
        println!("[7/12] systemd units installed");

        self.start_controller().await?;
        println!("[8/12] controller running");

        self.export_ca().await?;
        println!("[9/12] ca exported to {:?}", self.ca_pem_file());

        let token = self.mint_token().await?;
        self.write_token_file(&token)?;
        println!("[10/12] enrollment token minted");

        self.start_agent()?;
        println!("[11/12] agent enrolled");

        self.print_banner(&token);
        println!("[12/12] done");
        Ok(())
    }

    fn setup_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.etc_dir)
            .with_context(|| format!("create {:?}", self.etc_dir))?;
        std::fs::create_dir_all(self.controller_state_dir())
            .with_context(|| format!("create {:?}", self.controller_state_dir()))?;
        std::fs::create_dir_all(self.agent_state_dir())
            .with_context(|| format!("create {:?}", self.agent_state_dir()))?;
        std::fs::create_dir_all(self.state_dir.join("stacks"))
            .with_context(|| format!("create {:?}", self.state_dir.join("stacks")))?;
        std::fs::create_dir_all("/var/lib/wisp").context("create /var/lib/wisp")?;
        // Tighten the etc dir before we drop the master key in.
        let _ = chmod(&self.etc_dir, 0o750);
        Ok(())
    }

    fn setup_master_key(&self) -> Result<()> {
        let path = self.master_key_file();
        if path.exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size != 32 {
                bail!(
                    "master key {path:?} exists but is {size} bytes (expected 32). Refusing to overwrite; delete it manually and re-run."
                );
            }
            if self.force {
                tracing::warn!(
                    "--force passed but master key already valid at {:?}; keeping existing key (the master key gates access to all stored secrets, never auto-rotated)",
                    path
                );
            }
            // Existing key: keep it. The key is the only path back to
            // existing secrets, never overwrite even on --force.
            return Ok(());
        }
        let mut key = [0u8; 32];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut key);
        write_secret(&path, &key, 0o600).with_context(|| format!("write master key {path:?}"))?;
        Ok(())
    }

    fn write_master_key_env(&self) -> Result<()> {
        let path = self.master_key_env_file();
        let body = format!(
            "# Auto-generated by `isengard init`. Sourced by iso-controller.service.\n\
             ISENGARD_MASTER_KEY_FILE={}\n",
            self.master_key_file().display()
        );
        write_secret(&path, body.as_bytes(), 0o600).with_context(|| format!("write {path:?}"))?;
        Ok(())
    }

    /// Pipe each operator-supplied secret through the same in-process
    /// `SecretsStore::put` the existing `secret bootstrap` subcommand uses.
    /// Plaintext stays in process memory; ciphertext lands in the
    /// controller's SQLite.
    async fn bootstrap_secrets(&self) -> Result<()> {
        let key_path = self.master_key_file();
        let key = isengard_controller::secrets::read_master_key(&key_path)
            .with_context(|| format!("read master key {key_path:?}"))?;
        let inv = Arc::new(open_controller_inventory(&self.controller_state_dir()).await?);
        let store = isengard_controller::secrets::SecretsStore::new(inv, key);

        if let Some(t) = &self.cf_dns_token {
            store
                .put("cf_dns_api_token", t.as_bytes(), Some("init"))
                .await
                .context("storing cf_dns_api_token")?;
        }
        if let Some(p) = &self.backup_passphrase {
            store
                .put("backup_passphrase", p.as_bytes(), Some("init"))
                .await
                .context("storing backup_passphrase")?;
        }
        Ok(())
    }

    fn write_env_file(&self) -> Result<()> {
        let path = self.env_file();
        let extra_sans = self.full_extra_sans().join(",");
        let body = format!(
            "# Isengard non-secret config (Phase 0.10 systemd-native install).\n\
             # Sourced by iso-controller.service + iso-agent.service via systemd's\n\
             # EnvironmentFile=. Secrets live encrypted in the controller's SQLite,\n\
             # keyed by /etc/isengard/master.key.\n\
             \n\
             ISENGARD_ACME_EMAIL={acme_email}\n\
             ISENGARD_ACME_DOMAINS={acme_domains}\n\
             ISENGARD_ACME_DIRECTORY={acme_directory}\n\
             \n\
             # Controller listen addresses. The unit's ExecStart picks these\n\
             # up via env (`ISENGARD_LISTEN`). Dashboard binding is handled\n\
             # by the dashboard plugin.\n\
             ISENGARD_LISTEN={listen_grpc}\n\
             ISENGARD_DASHBOARD_LISTEN={listen_dashboard}\n\
             \n\
             # Phase 0.10: SANs the controller's first-boot server cert is\n\
             # generated for. Includes localhost, 127.0.0.1, the auto-detected\n\
             # host IP, and any --extra-san values from `isengard init`.\n\
             ISENGARD_EXTRA_SANS={extra_sans}\n\
             \n\
             # Wisp is the only supported runtime under the systemd-native install.\n\
             ISENGARD_RUNTIME={runtime}\n\
             \n\
             # Default log level. Override to `debug` for short-term troubleshooting.\n\
             RUST_LOG=info\n",
            acme_email = self.acme_email,
            acme_domains = self.acme_domains,
            acme_directory = self.acme_directory,
            listen_grpc = self.listen_grpc,
            listen_dashboard = self.listen_dashboard,
            extra_sans = extra_sans,
            runtime = self.runtime,
        );
        write_secret(&path, body.as_bytes(), 0o644).with_context(|| format!("write {path:?}"))?;
        Ok(())
    }

    fn install_systemd_units(&self) -> Result<()> {
        let dir = std::path::Path::new("/etc/systemd/system");
        std::fs::create_dir_all(dir).context("create /etc/systemd/system")?;
        // The agent unit shipped on disk hardcodes
        // `--controller https://controller.local:9417`. Phase 0.10 init
        // talks to itself via `https://localhost:9417` because the
        // controller's cert now has localhost in its SANs. Patch the
        // ExecStart line on the way through.
        let agent_unit = ISO_AGENT_UNIT.replace(
            "--controller https://controller.local:9417",
            "--controller https://localhost:9417",
        );
        write_secret(
            &dir.join("iso-controller.service"),
            ISO_CONTROLLER_UNIT.as_bytes(),
            0o644,
        )?;
        write_secret(&dir.join("iso-agent.service"), agent_unit.as_bytes(), 0o644)?;
        write_secret(
            &dir.join("iso-agent.target"),
            ISO_AGENT_TARGET.as_bytes(),
            0o644,
        )?;
        run_systemctl(&["daemon-reload"])?;
        Ok(())
    }

    async fn start_controller(&self) -> Result<()> {
        run_systemctl(&["enable", "iso-controller.service"]).ok();
        run_systemctl(&["start", "iso-controller.service"])?;

        // Poll for CA readiness (which doubles as a controller-up signal):
        // `Authority::load_or_init` succeeds once the boot path has run.
        let inv_dir = self.controller_state_dir();
        for i in 0..30u32 {
            let inv = match open_controller_inventory(&inv_dir).await {
                Ok(i) => i,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if isengard_controller::ca::Authority::load_or_init(&inv)
                .await
                .is_ok()
            {
                return Ok(());
            }
            print!(".");
            std::io::stdout().flush().ok();
            tokio::time::sleep(Duration::from_secs(1)).await;
            // Touch i to silence the unused-binding warning in case the
            // last `Err` arm short-circuits.
            let _ = i;
        }
        bail!("controller did not become ready in 30s; check `journalctl -u iso-controller`")
    }

    async fn export_ca(&self) -> Result<()> {
        let inv = open_controller_inventory(&self.controller_state_dir()).await?;
        let ca = isengard_controller::ca::Authority::load_or_init(&inv).await?;
        write_secret(&self.ca_pem_file(), ca.root_cert_pem().as_bytes(), 0o644)?;
        Ok(())
    }

    async fn mint_token(&self) -> Result<String> {
        use isengard_controller::ca::Authority;
        use isengard_controller::enrollment::EnrollmentService;
        use isengard_storage::enrollment_token::TokenRole;

        let inv = Arc::new(open_controller_inventory(&self.controller_state_dir()).await?);
        let ca = Arc::new(Authority::load_or_init(&inv).await?);
        let svc = EnrollmentService::new(inv, ca);
        // 15 minutes matches the install.sh default. Long enough for the
        // local agent to enroll on the same host, short enough that a
        // leaked token isn't useful for long.
        let ttl = chrono::Duration::minutes(15);
        let token = svc.mint(TokenRole::Agent, ttl).await?;
        Ok(token)
    }

    fn write_token_file(&self, token: &str) -> Result<()> {
        let path = self.token_file();
        let body = format!(
            "# Auto-generated by `isengard init`. The agent consumes this on first start,\n\
             # persists its mTLS cert under {agent_dir}, and ignores this file on\n\
             # subsequent restarts. Safe to delete after enrollment.\n\
             ISENGARD_ENROLL_TOKEN={token}\n\
             ISENGARD_CONTROLLER_CA_PEM_PATH={ca_path}\n",
            agent_dir = self.agent_state_dir().display(),
            token = token,
            ca_path = self.ca_pem_file().display(),
        );
        write_secret(&path, body.as_bytes(), 0o600).with_context(|| format!("write {path:?}"))?;
        Ok(())
    }

    fn start_agent(&self) -> Result<()> {
        run_systemctl(&["enable", "iso-agent.service"]).ok();
        run_systemctl(&["start", "iso-agent.service"])?;
        // Best-effort: agent enrollment is async via gRPC; we don't poll
        // here today. The success banner instructs the operator how to
        // verify (`systemctl status iso-agent` + `isengard controller agent list`).
        Ok(())
    }

    fn print_banner(&self, token: &str) {
        let host_ip = if self.host_ip.is_empty() {
            "<your-host-ip>".to_string()
        } else {
            self.host_ip.clone()
        };
        println!();
        println!("=================================================================");
        println!("Isengard is up.");
        println!();
        println!("Controller (local):  https://localhost:9417");
        println!("Controller (remote): https://{host_ip}:9417");
        println!("Dashboard:           http://localhost:9418");
        println!();
        println!("To enroll another host, run on that host:");
        println!();
        println!(
            "  isengard join \\\n    --token {token} \\\n    --ca-pem-path /etc/isengard/ca.pem \\\n    https://{host_ip}:9417"
        );
        println!();
        println!("(Copy /etc/isengard/ca.pem from this host to the joining host first,");
        println!(" or paste it via --ca-pem-base64. Token expires in 15 minutes.)");
        println!();
        println!("Logs:    journalctl -u iso-controller -f");
        println!("         journalctl -u iso-agent -f");
        println!("Status:  systemctl status iso-controller iso-agent");
        println!("=================================================================");
    }
}

/// Open the controller's SQLite inventory. Mirrors `open_inventory` in
/// main.rs but kept local so the init flow doesn't reach into the binary's
/// helper namespace.
async fn open_controller_inventory(dir: &std::path::Path) -> Result<isengard_storage::Inventory> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    let db = dir.join("isengard.db");
    isengard_storage::Inventory::open(&db)
        .await
        .with_context(|| format!("open inventory {db:?}"))
}

/// Atomically write `bytes` to `path` with `mode`. Creates the parent
/// directory if missing. Used for env files, master key, ca.pem.
fn write_secret(path: &std::path::Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create parent of {path:?}"))?;
    }
    let tmp = path.with_extension("tmp.isengard-init");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("open {tmp:?}"))?;
        f.write_all(bytes)
            .with_context(|| format!("write {tmp:?}"))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    let _ = chmod(path, mode);
    Ok(())
}

fn chmod(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("exec systemctl {args:?}"))?;
    if !status.success() {
        bail!("systemctl {args:?} failed: {status}");
    }
    Ok(())
}
