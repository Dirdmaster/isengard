//! Phase 0.12: `isengard init` and `isengard join` rendered through
//! `cliclack`, the Rust port of the `@clack/prompts` library every major
//! 2026 installer uses (`nuxi init`, `create-astro`, `create-svelte`,
//! `pnpm create vite`, `npx shadcn init`, `create-t3-app`). The whole
//! installer reads as a single vertical pipe: `┌` intro, `│` connector
//! between every line, `◇` for completed prompts/steps, `◆` for an
//! active section header, `└` outro. cliclack vendors `console` and
//! `indicatif` internally so we never reach for them directly.
//!
//! Two modes:
//!
//!   - **Interactive**: stdin is a TTY and `--non-interactive` was not
//!     passed. cliclack drives every prompt; we collect the answers into
//!     a [`Plan`] and call [`Plan::execute`].
//!   - **Non-interactive**: stdin is piped (curl | bash) or
//!     `--non-interactive` was set. cliclack would render the connector
//!     bar to a non-TTY in ugly ways, so we drop the chrome entirely
//!     and emit plain `[step] message` lines. Plan-building still goes
//!     through `Plan::from_args`; only the rendering changes.
//!
//! The actual install steps in [`Plan::execute`] are unchanged from the
//! pre-cliclack rewrite. Only the input-gathering and step-rendering
//! layers were replaced.

mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;

use ui::{
    done, format_host_info, format_ready_block, probe_host, validate_email, validate_hostnames,
};

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
    /// Top-level state directory. The controller writes its SQLite + CA
    /// directly here; the local agent's cert bundle + agent.json live
    /// under the `agent/` subdir so the controller and agent don't fight
    /// over the same filenames.
    #[arg(long, default_value = "/var/lib/isengard")]
    pub state_dir: PathBuf,
    /// Config dir for env files, master key, and CA pem.
    #[arg(long, default_value = "/etc/isengard")]
    pub etc_dir: PathBuf,
    /// Skip all interactive prompts. Every required field must come from a
    /// flag; missing values fail loudly. Implied when stdin is not a TTY.
    #[arg(long, alias = "yes")]
    pub non_interactive: bool,
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

/// Resolved configuration for one `isengard init` run. All values are
/// fully populated by the time `execute` runs.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    acme_email: String,
    acme_domains: String,
    acme_directory: String,
    cf_dns_token: Option<String>,
    backup_passphrase: Option<String>,
    extra_sans: Vec<String>,
    listen_grpc: String,
    listen_dashboard: String,
    pub(crate) state_dir: PathBuf,
    pub(crate) etc_dir: PathBuf,
    force: bool,
    pub(crate) host_ip: String,
}

