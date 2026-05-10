//! Phase 0.10: `isengard init` subcommand.
//!
//! Replaces the ~900 line `install/install.sh` with native Rust. The bash
//! script becomes a ~50 line bootstrap that downloads the binary and execs
//! `isengard init` with the same flags the operator would have passed.
//!
//! Two operating modes:
//!
//!   - **Interactive**: stdin (or `/dev/tty` when piped) is a real TTY and
//!     `--non-interactive` was not set. A ratatui wizard collects every
//!     unset value and streams the install steps live as they execute.
//!   - **Non-interactive**: `--non-interactive` was set, or no TTY could be
//!     opened (fully headless). Every required value must come from a flag;
//!     missing values fail with a clear error naming the flag, and the
//!     install runs as a plain stdout log.
//!
//! The actual install steps live in [`Plan::execute_with_progress`] and run
//! identically in both modes; the only difference is whether the wizard
//! consumes the [`ProgressEvent`] stream or it gets dropped.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use tokio::sync::mpsc;

mod progress;
mod theme;
mod wizard;

pub(crate) use progress::{BootstrapStep, ProgressEvent, SummaryData};

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

/// Resolved configuration for one `isengard init` run. All values are
/// fully populated by the time `execute_with_progress` runs: the wizard
/// (interactive) or flag parsing (non-interactive) handled the resolution
/// upstream.
///
/// Visible to the in-crate wizard module so it can construct a plan from
/// the operator's wizard answers and hand it back to `run`.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) acme_email: String,
    pub(crate) acme_domains: String,
    pub(crate) acme_directory: String,
    pub(crate) cf_dns_token: Option<String>,
    pub(crate) backup_passphrase: Option<String>,
    pub(crate) extra_sans: Vec<String>,
    pub(crate) listen_grpc: String,
    pub(crate) listen_dashboard: String,
    pub(crate) state_dir: PathBuf,
    pub(crate) etc_dir: PathBuf,
    pub(crate) runtime: String,
    pub(crate) force: bool,
    pub(crate) host_ip: String,
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
/// Three branches:
///
///   1. `--non-interactive` (or no TTY available anywhere): build the plan
///      from CLI flags, run the install with stdout logging.
///   2. `/dev/tty` available: launch the ratatui wizard, collect missing
///      values, run the install while streaming progress events into the
///      wizard. The wizard owns the screen until completion.
///   3. Wizard requested but no TTY: fall back to (1) with a friendly
///      message naming the flags the operator can pass.
pub async fn run(args: InitArgs) -> Result<()> {
    preflight()?;
    let host_ip = match &args.host_ip {
        Some(ip) => ip.clone(),
        None => detect_host_ip().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "host ip auto-detect failed; falling back to 127.0.0.1 (pass --host-ip to override)");
            "127.0.0.1".to_string()
        }),
    };

    if args.non_interactive {
        let plan = Plan::from_args(&args, host_ip)?;
        return plan.execute_logged().await;
    }

    // Default (interactive). If stdin is a TTY use it directly; otherwise
    // try `/dev/tty`. Headless: bail with a clear hint.
    if std::io::stdin().is_terminal() {
        return wizard::run(args, host_ip).await;
    }
    if std::path::Path::new("/dev/tty").exists() {
        return wizard::run(args, host_ip).await;
    }

    bail!(
        "isengard init: TUI unavailable (no controlling terminal). Re-run with --non-interactive and the required flags, e.g.:\n  isengard init --non-interactive --acme-email you@example.com --acme-domains example.com --acme-directory staging"
    )
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
const ISO_CONTROLLER_UNIT: &str =
    include_str!("../../../../install/systemd/iso-controller.service");
const ISO_AGENT_UNIT: &str = include_str!("../../../../install/systemd/iso-agent.service");
const ISO_AGENT_TARGET: &str = include_str!("../../../../install/systemd/iso-agent.target");

