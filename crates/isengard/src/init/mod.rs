//! Phase 0.11: `isengard init` subcommand. Linear, scrollback-friendly
//! install transcript. Polished `println!` + ANSI color + braille
//! spinners; no full-screen TUI (a one-shot installer that takes over
//! the screen vanishes from history once it exits).
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
//! both modes; the only difference is how [`Plan`] gets populated. The
//! visual chrome lives in [`ui`] and [`progress`] so the install logic
//! is not entangled with print formatting.

mod progress;
mod ui;

use std::fmt;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use inquire::{Confirm, Password, PasswordDisplayMode, Select, Text, validator::Validation};

/// One choice in a [`Select`] prompt. The `Display` impl renders as
/// `<label>  <detail>` in dim grey for `<detail>`, so picking from a
/// list with descriptions matches the mockup. inquire 0.7 has no
/// per-option help-text API, so we encode the description into the
/// `Display` output.
#[derive(Clone, Copy)]
struct SelectOption {
    /// Stable token returned to the caller (`production`, `staging`, ...).
    value: &'static str,
    /// Human label shown left of the description.
    label: &'static str,
    /// Dim explanatory text after the label.
    detail: &'static str,
}

impl fmt::Display for SelectOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Two-space gap separates the label from the dim detail. The
        // label is left-padded so options align in a column even when
        // labels differ in length.
        write!(
            f,
            "{:<26}  {}",
            self.label,
            console::style(self.detail).dim()
        )
    }
}

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
    /// operator can hit Enter through the safe path. Section headers,
    /// prompt labels, and help text are printed via [`ui`]; inquire's
    /// own widget message is kept short so the formatted label stays
    /// readable above the input cursor.
    fn interactive(args: &InitArgs, host_ip: String) -> Result<Self> {
        ui::section("Network identity");

        let acme_email = match &args.acme_email {
            Some(e) => e.clone(),
            None => {
                ui::prompt_header(
                    "ACME contact email",
                    Some(
                        "Used for Let's Encrypt expiry notifications. Blank for internal-only deploys.",
                    ),
                );
                Text::new("›")
                    .with_validator(|v: &str| {
                        if v.is_empty() || v.contains('@') {
                            Ok(Validation::Valid)
                        } else {
                            Ok(Validation::Invalid("must contain '@' or be blank".into()))
                        }
                    })
                    .prompt()
                    .map_err(|e| anyhow!("acme email prompt: {e}"))?
            }
        };

        let acme_domains = match &args.acme_domains {
            Some(d) => d.clone(),
            None => {
                ui::prompt_header(
                    "ACME domains (cert SANs)",
                    Some("Comma-separated, e.g. *.foo.com,foo.com. Wildcards require DNS-01."),
                );
                Text::new("›")
                    .with_default("")
                    .prompt()
                    .map_err(|e| anyhow!("acme domains prompt: {e}"))?
            }
        };

        let acme_directory = match &args.acme_directory {
            Some(d) => d.clone(),
            None => {
                ui::prompt_header("ACME directory", None);
                let opts = vec![
                    SelectOption {
                        value: "production",
                        label: "Let's Encrypt production",
                        detail: "real certs, requires DNS",
                    },
                    SelectOption {
                        value: "staging",
                        label: "Let's Encrypt staging",
                        detail: "test only",
                    },
                    SelectOption {
                        value: "custom",
                        label: "Custom URL",
                        detail: "self-hosted CA",
                    },
                ];
                let pick = Select::new("›", opts)
                    .with_starting_cursor(0)
                    .prompt()
                    .map_err(|e| anyhow!("acme directory prompt: {e}"))?;
                if pick.value == "custom" {
                    ui::prompt_header("Custom ACME directory URL", None);
                    Text::new("›")
                        .prompt()
                        .map_err(|e| anyhow!("custom acme url prompt: {e}"))?
                } else {
                    pick.value.to_string()
                }
            }
        };

        ui::section("Secrets");

        let cf_dns_token = if args.no_cf_dns_token {
            None
        } else {
            match &args.cf_dns_token {
                Some(t) => Some(t.clone()),
                None => {
                    ui::prompt_header(
                        "Set Cloudflare DNS API token?",
                        Some("Required for DNS-01 wildcard issuance. Stored encrypted."),
                    );
                    let want = Confirm::new("›")
                        .with_default(false)
                        .prompt()
                        .map_err(|e| anyhow!("cf token confirm: {e}"))?;
                    if want {
                        ui::prompt_header(
                            "Cloudflare DNS API token",
                            Some("Hidden input. Encrypted with the master key."),
                        );
                        let v = Password::new("›")
                            .with_display_mode(PasswordDisplayMode::Masked)
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
                    ui::prompt_header(
                        "Set a backup passphrase?",
                        Some("Used to encrypt automated snapshots. Stored encrypted."),
                    );
                    let want = Confirm::new("›")
                        .with_default(false)
                        .prompt()
                        .map_err(|e| anyhow!("backup confirm: {e}"))?;
                    if want {
                        ui::prompt_header(
                            "Backup passphrase",
                            Some("Hidden input. Encrypted with the master key."),
                        );
                        let v = Password::new("›")
                            .with_display_mode(PasswordDisplayMode::Masked)
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
            ui::prompt_header(
                &format!("Use detected host IP {host_ip} for cross-host join?"),
                Some("This IP is added to the controller cert SANs."),
            );
            let ok = Confirm::new("›")
                .with_default(true)
                .prompt()
                .map_err(|e| anyhow!("host ip confirm: {e}"))?;
            if ok {
                host_ip
            } else {
                ui::prompt_header("Host IP for join command", None);
                Text::new("›")
                    .with_default(&host_ip)
                    .prompt()
                    .map_err(|e| anyhow!("host ip prompt: {e}"))?
            }
        };

        let mut extra_sans = args.extra_sans.clone();
        if extra_sans.is_empty() {
            ui::prompt_header(
                "Extra SANs for the controller cert (optional)",
                Some(
                    "Comma-separated. localhost, 127.0.0.1, and detected host IP are added automatically.",
                ),
            );
            let raw = Text::new("›")
                .with_default("")
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
///
/// Order of operations:
///   1. Preflight (Linux + systemd + root)
///   2. TTY-vs-flags reconciliation (refuse to proceed when piped + no flags)
///   3. Banner + detected host
///   4. Apply the inquire theme (no-op on the non-interactive path)
///   5. Build a [`Plan`] (interactive prompts or pure flag parse)
///   6. [`Plan::execute`] runs the install with spinners + summary box
pub async fn run(args: InitArgs) -> Result<()> {
    preflight()?;

    // TTY fallback: when stdin is piped (e.g. `curl ... | bash`) and no
    // flags were supplied, prompts can't read keys. Bail with a hint
    // instead of failing inside an inquire prompt with a cryptic EOF.
    let stdin_is_tty = std::io::stdin().is_terminal();
    if !stdin_is_tty && !args.non_interactive && !has_required_flags(&args) {
        bail!(
            "isengard init has no TTY for prompts and no flags supplied. \
             Either run interactively (`isengard init` from a terminal) or pass \
             `--non-interactive` plus the required flags. See `isengard init --help`."
        );
    }

    let host_ip = match &args.host_ip {
        Some(ip) => ip.clone(),
        None => detect_host_ip().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "host ip auto-detect failed; falling back to 127.0.0.1 (pass --host-ip to override)");
            "127.0.0.1".to_string()
        }),
    };

    ui::banner();
    ui::detected(&ui::probe_host());

    if args.non_interactive_resolved() {
        ui::note("Using flag-driven config (non-interactive mode).");
        let plan = Plan::from_args(&args, host_ip)?;
        plan.execute().await
    } else {
        ui::install_inquire_theme();
        let plan = Plan::interactive(&args, host_ip)?;
        plan.execute().await
    }
}

/// True if the caller passed enough flags to drive a non-interactive
/// install without prompts. Conservative: we require at least the ACME
/// triple so a piped install doesn't silently fall through to defaults
/// the operator can't see.
fn has_required_flags(args: &InitArgs) -> bool {
    args.acme_email.is_some() && args.acme_directory.is_some()
        || args.no_cf_dns_token && args.no_backup
}

/// Display the first 8 chars of a token followed by an ellipsis, e.g.
/// `01KR9876…`. Used in the spinner-finalize line so the operator can
/// eyeball it without leaking the full token into shell history.
fn token_preview(token: &str) -> String {
    let prefix: String = token.chars().take(8).collect();
    format!("{prefix}…")
}

/// Display the first 12 chars of a hex fingerprint followed by an
/// ellipsis. 12 hex chars = 48 bits of entropy, enough for the
/// operator to recognize the right CA without printing the whole 64
/// chars (which would force a wrap inside the summary box).
fn short_fingerprint(fp: &str) -> String {
    let prefix: String = fp.chars().take(12).collect();
    format!("{prefix}…")
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
pub(super) fn detect_host_ip() -> Result<String> {
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
const ISO_CONTROLLER_UNIT: &str =
    include_str!("../../../../install/systemd/iso-controller.service");
const ISO_AGENT_UNIT: &str = include_str!("../../../../install/systemd/iso-agent.service");
const ISO_AGENT_TARGET: &str = include_str!("../../../../install/systemd/iso-agent.target");

impl Plan {
    /// Drive every step of the install: dirs, master key, secrets, units,
    /// service start, token mint, ca export, agent start, summary box.
    /// Each step renders via [`progress`] so the operator sees a live
    /// braille spinner while a step is in flight, then a static `✓`
    /// line once it completes (or `✗` on failure). After the install,
    /// only the static lines remain in scrollback.
    async fn execute(&self) -> Result<()> {
        ui::bootstrap_header();

        // Instant ops: dirs + master key + master-key.env + env file.
        self.setup_dirs()?;
        progress::instant_done(
            "Created directories",
            format!("{} + {}", self.state_dir.display(), self.etc_dir.display()),
        );

        self.setup_master_key()?;
        progress::instant_done(
            "Generated master key",
            self.master_key_file().display().to_string(),
        );

        self.write_master_key_env()?;
        progress::instant_done(
            "Wrote master-key.env",
            self.master_key_env_file().display().to_string(),
        );

        // Bootstrap secrets touches SQLite, but it's fast enough that a
        // spinner would only flash for one frame. Time it via the
        // wrapped step anyway so the timing label appears.
        let secret_count = self.cf_dns_token.iter().count() + self.backup_passphrase.iter().count();
        if secret_count == 0 {
            progress::instant_done("Encrypted secrets", "(none configured)");
        } else {
            let pb = progress::start("Encrypting secrets");
            match self.bootstrap_secrets().await {
                Ok(()) => pb.done(format!("{secret_count} secret(s) stored")),
                Err(e) => {
                    pb.fail(format!("{e:#}"));
                    return Err(e);
                }
            }
        }

        self.write_env_file()?;
        progress::instant_done("Wrote env file", self.env_file().display().to_string());

        self.install_systemd_units()?;
        progress::instant_done("Installed systemd units", "iso-controller, iso-agent");

        // Controller boot: spawn the unit, then poll for CA readiness.
        // The spinner stays animated while we wait; on success it
        // collapses to a single ✓ line.
        let pb = progress::start("Starting iso-controller");
        match self.start_controller_with_progress(&pb).await {
            Ok(()) => pb.done("https://localhost:9417"),
            Err(e) => {
                pb.fail(format!("{e:#}"));
                return Err(e);
            }
        }

        // CA export is fast; treat as instant.
        self.export_ca().await?;
        progress::instant_done("Exported CA cert", self.ca_pem_file().display().to_string());

        let token = {
            let pb = progress::start("Minting enrollment token");
            match self.mint_token().await {
                Ok(t) => {
                    let preview = token_preview(&t);
                    pb.done(format!("{preview} (15m TTL)"));
                    t
                }
                Err(e) => {
                    pb.fail(format!("{e:#}"));
                    return Err(e);
                }
            }
        };
        self.write_token_file(&token)?;

        let pb = progress::start("Starting iso-agent");
        match self.start_agent_with_progress() {
            Ok(()) => pb.done("enrolled"),
            Err(e) => {
                pb.fail(format!("{e:#}"));
                return Err(e);
            }
        }

        self.print_summary(&token);
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
        // The agent unit ships pointing at https://localhost:9417 (the
        // controller's cert now has localhost in its SANs). `join`
        // rewrites the ExecStart for cross-host setups.
        write_secret(
            &dir.join("iso-controller.service"),
            ISO_CONTROLLER_UNIT.as_bytes(),
            0o644,
        )?;
        write_secret(
            &dir.join("iso-agent.service"),
            ISO_AGENT_UNIT.as_bytes(),
            0o644,
        )?;
        write_secret(
            &dir.join("iso-agent.target"),
            ISO_AGENT_TARGET.as_bytes(),
            0o644,
        )?;
        run_systemctl(&["daemon-reload"])?;
        Ok(())
    }

    /// Boot the controller unit and poll for CA readiness. The caller
    /// owns the spinner; this just updates the per-attempt message
    /// (`Waiting for controller (3/30s)`) so the operator sees forward
    /// progress on slow first boots without us spamming new lines.
    async fn start_controller_with_progress(&self, pb: &progress::StepGuard) -> Result<()> {
        run_systemctl(&["enable", "iso-controller.service"]).ok();
        run_systemctl(&["start", "iso-controller.service"])?;
        pb.set_message("Waiting for controller to become ready…");

        // Poll for CA readiness (which doubles as a controller-up signal):
        // `Authority::load_or_init` succeeds once the boot path has run.
        let inv_dir = self.controller_state_dir();
        for i in 0..30u32 {
            let inv = match open_controller_inventory(&inv_dir).await {
                Ok(i) => i,
                Err(_) => {
                    pb.set_message(format!("Waiting for controller ({}/30s)…", i + 1));
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
            pb.set_message(format!("Waiting for controller ({}/30s)…", i + 1));
            tokio::time::sleep(Duration::from_secs(1)).await;
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

    /// Best-effort agent boot. The agent's gRPC enrollment is async; we
    /// don't poll for completion here (see [`Plan::execute`] for the
    /// summary box's verification hint).
    fn start_agent_with_progress(&self) -> Result<()> {
        run_systemctl(&["enable", "iso-agent.service"]).ok();
        run_systemctl(&["start", "iso-agent.service"])?;
        Ok(())
    }

    /// Render the final "Ready" summary box. Includes controller URLs,
    /// state paths, journalctl hint, and the cross-host join command
    /// (with the freshly-minted token + CA fingerprint pre-baked in).
    /// The box stays in scrollback so the operator can scroll up after
    /// the install completes.
    fn print_summary(&self, token: &str) {
        let host_ip = if self.host_ip.is_empty() {
            "<your-host-ip>".to_string()
        } else {
            self.host_ip.clone()
        };

        // Compute the CA fingerprint for the optional pin hint. Failure
        // is non-fatal: the join command falls back to pem-path pinning
        // (which always works since we just wrote the file).
        let ca_fingerprint = std::fs::read_to_string(self.ca_pem_file())
            .ok()
            .and_then(|pem| ca_sha256_fingerprint(&pem).ok());

        let mut lines: Vec<String> = Vec::new();
        // The box helper renders its own top/bottom padding rows; no
        // need for an explicit blank line here.
        lines.push(format!(
            "{:<13} {}",
            "Controller",
            console::style(format!("https://{host_ip}:9417")).bold()
        ));
        lines.push(format!(
            "{:<13} {}",
            "Dashboard",
            console::style("http://localhost:9418").bold()
        ));
        lines.push(format!(
            "{:<13} {}",
            "State",
            console::style(self.state_dir.display().to_string()).bold()
        ));
        lines.push(format!(
            "{:<13} {}",
            "Logs",
            console::style("journalctl -u iso-controller -u iso-agent").dim()
        ));
        lines.push(String::new());
        lines.push(console::style("Add another host:").bold().to_string());
        lines.push(String::new());
        // Pre-split the join command onto its own indented lines with
        // shell backslash continuations, matching the mockup. The wrap
        // helper in `summary_box` would otherwise break on the long
        // base32 token (which has no whitespace) and produce ragged
        // chunks that overflow the box. We default to `--ca-pem-path`
        // because the path is short and always fits on one line; the
        // computed fingerprint is shown below the join block as a
        // belt-and-braces pin the operator can append manually.
        lines.push(console::style("  isengard join \\").cyan().to_string());
        lines.push(
            console::style(format!("    --token {token} \\"))
                .cyan()
                .to_string(),
        );
        lines.push(
            console::style("    --ca-pem-path /etc/isengard/ca.pem \\")
                .cyan()
                .to_string(),
        );
        lines.push(
            console::style(format!("    https://{host_ip}:9417"))
                .cyan()
                .to_string(),
        );
        if let Some(fp) = &ca_fingerprint {
            // CA pin hint: short prefix only, dim. The full 64-char
            // hex shows up in `cat /etc/isengard/ca.pem | openssl ...`
            // so we don't need to print all of it; the prefix is
            // enough for the operator to verify the right CA was
            // provisioned.
            lines.push(String::new());
            lines.push(format!(
                "  {} sha256:{}",
                console::style("CA fingerprint:").dim(),
                console::style(short_fingerprint(fp)).dim(),
            ));
        }
        lines.push(String::new());
        lines.push(
            console::style("Next: deploy your first stack")
                .bold()
                .to_string(),
        );
        lines.push(
            console::style("  isd deploy --file ./compose.toml --stack hello")
                .cyan()
                .to_string(),
        );

        ui::summary_box("Ready", &lines);
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
        use std::io::Write;
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

/// Invoke `systemctl <args>` and capture its output instead of letting
/// it print directly. systemd's `enable` and `daemon-reload` paths
/// write `Created symlink …` to stderr, which interleaves with our
/// spinner+println chrome and breaks the layout. We swallow the chatter
/// on success and re-emit captured output on failure so debugging is
/// still possible.
fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("exec systemctl {args:?}"))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "systemctl {args:?} failed: {status}\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}",
            status = out.status,
        );
    }
    Ok(())
}

// =====================================================================
// `isengard join`: enrol this host as an agent against a remote
// controller. Strict subset of `init`: no master key, no controller
// boot, no secrets bootstrap. Just the agent.
// =====================================================================

/// Flags for `isengard join`. Required values: token + CA pin (path or
/// base64) + controller URL. Everything else falls back to the same
/// defaults `init` uses so a join host has the same on-disk layout.
#[derive(Debug, Args, Clone)]
pub struct JoinArgs {
    /// Controller URL, e.g. `https://controller.example.com:9417`.
    /// Positional so the join command reads naturally:
    /// `isengard join --token X --ca-pem-path Y https://...`.
    pub controller: String,
    /// One-time enrollment token minted on the controller via
    /// `isengard controller token mint --role agent`. Consumed on
    /// first agent start; subsequent restarts ignore it.
    #[arg(long)]
    pub token: String,
    /// Path to a PEM file holding the controller's CA root. Pinned for
    /// the bootstrap channel handshake. One of `--ca-pem-path` or
    /// `--ca-pem-base64` is required (they're mutually exclusive).
    #[arg(long, conflicts_with = "ca_pem_base64")]
    pub ca_pem_path: Option<PathBuf>,
    /// Base64-encoded controller CA PEM (single-line, shell-safe). The
    /// `controller token mint` join block emits this form. Decoded into
    /// `<etc>/ca.pem` at install time.
    #[arg(long)]
    pub ca_pem_base64: Option<String>,
    /// Optional sha256 fingerprint of the controller CA cert in
    /// `sha256:HEX` form. When set, the join flow verifies the supplied
    /// CA matches before writing it. Defense-in-depth pin.
    #[arg(long)]
    pub ca_fingerprint: Option<String>,
    /// State dir layout matches `init`. The agent state lives at
    /// `<state-dir>/agent`.
    #[arg(long, default_value = "/var/lib/isengard")]
    pub state_dir: PathBuf,
    #[arg(long, default_value = "/etc/isengard")]
    pub etc_dir: PathBuf,
    #[arg(long, default_value = "wisp")]
    pub runtime: String,
    #[arg(long, alias = "yes")]
    pub non_interactive: bool,
}

pub async fn run_join(args: JoinArgs) -> Result<()> {
    preflight()?;
    if args.ca_pem_path.is_none() && args.ca_pem_base64.is_none() {
        bail!(
            "missing CA pin: pass either --ca-pem-path <file> or --ca-pem-base64 <b64> (the controller's `token mint` output prints both)"
        );
    }

    println!("[1/8] preflight ok");

    // Layout
    std::fs::create_dir_all(&args.etc_dir).with_context(|| format!("create {:?}", args.etc_dir))?;
    let agent_state_dir = args.state_dir.join("agent");
    std::fs::create_dir_all(&agent_state_dir)
        .with_context(|| format!("create {agent_state_dir:?}"))?;
    std::fs::create_dir_all("/var/lib/wisp").context("create /var/lib/wisp")?;
    let _ = chmod(&args.etc_dir, 0o750);
    println!("[2/8] state + etc dirs ready");

    // Resolve + write CA pem
    let ca_pem = if let Some(path) = &args.ca_pem_path {
        std::fs::read_to_string(path).with_context(|| format!("read CA pem {path:?}"))?
    } else if let Some(b64) = &args.ca_pem_base64 {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .context("base64-decode --ca-pem-base64")?;
        String::from_utf8(decoded).context("--ca-pem-base64 not utf-8")?
    } else {
        unreachable!("checked above")
    };
    if !ca_pem.contains("BEGIN CERTIFICATE") {
        bail!("supplied CA does not contain a PEM certificate block");
    }

    if let Some(want) = &args.ca_fingerprint {
        let want = want.trim();
        let want = want
            .strip_prefix("sha256:")
            .or_else(|| want.strip_prefix("SHA256:"))
            .unwrap_or(want);
        let got = ca_sha256_fingerprint(&ca_pem)?;
        if !got.eq_ignore_ascii_case(want) {
            bail!(
                "CA fingerprint mismatch: supplied --ca-fingerprint={want} but computed sha256={got}"
            );
        }
    }

    let ca_path = args.etc_dir.join("ca.pem");
    write_secret(&ca_path, ca_pem.as_bytes(), 0o644)?;
    println!("[3/8] ca pinned at {ca_path:?}");

    // Token file
    let token_path = args.etc_dir.join("agent-token.env");
    let token_body = format!(
        "ISENGARD_ENROLL_TOKEN={}\n\
         ISENGARD_CONTROLLER_CA_PEM_PATH={}\n",
        args.token,
        ca_path.display()
    );
    write_secret(&token_path, token_body.as_bytes(), 0o600)?;
    println!("[4/8] enrollment token persisted");

    // Env file (controller URL, runtime). Written even on join so the
    // unit's EnvironmentFile picks up RUST_LOG and runtime backend.
    let env_path = args.etc_dir.join("isengard.env");
    let env_body = format!(
        "# Isengard agent-only join (Phase 0.10). Sourced by iso-agent.service.\n\
         ISENGARD_CONTROLLER={controller}\n\
         ISENGARD_RUNTIME={runtime}\n\
         RUST_LOG=info\n",
        controller = args.controller,
        runtime = args.runtime,
    );
    write_secret(&env_path, env_body.as_bytes(), 0o644)?;
    println!("[5/8] env file at {env_path:?}");

    // Install only the agent unit + target. The agent unit on disk
    // points at `https://localhost:9417`; rewrite ExecStart to use the
    // operator-supplied controller URL. Drop PartOf= and the
    // controller-side After= since there's no controller on this host.
    let dir = std::path::Path::new("/etc/systemd/system");
    std::fs::create_dir_all(dir).context("create /etc/systemd/system")?;
    let agent_unit = ISO_AGENT_UNIT
        .replace(
            "--controller https://localhost:9417",
            &format!("--controller {}", args.controller),
        )
        .replace("PartOf=iso-controller.service\n", "")
        .replace(
            "After=network-online.target iso-controller.service",
            "After=network-online.target",
        );
    write_secret(&dir.join("iso-agent.service"), agent_unit.as_bytes(), 0o644)?;
    run_systemctl(&["daemon-reload"])?;
    println!("[6/8] iso-agent.service installed");

    run_systemctl(&["enable", "iso-agent.service"]).ok();
    run_systemctl(&["start", "iso-agent.service"])?;
    println!("[7/8] iso-agent.service started");

    println!();
    println!("=================================================================");
    println!("Joined controller {}.", args.controller);
    println!();
    println!("Verify:  systemctl status iso-agent");
    println!("Logs:    journalctl -u iso-agent -f");
    println!("On the controller host, confirm enrollment:");
    println!(
        "  isengard controller --state-dir {} agent list",
        args.state_dir.join("controller").display()
    );
    println!("=================================================================");
    println!("[8/8] done");
    let _ = args.non_interactive; // reserved for future prompts
    Ok(())
}

/// Compute the lowercase-hex sha256 of the first PEM CERTIFICATE block in
/// `pem`. Used by `isengard join --ca-fingerprint` to defense-in-depth
/// pin the controller's CA against tampering between mint + paste.
fn ca_sha256_fingerprint(pem: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let der = pem_to_der(pem)?;
    let mut h = Sha256::new();
    h.update(&der);
    Ok(hex::encode(h.finalize()))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let begin = pem
        .find("-----BEGIN CERTIFICATE-----")
        .ok_or_else(|| anyhow!("no BEGIN CERTIFICATE in CA pem"))?;
    let after_begin = begin + "-----BEGIN CERTIFICATE-----".len();
    let end = pem
        .find("-----END CERTIFICATE-----")
        .ok_or_else(|| anyhow!("no END CERTIFICATE in CA pem"))?;
    let body = &pem[after_begin..end];
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .context("base64-decode CA cert body")
}