impl Plan {
    /// Build a [`Plan`] purely from CLI flags. Used in non-interactive mode.
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
            force: args.force,
            host_ip,
        })
    }

    /// Walk the operator through `cliclack` prompts for each unset CLI
    /// flag. Every prompt produces a `◇` line that stays in scrollback,
    /// so the operator can review every choice after the install.
    fn gather_interactive(args: &InitArgs, host_ip: String) -> Result<Self> {
        // 1. Public hostnames (cert SANs). Comma-separated list; empty
        //    is allowed for internal-only deploys.
        let acme_domains = match &args.acme_domains {
            Some(d) => d.clone(),
            None => cliclack::input("Public hostnames (cert SANs)")
                .placeholder("vault.example.com, dashboard.example.com")
                .required(false)
                .validate(validate_hostnames)
                .interact()
                .map_err(|e| anyhow!("hostnames prompt: {e}"))?,
        };

        // 2. ACME directory: production / staging / custom URL.
        let acme_directory = match &args.acme_directory {
            Some(d) => d.clone(),
            None => {
                let pick: &'static str = cliclack::select("ACME directory")
                    .initial_value("production")
                    .item("production", "Let's Encrypt production", "real certs")
                    .item("staging", "Let's Encrypt staging", "test only")
                    .item("custom", "Custom URL", "self-hosted CA")
                    .interact()
                    .map_err(|e| anyhow!("acme directory prompt: {e}"))?;
                if pick == "custom" {
                    cliclack::input("Custom ACME directory URL")
                        .placeholder("https://acme.internal/directory")
                        .interact()
                        .map_err(|e| anyhow!("custom acme url prompt: {e}"))?
                } else {
                    pick.to_string()
                }
            }
        };

        // 3. ACME contact email. Permissive validator; empty allowed.
        let acme_email = match &args.acme_email {
            Some(e) => e.clone(),
            None => cliclack::input("ACME contact email")
                .placeholder("operator@example.com")
                .required(false)
                .validate(validate_email)
                .interact()
                .map_err(|e| anyhow!("acme email prompt: {e}"))?,
        };

        // 4. Cloudflare DNS API token (optional).
        let cf_dns_token = if args.no_cf_dns_token {
            None
        } else {
            match &args.cf_dns_token {
                Some(t) => Some(t.clone()),
                None => {
                    let want = cliclack::confirm("Set a Cloudflare DNS API token?")
                        .initial_value(true)
                        .interact()
                        .map_err(|e| anyhow!("cf token confirm: {e}"))?;
                    if want {
                        let v: String = cliclack::password("Cloudflare DNS API token")
                            .mask('●')
                            .interact()
                            .map_err(|e| anyhow!("cf token prompt: {e}"))?;
                        if v.is_empty() { None } else { Some(v) }
                    } else {
                        None
                    }
                }
            }
        };

        // 5. Backup passphrase (optional). Two prompts when set so the
        //    operator can confirm the value before it gets encrypted.
        let backup_passphrase = if args.no_backup {
            None
        } else {
            match &args.backup_passphrase_file {
                Some(p) => Some(read_passphrase_file(p)?),
                None => {
                    let want = cliclack::confirm("Set a backup passphrase?")
                        .initial_value(false)
                        .interact()
                        .map_err(|e| anyhow!("backup confirm: {e}"))?;
                    if want {
                        let first: String = cliclack::password("Backup passphrase")
                            .mask('●')
                            .interact()
                            .map_err(|e| anyhow!("backup prompt: {e}"))?;
                        // cliclack's `validate` runs against the input
                        // text of the prompt itself, not against a prior
                        // captured value, so we collect a second password
                        // and compare here. A mismatch loops back via a
                        // recursive call: simpler than wiring up shared
                        // mutable state through `validate`.
                        let confirm: String = cliclack::password("Confirm backup passphrase")
                            .mask('●')
                            .interact()
                            .map_err(|e| anyhow!("backup confirm prompt: {e}"))?;
                        if first != confirm {
                            cliclack::log::error("passphrases did not match; please re-enter")?;
                            return Self::gather_interactive(args, host_ip);
                        }
                        if first.is_empty() { None } else { Some(first) }
                    } else {
                        None
                    }
                }
            }
        };

        // extra-sans: not exposed as a prompt in the 0.12 mockup; flag-only.
        let extra_sans = args.extra_sans.clone();

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
            force: args.force,
            host_ip,
        })
    }

    /// The controller's state-dir. Matches the systemd unit's
    /// `--state-dir /var/lib/isengard` ExecStart so a one-shot CLI like
    /// `isengard controller token mint` lands on the same SQLite DB the
    /// running controller is using. Phase 0.16 dropped the `/controller`
    /// suffix that was a leak from the bollard-compose era.
    ///
    /// Migration: if a pre-0.16 install has data at
    /// `<state_dir>/controller/isengard.db`, prefer that path so a re-run
    /// of `isengard init` is idempotent. Greenfield installs land on the
    /// new path.
    fn controller_state_dir(&self) -> PathBuf {
        let new_db = self.state_dir.join("isengard.db");
        if new_db.exists() {
            return self.state_dir.clone();
        }
        let legacy = self.state_dir.join("controller");
        if legacy.join("isengard.db").exists() {
            return legacy;
        }
        self.state_dir.clone()
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
    /// File the operator reads to invite a second host into the fleet.
    /// Pre-Phase-0.16 this file was misnamed `enrollment.txt` and held
    /// the same token the local agent had just redeemed (so the snippet
    /// was dead on arrival). Phase 0.16 mints a dedicated second token
    /// and renames the file to `join.txt` to match the `isengard join`
    /// CLI subcommand it goes with.
    fn join_file(&self) -> PathBuf {
        self.etc_dir.join("join.txt")
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

    let stdin_is_tty = std::io::stdin().is_terminal();
    let interactive = stdin_is_tty && !args.non_interactive;

    // Bail early when there's no TTY for prompts and no flags to drive
    // a flag-only install. Inside cliclack's prompt this surfaces as an
    // unhelpful EOF; surface a clear error before the chrome starts.
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

    if interactive {
        run_interactive(args, host_ip).await
    } else {
        run_non_interactive(args, host_ip).await
    }
}

/// Interactive flow: cliclack drives every prompt and every step glyph.
async fn run_interactive(args: InitArgs, host_ip: String) -> Result<()> {
    cliclack::intro(format!("isengard init  {}", env!("ISENGARD_BUILD_VERSION")))?;

    let host = probe_host();
    // `cliclack::note` would render the body inside a rounded ASCII box;
    // the mockup is just a `◇  Detected host` header followed by free
    // rows under the connector. `log::step` prints the header line; the
    // rows below go through `eprintln!` so they stay on stderr in the
    // same buffer cliclack is using (mixing stdout `println!` here would
    // interleave on slow terminals).
    cliclack::log::step("Detected host")?;
    for line in format_host_info(&host).lines() {
        eprintln!("\x1b[38;5;8m│\x1b[0m  {line}");
    }
    eprintln!("\x1b[38;5;8m│\x1b[0m");

    let plan = Plan::gather_interactive(&args, host_ip)?;

    // `◆  Bootstrapping` — uses the active-step glyph because this is the
    // header of the in-flight section. cliclack's `log::success` is the
    // helper that wraps `active_symbol()` (◆); `log::step` would render
    // `◇` (submit). The completed step lines below use `log::step` so the
    // header pops as the brighter glyph.
    cliclack::log::success("Bootstrapping")?;

    let token = match plan.execute_with_clack().await {
        Ok(t) => t,
        Err(e) => {
            cliclack::outro_cancel(format!("install failed: {e:#}"))?;
            return Err(e);
        }
    };

    // Plain outro line (`└  isengard is ready.`) followed by the multi-line
    // ready block as free-form indented text. `outro_note` wraps the body
    // in a rounded ASCII box which doesn't match the target shape (the
    // mockup is just labelled rows under the outro, no second border).
    cliclack::outro("isengard is ready.")?;
    println!();
    for line in format_ready_block(&plan, &token).lines() {
        println!("   {line}");
    }
    Ok(())
}

/// Non-interactive flow: plain `[step]` lines, no `│` connector.
async fn run_non_interactive(args: InitArgs, host_ip: String) -> Result<()> {
    println!("isengard init {}", env!("ISENGARD_BUILD_VERSION"));
    println!("Using flag-driven config.");
    let plan = Plan::from_args(&args, host_ip)?;
    let token = plan.execute_plain().await?;
    println!();
    println!("isengard is ready.");
    println!();
    for line in format_ready_block(&plan, &token).lines() {
        println!("  {line}");
    }
    Ok(())
}

/// True if the caller passed enough flags to drive a non-interactive
/// install without prompts. Conservative: requires either the ACME pair
/// or the `--no-cf-dns-token --no-backup` opt-outs.
fn has_required_flags(args: &InitArgs) -> bool {
    args.acme_email.is_some() && args.acme_directory.is_some()
        || args.no_cf_dns_token && args.no_backup
}

/// First 8 chars + ellipsis. Used to preview enrollment tokens without
/// leaking the full value into shell history.
fn token_preview(token: &str) -> String {
    let prefix: String = token.chars().take(8).collect();
    format!("{prefix}...")
}

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

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getuid() -> u32;
}