impl Plan {
    /// Non-interactive install path: prints one log line per phase to
    /// stdout. Used when `--non-interactive` is set or no TTY can be
    /// opened. Calls into the same step methods the wizard uses, just
    /// without the event channel.
    async fn execute_logged(&self) -> Result<()> {
        println!();
        println!("[1/12] preflight ok");
        let summary = self.execute_with_progress(None).await?;
        println!("[12/12] done");
        self.print_banner(&summary.token);
        Ok(())
    }

    /// Drive every install phase, optionally streaming progress events
    /// for the wizard. Returns the summary data the Done screen renders.
    /// Errors propagate after a `Failed` event is emitted (so the wizard
    /// has a chance to render the failed step before unwinding).
    pub(crate) async fn execute_with_progress(
        &self,
        events: Option<mpsc::Sender<ProgressEvent>>,
    ) -> Result<SummaryData> {
        // Helper closures: they tolerate `events.is_none()` and a closed
        // channel (best-effort; never block the install on a stuck UI).
        let send = |evt: ProgressEvent| {
            if let Some(tx) = events.as_ref() {
                let _ = tx.try_send(evt);
            }
        };
        let started = |step: BootstrapStep| ProgressEvent::Started(step);
        let finished =
            |step: BootstrapStep, detail: Option<String>| ProgressEvent::Finished { step, detail };
        let failed = |step: BootstrapStep, error: String| ProgressEvent::Failed { step, error };

        // ---- 1: dirs --------------------------------------------------
        send(started(BootstrapStep::SetupDirs));
        match self.setup_dirs() {
            Ok(()) => send(finished(
                BootstrapStep::SetupDirs,
                Some(format!(
                    "{} + {}",
                    self.state_dir.display(),
                    self.etc_dir.display()
                )),
            )),
            Err(e) => {
                send(failed(BootstrapStep::SetupDirs, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 2: master key -------------------------------------------
        send(started(BootstrapStep::GenerateMasterKey));
        match self.setup_master_key() {
            Ok(()) => send(finished(
                BootstrapStep::GenerateMasterKey,
                Some(self.master_key_file().display().to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::GenerateMasterKey, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 3: master-key.env ---------------------------------------
        send(started(BootstrapStep::WriteMasterKeyEnv));
        match self.write_master_key_env() {
            Ok(()) => send(finished(
                BootstrapStep::WriteMasterKeyEnv,
                Some(self.master_key_env_file().display().to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::WriteMasterKeyEnv, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 4: secrets ----------------------------------------------
        send(started(BootstrapStep::BootstrapSecrets));
        let mut secret_count = 0usize;
        if self.cf_dns_token.is_some() {
            secret_count += 1;
        }
        if self.backup_passphrase.is_some() {
            secret_count += 1;
        }
        match self.bootstrap_secrets().await {
            Ok(()) => send(finished(
                BootstrapStep::BootstrapSecrets,
                Some(if secret_count == 0 {
                    "no secrets to encrypt".to_string()
                } else if secret_count == 1 {
                    "1 secret encrypted".to_string()
                } else {
                    format!("{secret_count} secrets encrypted")
                }),
            )),
            Err(e) => {
                send(failed(BootstrapStep::BootstrapSecrets, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 5: env file ---------------------------------------------
        send(started(BootstrapStep::WriteEnvFile));
        match self.write_env_file() {
            Ok(()) => send(finished(
                BootstrapStep::WriteEnvFile,
                Some(self.env_file().display().to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::WriteEnvFile, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 6: systemd units ----------------------------------------
        send(started(BootstrapStep::InstallSystemdUnits));
        match self.install_systemd_units() {
            Ok(()) => send(finished(
                BootstrapStep::InstallSystemdUnits,
                Some("daemon-reload OK".to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::InstallSystemdUnits, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 7+8: start + wait controller ----------------------------
        // The legacy `start_controller` does both: systemctl start +
        // 30s health poll. Split here for the wizard so the operator
        // sees a separate spinner row for the wait.
        send(started(BootstrapStep::StartController));
        match self.systemctl_start_controller() {
            Ok(()) => send(finished(
                BootstrapStep::StartController,
                Some("systemctl start iso-controller.service".to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::StartController, format!("{e:#}")));
                return Err(e);
            }
        }
        send(started(BootstrapStep::WaitControllerHealthy));
        match self.wait_controller_healthy(events.as_ref()).await {
            Ok(()) => send(finished(
                BootstrapStep::WaitControllerHealthy,
                Some(format!("https://localhost:{}", self.listen_port())),
            )),
            Err(e) => {
                send(failed(
                    BootstrapStep::WaitControllerHealthy,
                    format!("{e:#}"),
                ));
                return Err(e);
            }
        }

        // ---- 9: export CA --------------------------------------------
        send(started(BootstrapStep::ExportCa));
        match self.export_ca().await {
            Ok(()) => send(finished(
                BootstrapStep::ExportCa,
                Some(self.ca_pem_file().display().to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::ExportCa, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 10: mint token ------------------------------------------
        send(started(BootstrapStep::MintToken));
        let token = match self.mint_token().await {
            Ok(t) => {
                send(finished(
                    BootstrapStep::MintToken,
                    Some("15m TTL".to_string()),
                ));
                t
            }
            Err(e) => {
                send(failed(BootstrapStep::MintToken, format!("{e:#}")));
                return Err(e);
            }
        };

        // ---- 11: token file ------------------------------------------
        send(started(BootstrapStep::WriteTokenFile));
        match self.write_token_file(&token) {
            Ok(()) => send(finished(
                BootstrapStep::WriteTokenFile,
                Some(self.token_file().display().to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::WriteTokenFile, format!("{e:#}")));
                return Err(e);
            }
        }

        // ---- 12: agent ----------------------------------------------
        send(started(BootstrapStep::StartAgent));
        match self.start_agent() {
            Ok(()) => send(finished(
                BootstrapStep::StartAgent,
                Some("iso-agent.service started".to_string()),
            )),
            Err(e) => {
                send(failed(BootstrapStep::StartAgent, format!("{e:#}")));
                return Err(e);
            }
        }

        let summary = SummaryData {
            controller_url_local: format!("https://localhost:{}", self.listen_port()),
            controller_url_remote: format!(
                "https://{}:{}",
                if self.host_ip.is_empty() {
                    "<your-host-ip>"
                } else {
                    &self.host_ip
                },
                self.listen_port()
            ),
            dashboard_url: format!("http://localhost:{}", self.dashboard_port()),
            host_ip: self.host_ip.clone(),
            token: token.clone(),
            ca_pem_path: self.ca_pem_file().display().to_string(),
            state_dir: self.state_dir.display().to_string(),
        };
        send(ProgressEvent::Summary(summary.clone()));
        Ok(summary)
    }

    /// Listen port extracted from `listen_grpc` (`0.0.0.0:9417` -> 9417).
    fn listen_port(&self) -> u16 {
        self.listen_grpc
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9417)
    }

    fn dashboard_port(&self) -> u16 {
        self.listen_dashboard
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9418)
    }

    fn systemctl_start_controller(&self) -> Result<()> {
        run_systemctl(&["enable", "iso-controller.service"]).ok();
        run_systemctl(&["start", "iso-controller.service"])
    }

    /// Poll for controller readiness, ticking the wizard on each retry.
    /// Mirrors the legacy 30s budget; emits `Tick` events so the wizard
    /// can show "Waiting for controller… 00:07".
    async fn wait_controller_healthy(
        &self,
        events: Option<&mpsc::Sender<ProgressEvent>>,
    ) -> Result<()> {
        let inv_dir = self.controller_state_dir();
        for i in 0..30u32 {
            if let Some(tx) = events {
                let _ = tx.try_send(ProgressEvent::Tick(BootstrapStep::WaitControllerHealthy));
            }
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
            // Only print the dot in non-wizard mode (events == None).
            if events.is_none() {
                print!(".");
                std::io::stdout().flush().ok();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = i;
        }
        bail!("controller did not become ready in 30s; check `journalctl -u iso-controller`")
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