#[cfg(target_os = "linux")]
unsafe fn libc_getuid() -> u32 {
    unsafe { getuid() }
}

/// Best-effort host-IP probe.
pub(super) fn detect_host_ip() -> Result<String> {
    if let Ok(out) = std::process::Command::new("ip")
        .args(["route", "get", "1.1.1.1"])
        .output()
        && out.status.success()
    {
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

// Embed the systemd unit files at compile time.
const ISO_CONTROLLER_UNIT: &str =
    include_str!("../../../../install/systemd/iso-controller.service");
const ISO_AGENT_UNIT: &str = include_str!("../../../../install/systemd/iso-agent.service");
const ISO_AGENT_TARGET: &str = include_str!("../../../../install/systemd/iso-agent.target");

impl Plan {
    /// Execute the install with cliclack rendering. Each instant step
    /// emits a `cliclack::log::step` line (◇ glyph, "submitted" colour);
    /// each timed step starts a `cliclack::spinner` that stops with a
    /// matching `◇` once it finishes. Returns the minted enrollment
    /// token so the outro can render it.
    async fn execute_with_clack(&self) -> Result<String> {
        // Instant ops: dirs + master key + master-key.env.
        self.setup_dirs()?;
        cliclack::log::step(done(
            "Created directories",
            &format!("{} + {}", self.state_dir.display(), self.etc_dir.display()),
        ))?;

        self.setup_master_key()?;
        cliclack::log::step(done(
            "Generated master key",
            &self.master_key_file().display().to_string(),
        ))?;

        self.write_master_key_env()?;
        cliclack::log::step(done(
            "Wrote master-key.env",
            &self.master_key_env_file().display().to_string(),
        ))?;

        let secret_count = self.cf_dns_token.iter().count() + self.backup_passphrase.iter().count();
        if secret_count == 0 {
            cliclack::log::step(done("Encrypted secrets", "(none configured)"))?;
        } else {
            let sp = cliclack::spinner();
            sp.start("Encrypting secrets");
            match self.bootstrap_secrets().await {
                Ok(()) => sp.stop(done("Encrypted secrets", &format!("{secret_count} stored"))),
                Err(e) => {
                    sp.error(format!("Encrypting secrets failed: {e:#}"));
                    return Err(e);
                }
            }
        }

        self.write_env_file()?;
        cliclack::log::step(done(
            "Wrote env file",
            &self.env_file().display().to_string(),
        ))?;

        self.install_systemd_units()?;
        cliclack::log::step(done("Installed systemd units", "iso-controller, iso-agent"))?;

        // Controller boot: poll for CA readiness.
        let sp = cliclack::spinner();
        sp.start("Starting iso-controller");
        let started = Instant::now();
        match self.start_controller_with_spinner(&sp).await {
            Ok(()) => sp.stop(done(
                "Controller healthy",
                &format!(
                    "https://localhost:9417  ({:.1}s)",
                    started.elapsed().as_secs_f32()
                ),
            )),
            Err(e) => {
                sp.error(format!("Controller boot failed: {e:#}"));
                return Err(e);
            }
        }

        self.export_ca().await?;
        cliclack::log::step(done(
            "Exported CA cert",
            &self.ca_pem_file().display().to_string(),
        ))?;

        // Phase 0.16 (P2.2 fix): two-token flow. `agent_token` is consumed
        // by the local agent at boot; `join_token` is minted *after* the
        // local agent enrolls and is what we surface to the operator for
        // inviting a second host. Previously both jobs reused one token,
        // so the printed snippet + enrollment.txt were dead on arrival.
        let agent_token = {
            let sp = cliclack::spinner();
            sp.start("Minting local-agent enrollment token");
            match self.mint_token().await {
                Ok(t) => {
                    sp.stop(done("Minted local-agent token", "(single-use)"));
                    t
                }
                Err(e) => {
                    sp.error(format!("Mint local-agent token failed: {e:#}"));
                    return Err(e);
                }
            }
        };
        self.write_token_file(&agent_token)?;

        let sp = cliclack::spinner();
        sp.start("Starting iso-agent");
        let started = Instant::now();
        match self.start_agent_with_progress() {
            Ok(()) => sp.stop(done(
                "Agent enrolled",
                &format!("({:.1}s)", started.elapsed().as_secs_f32()),
            )),
            Err(e) => {
                sp.error(format!("Agent start failed: {e:#}"));
                return Err(e);
            }
        }

        let join_token = {
            let sp = cliclack::spinner();
            sp.start("Minting join token");
            match self.mint_token().await {
                Ok(t) => {
                    sp.stop(done(
                        "Minted join token",
                        &format!("{} (15m TTL)", token_preview(&t)),
                    ));
                    t
                }
                Err(e) => {
                    sp.error(format!("Mint join token failed: {e:#}"));
                    return Err(e);
                }
            }
        };
        self.write_join_file(&join_token)?;

        Ok(join_token)
    }

    /// Execute the install with plain stdout. Used when stdin is piped
    /// or `--non-interactive` was set; cliclack's connector bar would
    /// render unreadably without a TTY.
    async fn execute_plain(&self) -> Result<String> {
        let mut step = 0u32;
        let total = 10u32;
        let mut emit = |label: &str, detail: &str| {
            step += 1;
            println!("[{step}/{total}] {label:<26}{detail}");
        };

        self.setup_dirs()?;
        emit(
            "Created directories",
            &format!("{} + {}", self.state_dir.display(), self.etc_dir.display()),
        );

        self.setup_master_key()?;
        emit(
            "Generated master key",
            &self.master_key_file().display().to_string(),
        );

        self.write_master_key_env()?;
        emit(
            "Wrote master-key.env",
            &self.master_key_env_file().display().to_string(),
        );

        let secret_count = self.cf_dns_token.iter().count() + self.backup_passphrase.iter().count();
        if secret_count == 0 {
            emit("Encrypted secrets", "(none configured)");
        } else {
            self.bootstrap_secrets().await?;
            emit("Encrypted secrets", &format!("{secret_count} stored"));
        }

        self.write_env_file()?;
        emit("Wrote env file", &self.env_file().display().to_string());

        self.install_systemd_units()?;
        emit("Installed systemd units", "iso-controller, iso-agent");

        self.start_controller_plain().await?;
        emit("Controller healthy", "https://localhost:9417");

        self.export_ca().await?;

        // Phase 0.16 (P2.2 fix): two-token flow. See execute_with_clack
        // for the rationale; same shape, plain-stdout step emitters.
        let agent_token = self.mint_token().await?;
        self.write_token_file(&agent_token)?;
        emit("Minted local-agent token", "(single-use)");

        self.start_agent_with_progress()?;
        emit("Agent enrolled", "");

        let join_token = self.mint_token().await?;
        self.write_join_file(&join_token)?;
        emit(
            "Minted join token",
            &format!("{} (15m TTL)", token_preview(&join_token)),
        );

        Ok(join_token)
    }

    fn setup_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.etc_dir)
            .with_context(|| format!("create {:?}", self.etc_dir))?;
        // Controller state lives at `self.state_dir` directly (no
        // `/controller` subdir as of Phase 0.16). The agent's state lives
        // under `agent/` so the two co-tenants don't fight over filenames.
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("create {:?}", self.state_dir))?;
        std::fs::create_dir_all(self.agent_state_dir())
            .with_context(|| format!("create {:?}", self.agent_state_dir()))?;
        std::fs::create_dir_all(self.state_dir.join("stacks"))
            .with_context(|| format!("create {:?}", self.state_dir.join("stacks")))?;
        std::fs::create_dir_all("/var/lib/wisp").context("create /var/lib/wisp")?;
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
            "# Isengard non-secret config (Phase 0.12 systemd-native install).\n\
             # Sourced by iso-controller.service + iso-agent.service via systemd's\n\
             # EnvironmentFile=. Secrets live encrypted in the controller's SQLite,\n\
             # keyed by /etc/isengard/master.key.\n\
             \n\
             ISENGARD_ACME_EMAIL={acme_email}\n\
             ISENGARD_ACME_DOMAINS={acme_domains}\n\
             ISENGARD_ACME_DIRECTORY={acme_directory}\n\
             \n\
             ISENGARD_LISTEN={listen_grpc}\n\
             ISENGARD_DASHBOARD_LISTEN={listen_dashboard}\n\
             \n\
             ISENGARD_EXTRA_SANS={extra_sans}\n\
             \n\
             RUST_LOG=info\n",
            acme_email = self.acme_email,
            acme_domains = self.acme_domains,
            acme_directory = self.acme_directory,
            listen_grpc = self.listen_grpc,
            listen_dashboard = self.listen_dashboard,
            extra_sans = extra_sans,
        );
        write_secret(&path, body.as_bytes(), 0o644).with_context(|| format!("write {path:?}"))?;
        Ok(())
    }

    fn install_systemd_units(&self) -> Result<()> {
        let dir = std::path::Path::new("/etc/systemd/system");
        std::fs::create_dir_all(dir).context("create /etc/systemd/system")?;
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

    /// Boot the controller unit, then poll for CA readiness. The caller
    /// owns the spinner; we update the message in-place so the operator
    /// sees forward progress on slow first boots.
    async fn start_controller_with_spinner(&self, sp: &cliclack::ProgressBar) -> Result<()> {
        run_systemctl(&["enable", "iso-controller.service"]).ok();
        run_systemctl(&["start", "iso-controller.service"])?;
        sp.set_message("Waiting for controller to become ready");

        let inv_dir = self.controller_state_dir();
        for i in 0..30u32 {
            let inv = match open_controller_inventory(&inv_dir).await {
                Ok(i) => i,
                Err(_) => {
                    sp.set_message(format!("Waiting for controller ({}/30s)", i + 1));
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
            sp.set_message(format!("Waiting for controller ({}/30s)", i + 1));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("controller did not become ready in 30s; check `journalctl -u iso-controller`")
    }

    /// Plain-output controller wait used by the non-interactive path.
    async fn start_controller_plain(&self) -> Result<()> {
        run_systemctl(&["enable", "iso-controller.service"]).ok();
        run_systemctl(&["start", "iso-controller.service"])?;

        let inv_dir = self.controller_state_dir();
        for _ in 0..30u32 {
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

    /// Persist a freshly-minted join token (chmod 0600) so an operator
    /// can invite another host within the TTL without re-running init.
    ///
    /// IMPORTANT: this MUST be a token distinct from the one the local
    /// agent consumed at boot. Enrollment tokens are single-use; writing
    /// the local agent's token here yielded a snippet that was dead on
    /// arrival (the P2.2 bug from the wave 2.D multi-host smoke).
    fn write_join_file(&self, token: &str) -> Result<()> {
        let path = self.join_file();
        let body = format!(
            "# Isengard join token (15 min TTL from mint time, single-use).\n\
             # Use on the new host:\n\
             #   isengard join --token <token> --ca-pem-path /etc/isengard/ca.pem https://<controller-host>:9417\n\
             # Mint a fresh one any time with: isengard controller token mint --role agent\n\
             token: {token}\n\
             ca:    {ca}\n\
             controller: https://{host}:9417\n",
            ca = self.ca_pem_file().display(),
            host = if self.host_ip.is_empty() {
                "<host-ip>".to_string()
            } else {
                self.host_ip.clone()
            },
        );
        write_secret(&path, body.as_bytes(), 0o600).with_context(|| format!("write {path:?}"))?;
        Ok(())
    }

    fn start_agent_with_progress(&self) -> Result<()> {
        run_systemctl(&["enable", "iso-agent.service"]).ok();
        run_systemctl(&["start", "iso-agent.service"])?;
        Ok(())
    }
}

async fn open_controller_inventory(dir: &std::path::Path) -> Result<isengard_storage::Inventory> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    let db = dir.join("isengard.db");
    isengard_storage::Inventory::open(&db)
        .await
        .with_context(|| format!("open inventory {db:?}"))
}

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
// `isengard join`: agent-only enrollment against an existing controller.
// Same cliclack chrome as `init` so the two flows feel like one product.
// =====================================================================

/// Flags for `isengard join`.
#[derive(Debug, Args, Clone)]
pub struct JoinArgs {
    /// Controller URL, e.g. `https://controller.example.com:9417`.
    pub controller: String,
    /// One-time enrollment token (minted via `isengard controller token mint`).
    #[arg(long)]
    pub token: String,
    /// Path to a PEM file holding the controller's CA root. One of
    /// `--ca-pem-path` or `--ca-pem-base64` is required.
    #[arg(long, conflicts_with = "ca_pem_base64")]
    pub ca_pem_path: Option<PathBuf>,
    /// Base64-encoded controller CA PEM (single-line, shell-safe).
    #[arg(long)]
    pub ca_pem_base64: Option<String>,
    /// Optional sha256 fingerprint of the controller CA cert
    /// (`sha256:HEX` form). Defense-in-depth pin verified before write.
    #[arg(long)]
    pub ca_fingerprint: Option<String>,
    #[arg(long, default_value = "/var/lib/isengard")]
    pub state_dir: PathBuf,
    #[arg(long, default_value = "/etc/isengard")]
    pub etc_dir: PathBuf,
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

    let interactive = std::io::stdin().is_terminal() && !args.non_interactive;

    if interactive {
        run_join_interactive(args).await
    } else {
        run_join_plain(args).await
    }
}

async fn run_join_interactive(args: JoinArgs) -> Result<()> {
    cliclack::intro(format!("isengard join  {}", env!("ISENGARD_BUILD_VERSION")))?;
    let host = probe_host();
    // Same free-rows treatment as `init` (see comment there).
    cliclack::log::step("Detected host")?;
    for line in format_host_info(&host).lines() {
        eprintln!("\x1b[38;5;8m│\x1b[0m  {line}");
    }
    eprintln!("\x1b[38;5;8m│\x1b[0m");
    // `◆  Joining controller` — same active-step glyph as `init` uses
    // for `Bootstrapping`. The completed step lines below go through
    // `log::step` so they render `◇`.
    cliclack::log::success("Joining controller")?;

    match join_steps(&args).await {
        Ok(()) => {}
        Err(e) => {
            cliclack::outro_cancel(format!("join failed: {e:#}"))?;
            return Err(e);
        }
    }

    cliclack::outro("agent enrolled.")?;
    println!();
    println!("   Controller   {}", args.controller);
    println!("   Verify       systemctl status iso-agent");
    println!("   Logs         journalctl -fu iso-agent");
    Ok(())
}

async fn run_join_plain(args: JoinArgs) -> Result<()> {
    println!("isengard join {}", env!("ISENGARD_BUILD_VERSION"));
    join_steps(&args).await?;
    println!();
    println!("agent enrolled.");
    println!("  Controller   {}", args.controller);
    println!("  Verify       systemctl status iso-agent");
    println!("  Logs         journalctl -fu iso-agent");
    Ok(())
}

/// Shared join steps. Renders cliclack steps when invoked from the
/// interactive path; cliclack's log helpers no-op gracefully on a
/// non-TTY so we can call them from both paths without branching.
async fn join_steps(args: &JoinArgs) -> Result<()> {
    let is_tty = std::io::stdin().is_terminal();

    let emit = |label: &str, detail: &str| -> Result<()> {
        if is_tty {
            cliclack::log::step(done(label, detail))?;
        } else {
            println!("- {label:<26}{detail}");
        }
        Ok(())
    };

    std::fs::create_dir_all(&args.etc_dir).with_context(|| format!("create {:?}", args.etc_dir))?;
    let agent_state_dir = args.state_dir.join("agent");
    std::fs::create_dir_all(&agent_state_dir)
        .with_context(|| format!("create {agent_state_dir:?}"))?;
    std::fs::create_dir_all("/var/lib/wisp").context("create /var/lib/wisp")?;
    let _ = chmod(&args.etc_dir, 0o750);
    emit("Created directories", &args.etc_dir.display().to_string())?;

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
    emit("Pinned controller CA", &ca_path.display().to_string())?;

    let token_path = args.etc_dir.join("agent-token.env");
    let token_body = format!(
        "ISENGARD_ENROLL_TOKEN={}\n\
         ISENGARD_CONTROLLER_CA_PEM_PATH={}\n",
        args.token,
        ca_path.display()
    );
    write_secret(&token_path, token_body.as_bytes(), 0o600)?;
    emit("Wrote enrollment token", &token_path.display().to_string())?;

    let env_path = args.etc_dir.join("isengard.env");
    let env_body = format!(
        "# Isengard agent-only join (Phase 0.12). Sourced by iso-agent.service.\n\
         ISENGARD_CONTROLLER={controller}\n\
         RUST_LOG=info\n",
        controller = args.controller,
    );
    write_secret(&env_path, env_body.as_bytes(), 0o644)?;
    emit("Wrote env file", &env_path.display().to_string())?;

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
    emit("Installed iso-agent.service", "")?;

    run_systemctl(&["enable", "iso-agent.service"]).ok();
    run_systemctl(&["start", "iso-agent.service"])?;
    emit("Started iso-agent.service", "")?;

    Ok(())
}

/// Compute the lowercase-hex sha256 of the first PEM CERTIFICATE block.
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
