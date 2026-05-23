//! Operator-facing SSH bastion CLI: `isd ssh ...`.
//!
//! Five subcommands:
//!
//!  - `isd ssh <host>`: auto-mints a short-lived SSH user cert when
//!    the current one is missing or about to expire, then exec's
//!    `ssh <user>@<host>` with whatever extra args the operator
//!    passed through. The `external_subcommand` clap trick captures
//!    everything that does not match the explicit verbs below.
//!  - `isd ssh mint`: explicit re-mint. Useful when an operator
//!    wants a fresh window without dialing a host.
//!  - `isd ssh status`: pretty-print the current local cert
//!    (fingerprint, principals, validity window, comment) by
//!    shelling out to `ssh-keygen -L`.
//!  - `isd ssh hosts`: lists the fleet hosts the controller knows
//!    about. Reuses the dashboard's `GET /api/v1/hosts` endpoint.
//!  - `isd ssh ca pubkey`: dumps the controller SSH CA pubkey for
//!    drop-in into a non-Isengard host's `TrustedUserCAKeys`.
//!
//! The mint path POSTs `(pubkey, principals, ttl, comment)` to the
//! dashboard `POST /api/v1/ssh/cert` endpoint. The signed cert lands
//! next to the pubkey: `id_ed25519.pub` -> `id_ed25519-cert.pub`,
//! matching OpenSSH's convention so `ssh -i id_ed25519` picks the
//! cert up automatically without a `CertificateFile` override.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
// `crate::render::Table` aliased as `RenderTable` is already imported via the
// ssh hosts/audit renderers; reuse it for the cert-detail readout below.
use serde::{Deserialize, Serialize};

use crate::dial_target;
use crate::index_cache::{IndexCache, IndexRow};
use crate::render::{Align, CellStyle, Column, Table as RenderTable, render, render_plain};
use crate::session::Session;

pub mod picker;
pub mod trust;
pub mod trusted_hosts;

/// CLI flags for `isd ssh`.
///
/// `command` is optional: bare `isd ssh` (no sub-verb, no positional)
/// dispatches to the interactive host picker (`run_picker`). This
/// matches the picker-by-default contract from PR #236 and keeps clap
/// from bailing with "the following required arguments were not
/// provided" when an operator just types `isd ssh`.
#[derive(Debug, Args)]
pub struct SshArgs {
    /// Resolved sub-verb. `None` means bare `isd ssh`: open the picker.
    #[command(subcommand)]
    pub command: Option<SshCommand>,
}

/// Sub-verbs under `isd ssh`. The `Host` variant is an
/// `external_subcommand`: any token that does not match `mint`,
/// `status`, `hosts`, `ca`, `audit`, `trust`, or `untrust` is captured
/// here as the dial target + passthrough `ssh` arguments.
#[derive(Debug, Subcommand)]
pub enum SshCommand {
    /// Connect to a fleet host (auto-mints a cert if needed).
    #[command(external_subcommand)]
    Host(Vec<String>),
    /// Explicit cert re-mint.
    Mint(MintArgs),
    /// Show the current cert's state.
    Status,
    /// List hosts (`isd ssh hosts`) or override the dial target of a
    /// single host (`isd ssh hosts set <agent> --dial <target>`).
    Hosts(HostsArgs),
    /// CA introspection.
    #[command(subcommand)]
    Ca(CaCommand),
    /// Show recent SSH cert issuances.
    Audit(AuditArgs),
    /// Bootstrap a non-Isengard server: install the controller CA via
    /// the operator's existing SSH access, then record the host in
    /// `~/.config/isd/trusted_hosts.toml`.
    Trust(TrustArgs),
    /// Remove a host from the local trusted_hosts.toml. Remote sshd
    /// config is left alone (operator-managed).
    Untrust(UntrustArgs),
}

/// Sub-verbs under `isd ssh ca`. Reserved as a subcommand group so the
/// future `rotate` verb slots in cleanly without breaking the
/// `pubkey` surface.
#[derive(Debug, Subcommand)]
pub enum CaCommand {
    /// Print the controller SSH CA pubkey in OpenSSH wire format.
    Pubkey,
}

/// CLI flags for `isd ssh hosts`.
///
/// Bare `isd ssh hosts` (no sub-verb) renders the host list with a
/// dial target column. Sub-verbs (currently just `set`) gate further
/// actions on an enrolled host row.
#[derive(Debug, Args)]
pub struct HostsArgs {
    /// Resolved sub-verb. `None` means `isd ssh hosts`: list rendering.
    #[command(subcommand)]
    pub command: Option<HostsCommand>,
}

/// Sub-verbs under `isd ssh hosts`. Only `set` lives here today; the
/// nesting is kept so future verbs (rename, untrust) slot in cleanly.
#[derive(Debug, Subcommand)]
pub enum HostsCommand {
    /// Override the operator-facing dial target on an enrolled host.
    /// `agent` accepts the host's reported hostname, the host id ULID,
    /// the existing `dial_target`, or a `#N` index from the last
    /// `isd ssh hosts` render.
    Set(HostsSetArgs),
}

/// CLI flags for `isd ssh hosts set <agent>`.
#[derive(Debug, Args, Clone)]
pub struct HostsSetArgs {
    /// Host selector: hostname, host id ULID, existing dial target, or
    /// a `#N` index from the last `isd ssh hosts` render.
    pub agent: String,
    /// New dial target. Empty string clears the value (back to the
    /// `(unset)` display). When neither `--dial`, `--clear`, nor
    /// `--name` is passed the command errors so an operator never
    /// accidentally no-ops the row.
    #[arg(long)]
    pub dial: Option<String>,
    /// Clear the stored dial target (equivalent to `--dial ""`).
    #[arg(long, default_value_t = false)]
    pub clear: bool,
    /// New operator-facing host name (the label rendered in
    /// `isd ssh hosts`). Use this when the agent-reported value is a
    /// container hash (docker-in-docker) or just unhelpful. Empty
    /// string is rejected: the column has no `(unset)` render
    /// fallback and an empty value would break the display path.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

/// CLI flags for `isd ssh audit`. Both filters are optional.
#[derive(Debug, Args, Clone)]
pub struct AuditArgs {
    /// Show entries from this RFC3339 timestamp onward (inclusive).
    /// Anything before the cutoff is dropped server-side.
    #[arg(long)]
    pub since: Option<String>,
    /// Cap the number of entries returned. The dashboard further
    /// clamps this against an internal 5000-row ceiling.
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
}

/// CLI flags for `isd ssh trust`. The operator must already have SSH
/// access to `<host>` via their existing key: `isd ssh trust` does not
/// know any host-side passwords.
#[derive(Debug, Args, Clone)]
pub struct TrustArgs {
    /// The host to bootstrap (must already be SSH-reachable with the
    /// operator's existing key).
    pub host: String,
    /// Override the SSH user for the bootstrap connection. Defaults to
    /// `$USER` (via `whoami` then `$USER`).
    #[arg(long)]
    pub user: Option<String>,
    /// Override the SSH port for the bootstrap connection.
    #[arg(long, default_value_t = 22)]
    pub port: u16,
    /// Skip the local `trusted_hosts.toml` write. The bootstrap script
    /// still runs; only the local record is suppressed.
    #[arg(long)]
    pub no_record: bool,
}

/// CLI flags for `isd ssh untrust`. Local-only: removes the host from
/// `trusted_hosts.toml`. Does not touch the remote sshd config.
#[derive(Debug, Args, Clone)]
pub struct UntrustArgs {
    /// The host to remove from `~/.config/isd/trusted_hosts.toml`.
    pub host: String,
}

/// CLI flags for `isd ssh mint`. Defaults match what the auto-mint
/// path picks when `isd ssh <host>` decides the local cert is stale.
#[derive(Debug, Args, Clone)]
pub struct MintArgs {
    /// Path to the SSH public key to sign. Defaults to
    /// `~/.ssh/id_ed25519.pub`.
    #[arg(long)]
    pub pubkey: Option<PathBuf>,
    /// Requested TTL in seconds. Server caps via
    /// `ISENGARD_SSH_CERT_MAX_TTL` (default 1h, hard cap 24h).
    #[arg(long, default_value_t = 3600)]
    pub ttl: u64,
    /// Principal name baked into the cert (Unix username on the
    /// target host). Repeatable. Defaults to the operator's laptop
    /// username (`$USER`) via `default_user()` so `isd ssh host`
    /// just works against the operator's account on every host.
    #[arg(long = "principal")]
    pub principals: Vec<String>,
    /// Free-form key-id baked into the cert. Surfaces in `auditd`
    /// and `last` on the agent host. Defaults to
    /// `operator@<hostname> <UTC ISO8601>`.
    #[arg(long)]
    pub comment: Option<String>,
}

impl Default for MintArgs {
    fn default() -> Self {
        Self {
            pubkey: None,
            ttl: 3600,
            principals: vec![default_user()],
            comment: None,
        }
    }
}

/// Issuance request body for `POST /api/v1/ssh/cert`. Mirrors the
/// dashboard's `IssueSshCertBody`.
#[derive(Debug, Serialize)]
struct IssueBody<'a> {
    /// Operator's SSH public key in OpenSSH `authorized_keys` format.
    pubkey: &'a str,
    /// Principals (Unix usernames) the cert will be valid for.
    principals: &'a [String],
    /// Requested TTL in seconds. The dashboard caps this server-side.
    ttl_seconds: u64,
    /// Free-form key-id; the audit log keys on this.
    comment: &'a str,
}

/// Issuance response from `POST /api/v1/ssh/cert`. Mirrors the
/// dashboard's `IssueSshCertResponse`.
#[derive(Debug, Deserialize)]
struct IssueResponse {
    /// Signed OpenSSH user certificate (`ssh-ed25519-cert-v01@...`).
    certificate: String,
    /// Effective TTL in seconds after server-side capping.
    ttl_seconds: u64,
    /// SHA-256 fingerprint of the pubkey the cert is bound to.
    pubkey_fingerprint: String,
}

/// Response shape for `GET /api/v1/ssh/ca`.
#[derive(Debug, Deserialize)]
pub(super) struct CaPubkeyResponse {
    /// SSH CA pubkey in OpenSSH wire format.
    pub pubkey: String,
}

/// Subset of the dashboard `HostDto` we render under `isd ssh hosts`.
/// Extra fields on the wire are ignored thanks to serde's default
/// tolerance.
#[derive(Debug, Deserialize)]
struct HostRow {
    /// Host id ULID, as printed by the dashboard. Used to target the
    /// PATCH endpoint from `isd ssh hosts set` and to seed the index
    /// cache.
    #[serde(default)]
    id: String,
    /// Reported hostname (`uname -n` on the agent host).
    hostname: String,
    /// Last heartbeat we saw from this host's agent, if any.
    last_seen_at: Option<DateTime<Utc>>,
    /// Operator-facing dial target (e.g. `dirdmaster@10.17.0.125`).
    /// `None` for hosts enrolled by a pre-PR-B CLI build or from a
    /// non-SSH docker context. Rendered as `(unset)` in the table.
    #[serde(default)]
    dial_target: Option<String>,
}

/// Dispatch to the matching `ssh` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
pub async fn run(args: SshArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        None => run_picker(context).await,
        Some(SshCommand::Host(tokens)) => run_dial(tokens, context).await,
        Some(SshCommand::Mint(a)) => {
            let issued = run_mint(a, context).await?;
            print_mint_summary(&issued);
            Ok(())
        }
        Some(SshCommand::Status) => run_status().await,
        Some(SshCommand::Hosts(HostsArgs { command: None })) => run_hosts(context).await,
        Some(SshCommand::Hosts(HostsArgs {
            command: Some(HostsCommand::Set(a)),
        })) => run_hosts_set(a, context).await,
        Some(SshCommand::Ca(CaCommand::Pubkey)) => run_ca_pubkey(context).await,
        Some(SshCommand::Audit(a)) => run_audit(a, context).await,
        Some(SshCommand::Trust(a)) => trust::run_trust(a, context).await,
        Some(SshCommand::Untrust(a)) => trust::run_untrust(a).await,
    }
}

/// Auto-mint a cert when needed, then exec `ssh <user>@<host>` with
/// any extra args the operator passed through.
///
/// `tokens[0]` is the host (as the operator typed it); `tokens[1..]`
/// is forwarded to the `ssh` child verbatim. Phase 4 keeps host
/// resolution simple: the token is the SSH-reachable address as-is.
/// Label-based host resolution lands in a later release.
///
/// # Errors
///
/// Returns `Err` when no host token was passed, when the auto-mint
/// path fails, or when `ssh` cannot be spawned. The function exits
/// the process with `ssh`'s exit code on success.
async fn run_dial(tokens: Vec<String>, context: Option<&str>) -> Result<()> {
    if tokens.is_empty() {
        // No-args entry point: surface the interactive picker (fzf or
        // inquire), then re-enter `run_dial` with the chosen host.
        return run_picker(context).await;
    }
    let mut iter = tokens.into_iter();
    let host_token = iter
        .next()
        .ok_or_else(|| anyhow!("isd ssh <host>: missing host argument"))?;
    let ssh_args: Vec<String> = iter.collect();

    // Numeric tokens (`isd ssh 1`) resolve via the index cache the
    // last `isd ssh hosts` render wrote. Strip a leading `#` so
    // `#3` and `3` both work.
    let resolved_token = resolve_dial_token(&host_token, context).await?;

    let (override_user, host) = parse_user_host(&resolved_token);
    let default = default_user();
    if default.is_empty() {
        return Err(anyhow!(
            "could not determine default user (whoami and $USER both empty); pass user@host explicitly"
        ));
    }
    let ssh_user = override_user.clone().unwrap_or_else(|| default.clone());

    ensure_local_keypair()?;
    let pubkey_path = default_pubkey_path()?;
    let cert_path = cert_path_for(&pubkey_path);
    let want_principals = effective_principals(override_user.as_deref(), &default);

    if cert_is_stale(&cert_path) || !cert_covers_principals(&cert_path, &want_principals) {
        let mint_args = MintArgs {
            principals: want_principals.clone(),
            ..MintArgs::default()
        };
        let issued = run_mint(mint_args, context).await?;
        print_mint_summary(&issued);
    }

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg(format!("{ssh_user}@{host}"));
    for arg in &ssh_args {
        cmd.arg(arg);
    }
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning ssh {ssh_user}@{host}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Project a raw dial token (what the operator typed) into the actual
/// address to pass to `ssh`.
///
/// Resolution chain:
///
/// 1. `#N` or bare `\d+`: look up the last `isd ssh hosts` index
///    cache and return the row's `container_id` field (which we pack
///    with the row's `dial_target` at render time, falling back to the
///    hostname when unset).
/// 2. `user@host` or bare `host` that matches an enrolled host's
///    hostname AND that host has a `dial_target` set: substitute the
///    dial target. Preserves any `user@` override the operator typed.
/// 3. Anything else: pass through verbatim (matches the pre-PR-B
///    behaviour).
///
/// The controller is optional: a token that does not look like a
/// number passes through without an HTTP fetch so `isd ssh user@host`
/// keeps working without a live controller (matches pre-PR-B).
async fn resolve_dial_token(token: &str, context: Option<&str>) -> Result<String> {
    let bare = token.strip_prefix('#').unwrap_or(token);
    if let Ok(idx) = bare.parse::<usize>() {
        return resolve_index_to_dial(idx);
    }
    // For non-numeric tokens we may still want to substitute the
    // dial target. Try the controller; on any failure fall back to
    // the original token (the operator's typed value still works).
    let session = match Session::open(context).await {
        Ok(s) => s,
        Err(_) => return Ok(token.to_string()),
    };
    let Some(controller_url) = session.controller_url_opt() else {
        return Ok(token.to_string());
    };
    let (override_user, host) = parse_user_host(token);
    let lookup = match dial_target::lookup_dial_target(&session.client, controller_url, &host).await
    {
        Ok(opt) => opt,
        Err(_) => return Ok(token.to_string()),
    };
    let Some(dial) = lookup else {
        return Ok(token.to_string());
    };
    // Preserve the operator's user override if any: substitute
    // user@<bare-host-from-dial> when the dial target carries no user,
    // else use the dial target verbatim.
    if let Some(user) = override_user {
        let (_, dial_host) = parse_user_host(&dial);
        Ok(format!("{user}@{dial_host}"))
    } else {
        Ok(dial)
    }
}

/// Look up an integer index in the `isd ssh hosts` cache and return
/// the dial target (or hostname fallback) for that row.
fn resolve_index_to_dial(idx: usize) -> Result<String> {
    let cache =
        crate::index_cache::read_for("ssh hosts").context("reading ssh hosts index cache")?;
    let cache = cache
        .ok_or_else(|| anyhow!("no recent `isd ssh hosts` to resolve index {idx}; run it first"))?;
    if cache.is_stale() {
        eprintln!(
            "isd: ssh hosts index cache is {} seconds old; run `isd ssh hosts` to refresh",
            cache.age_secs()
        );
    }
    let row = cache.rows.iter().find(|r| r.index == idx).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::index_cache::out_of_range_message(idx, "isd ssh hosts", cache.rows.len())
        )
    })?;
    Ok(row.container_id.clone())
}

/// Interactive host picker. Fetches Isengard-enrolled hosts from the
/// controller, merges with the local `trusted_hosts.toml`, then opens
/// the native ratatui picker. The picked hostname is fed back into
/// `run_dial` so the cert refresh + `ssh` exec path stays in one
/// place.
///
/// # Errors
///
/// Returns `Err` on HTTP failure, controller-less context, trusted
/// store I/O failure, picker draw failure, or when the picker returns
/// no selection (Esc / Ctrl-C / empty fleet).
async fn run_picker(context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/hosts");
    let isengard_rows: Vec<HostRow> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("listing hosts for picker")?
        .json()
        .await
        .context("decoding hosts JSON for picker")?;
    let isengard: Vec<picker::PickerRow> = isengard_rows
        .into_iter()
        .map(|h| picker::PickerRow {
            hostname: h.hostname,
            class: picker::HostClass::Isengard,
            last_seen: h
                .last_seen_at
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            dial_target: h.dial_target,
        })
        .collect();

    let trusted_path = trusted_hosts::TrustedHostsFile::default_path()?;
    let store = trusted_hosts::TrustedHostsFile::load(&trusted_path)?;
    let trusted: Vec<picker::PickerRow> = store
        .entries
        .into_iter()
        .map(|t| {
            let dial_target = match (t.ssh_user.as_deref(), t.ssh_port) {
                (Some(user), Some(port)) => Some(format!("{user}@{}:{port}", t.hostname)),
                (Some(user), None) => Some(format!("{user}@{}", t.hostname)),
                (None, Some(port)) => Some(format!("{}:{port}", t.hostname)),
                (None, None) => None,
            };
            picker::PickerRow {
                hostname: t.hostname,
                class: picker::HostClass::Trusted,
                last_seen: None,
                dial_target,
            }
        })
        .collect();

    let rows = picker::merge_sources(isengard, trusted);
    if rows.is_empty() {
        return Err(anyhow!(
            "no hosts to pick from. enroll an agent or `isd ssh trust <host>`"
        ));
    }
    let picked = picker::pick(rows).await?;
    let host = picked.ok_or_else(|| anyhow!("no host selected"))?;
    Box::pin(run_dial(vec![host], context)).await
}

/// Return true when the cert at `cert_path` carries every principal in
/// `want`. Used in `run_dial` to force a re-mint when the operator
/// overrides the user with a name not already in the existing cert
/// (e.g. cached cert covers `[dirdmaster]`, dial is `root@host`).
fn cert_covers_principals(cert_path: &Path, want: &[String]) -> bool {
    let Ok(raw) = ssh_keygen_dump(cert_path) else {
        return false;
    };
    let Ok(parsed) = parse_keygen_dump(&raw) else {
        return false;
    };
    want.iter()
        .all(|w| parsed.principals.iter().any(|p| p == w))
}

/// Mint a fresh cert and write it next to the pubkey. Returns the
/// issuance response so the caller can print a confirmation summary
/// (or skip it, in the auto-mint path).
///
/// # Errors
///
/// Returns `Err` on missing pubkey, controller-less context, HTTP
/// failure, JSON decode failure, or write-to-disk failure.
async fn run_mint(args: MintArgs, context: Option<&str>) -> Result<MintReceipt> {
    ensure_local_keypair()?;
    let pubkey_path = match &args.pubkey {
        Some(p) => p.clone(),
        None => default_pubkey_path()?,
    };
    let pubkey = std::fs::read_to_string(&pubkey_path)
        .with_context(|| format!("reading pubkey at {}", pubkey_path.display()))?;
    let pubkey_trimmed = pubkey.trim().to_string();

    let principals = if args.principals.is_empty() {
        vec![default_user()]
    } else {
        args.principals.clone()
    };
    let comment = args.comment.clone().unwrap_or_else(default_comment);
    let body = IssueBody {
        pubkey: &pubkey_trimmed,
        principals: &principals,
        ttl_seconds: args.ttl,
        comment: &comment,
    };

    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/ssh/cert");
    let resp = session
        .client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(anyhow!("POST {url} -> {status}: {txt}"));
    }
    let issued: IssueResponse = resp.json().await.context("decoding cert response")?;

    let cert_path = cert_path_for(&pubkey_path);
    std::fs::write(&cert_path, &issued.certificate)
        .with_context(|| format!("writing cert to {}", cert_path.display()))?;

    Ok(MintReceipt {
        cert_path,
        fingerprint: issued.pubkey_fingerprint,
        ttl_seconds: issued.ttl_seconds,
    })
}

/// Pretty-printed receipt for the mint path. The auto-mint path
/// surfaces this on stderr so the dial output stays clean; the
/// explicit `isd ssh mint` path prints to stdout.
#[derive(Debug)]
struct MintReceipt {
    /// Path on disk where the signed cert was written.
    cert_path: PathBuf,
    /// SHA-256 fingerprint of the bound pubkey.
    fingerprint: String,
    /// Effective TTL in seconds after server-side capping.
    ttl_seconds: u64,
}

/// Emit a one-line confirmation summary after a successful mint.
fn print_mint_summary(r: &MintReceipt) {
    println!(
        "issued cert {} (fingerprint {}, ttl {}s)",
        r.cert_path.display(),
        r.fingerprint,
        r.ttl_seconds
    );
}

/// Show the current local cert. Reads `~/.ssh/id_*-cert.pub`, parses
/// via `ssh-keygen -L`, and prints the columns in the existing isd
/// table style. Exits non-zero when no cert is present or when the
/// cert is already expired.
///
/// # Errors
///
/// Returns `Err` when no cert is found, when `ssh-keygen -L` cannot
/// be invoked, or when the cert is expired.
async fn run_status() -> Result<()> {
    let cert_path = locate_existing_cert()?
        .ok_or_else(|| anyhow!("no SSH cert found in ~/.ssh/id_*-cert.pub. run `isd ssh mint`."))?;
    let raw = ssh_keygen_dump(&cert_path)
        .with_context(|| format!("parsing cert at {}", cert_path.display()))?;
    let parsed = parse_keygen_dump(&raw)?;
    let now = Utc::now();
    let expired = parsed.valid_to <= now;

    let remaining = if expired {
        "expired".to_string()
    } else {
        let secs = (parsed.valid_to - now).num_seconds().max(0);
        format!("{secs}s")
    };
    let table = RenderTable {
        columns: vec![
            Column::new("FIELD", Align::Left, CellStyle::Dim, 6, 6),
            Column::new("VALUE", Align::Left, CellStyle::Plain, 1, 12),
        ],
        rows: vec![
            vec!["PATH".into(), cert_path.display().to_string()],
            vec!["FINGERPRINT".into(), parsed.fingerprint.clone()],
            vec!["PRINCIPALS".into(), parsed.principals.join(",")],
            vec!["KEY ID".into(), parsed.key_id.clone()],
            vec![
                "VALID FROM".into(),
                parsed.valid_from.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ],
            vec![
                "VALID TO".into(),
                parsed.valid_to.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ],
            vec!["REMAINING".into(), remaining.clone()],
        ],
    };
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        println!("{}", render(&table, width, console::colors_enabled()));
    } else {
        println!("{}", render_plain(&table));
    }

    if expired {
        return Err(anyhow!(
            "cert is expired. run `isd ssh mint` for a fresh one."
        ));
    }
    Ok(())
}

/// List the hosts the controller knows about. Renders the same
/// columns as `isd hosts ls` plus a hint at the default principal
/// to ssh as.
///
/// # Errors
///
/// Returns `Err` on HTTP failure, controller-less context, or JSON
/// decode error.
async fn run_hosts(context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/hosts");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let rows: Vec<HostRow> = resp
        .error_for_status()
        .context("listing hosts")?
        .json()
        .await
        .context("decoding hosts JSON")?;
    // Write the index cache before rendering so a downstream
    // `isd ssh <N>` reads the freshest set.
    write_hosts_index_cache(&rows, &session.context.name);
    print_hosts_table(&rows);
    Ok(())
}

/// Override the operator-facing dial target and / or host name on an
/// enrolled host.
///
/// Resolves `args.agent` against the controller's host list (matches
/// against hostname, host id ULID, dial_target, or a `#N` index from
/// the last `isd ssh hosts` render), then PATCHes the row with
/// whichever fields the operator passed. `--dial` / `--clear`
/// targets the dial column; `--name` targets the hostname column.
/// Both can ship in a single invocation.
///
/// # Errors
///
/// Returns `Err` when no patchable flag is passed, when `--dial` +
/// `--clear` collide, when `--name ""` is passed (the server rejects
/// empty hostnames), when the agent token does not resolve to a host
/// row, or when the PATCH HTTP call fails.
async fn run_hosts_set(args: HostsSetArgs, context: Option<&str>) -> Result<()> {
    if args.dial.is_some() && args.clear {
        return Err(anyhow!(
            "isd ssh hosts set: pass either --dial <target> or --clear, not both"
        ));
    }
    // Compute the dial value: Some("") means "clear", Some(t) means
    // "set to t", None means "leave the dial column alone".
    let dial_value: Option<String> = match (&args.dial, args.clear) {
        (Some(t), false) => Some(t.clone()),
        (None, true) => Some(String::new()),
        (None, false) => None,
        (Some(_), true) => unreachable!("guarded above"),
    };

    // Compute the name value: Some(n) means "set to n". Empty string
    // is rejected up front so we don't lose the operator's other
    // flags to a server-side 400.
    let name_value: Option<String> = match &args.name {
        Some(n) if n.is_empty() => {
            return Err(anyhow!(
                "isd ssh hosts set: --name cannot be empty; pass a non-empty value"
            ));
        }
        Some(n) => Some(n.clone()),
        None => None,
    };

    if dial_value.is_none() && name_value.is_none() {
        return Err(anyhow!(
            "isd ssh hosts set: pass --dial <target> to set the dial target, \
             --clear to remove it, or --name <name> to set the host name"
        ));
    }

    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/hosts");
    let hosts: Vec<HostRow> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("listing hosts for `isd ssh hosts set`")?
        .json()
        .await
        .context("decoding hosts JSON for `isd ssh hosts set`")?;

    let host_id = resolve_agent_token(&hosts, &args.agent)?;
    let patched = dial_target::patch_host_fields(
        &session.client,
        controller_url,
        &host_id,
        dial_value.as_deref(),
        name_value.as_deref(),
    )
    .await?;

    if let Some(name) = name_value.as_deref() {
        println!("set hostname on {} -> {}", patched.id, name);
    }
    match dial_value.as_deref() {
        Some("") => {
            println!(
                "cleared dial_target on {} ({})",
                patched.hostname, patched.id
            );
        }
        Some(t) => {
            println!(
                "set dial_target on {} ({}) -> {}",
                patched.hostname,
                patched.id,
                patched.dial_target.as_deref().unwrap_or(t)
            );
        }
        None => {}
    }
    Ok(())
}

/// Resolve an `isd ssh hosts set` agent token to a host id ULID.
///
/// Matches in this order:
/// 1. A `#N`-style integer (or bare integer) is looked up against the
///    last `isd ssh hosts` index cache.
/// 2. Otherwise we walk `hosts` and match against `hostname`, `id`,
///    or `dial_target` exactly.
///
/// Errors with an actionable message when no row matches.
fn resolve_agent_token(hosts: &[HostRow], token: &str) -> Result<String> {
    // Strip a leading `#` so `#3` and `3` both work.
    let bare = token.strip_prefix('#').unwrap_or(token);
    if let Ok(idx) = bare.parse::<usize>() {
        let cache =
            crate::index_cache::read_for("ssh hosts").context("reading ssh hosts index cache")?;
        let cache = cache.ok_or_else(|| {
            anyhow!(
                "no recent `isd ssh hosts` to resolve index {idx}; run it first so \
                 `isd ssh hosts set {token} ...` can resolve a numeric selector"
            )
        })?;
        let row = cache.rows.iter().find(|r| r.index == idx).ok_or_else(|| {
            anyhow!(
                "{}",
                crate::index_cache::out_of_range_message(idx, "isd ssh hosts", cache.rows.len())
            )
        })?;
        // The cache's `name` is the hostname; look it up in the host
        // list to recover the host id ULID for the PATCH route.
        let host = hosts
            .iter()
            .find(|h| h.hostname == row.name)
            .ok_or_else(|| {
                anyhow!(
                    "index {idx} pointed at {:?} but the controller no longer reports that host; \
                     run `isd ssh hosts` to refresh",
                    row.name
                )
            })?;
        return Ok(host.id.clone());
    }
    let host = hosts
        .iter()
        .find(|h| h.hostname == token || h.id == token || h.dial_target.as_deref() == Some(token))
        .ok_or_else(|| {
            anyhow!(
                "no host matched {token:?}; pass a hostname, the host id, the existing \
                 dial target, or a `#N` index from `isd ssh hosts`"
            )
        })?;
    Ok(host.id.clone())
}

/// Column layout for `isd ssh hosts`. Drops the `SSH USER` /
/// `PRINCIPAL` columns the operator no longer wants in the table:
/// the default user is surfaced once in a footer line under the box.
/// Columns in spec order: `#`, `NAME`, `DIAL TARGET`, `LAST SEEN`.
fn hosts_columns() -> Vec<Column> {
    vec![
        Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
        Column::new("NAME", Align::Left, CellStyle::Emphasis, 6, 8),
        // The dial target column is what the operator types into
        // ssh; emphasized so it sits visually next to NAME. shrink
        // priority sits below LAST SEEN so the timestamp stays
        // readable on narrow terminals.
        Column::new("DIAL TARGET", Align::Left, CellStyle::Plain, 5, 14),
        Column::new("LAST SEEN", Align::Left, CellStyle::Plain, 1, 12),
    ]
}

/// Build the row matrix for `isd ssh hosts`. Missing `last_seen_at`
/// renders as `-`. Missing `dial_target` renders as `(unset)` so the
/// column never collapses and the operator sees the action verb.
fn build_hosts_row_cells(rows: &[HostRow]) -> Vec<Vec<String>> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let last_seen = row
                .last_seen_at
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|| "-".into());
            let dial = row.dial_target.clone().unwrap_or_else(|| "(unset)".into());
            vec![i.to_string(), row.hostname.clone(), dial, last_seen]
        })
        .collect()
}

/// Footer hint that surfaces the default SSH user without consuming
/// table columns. Mirrors how `isd ssh <host>` actually dials.
fn hosts_footer_line(default_user: &str) -> String {
    format!("  SSH user: {default_user}   (override with `isd ssh user@<host>`)")
}

/// Render the host rows under `isd ssh hosts`. Empty input still
/// prints the header (and a header rule, on a TTY) so `wc -l` keeps a
/// stable shape and the operator sees the spec columns. A footer
/// line beneath the table calls out the default SSH user once.
fn print_hosts_table(rows: &[HostRow]) {
    let table = RenderTable {
        columns: hosts_columns(),
        rows: build_hosts_row_cells(rows),
    };
    let term = console::Term::stdout();
    let footer = hosts_footer_line(&default_user());
    if term.is_term() {
        let width = term.size().1 as usize;
        let color = console::colors_enabled();
        println!("{}", render(&table, width, color));
        println!("{footer}");
    } else {
        println!("{}", render_plain(&table));
        println!("{footer}");
    }
}

/// Build the index-cache rows for `isd ssh hosts`. Mirrors the
/// `ps.rs` pattern: render-order index + a stable identifier the dial
/// path can consume. We pack the dial target into `container_id` so
/// `isd ssh <N>` can read the cache via the same `IndexRow` schema
/// without bumping the on-disk format. `name` carries the
/// operator-visible label (the agent's reported hostname).
fn build_hosts_index_rows(rows: &[HostRow], context_name: &str) -> Vec<IndexRow> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| IndexRow {
            index: i,
            context: context_name.to_string(),
            // The dial path reads this column. Falls back to the
            // hostname so a `#N` lookup never hands an empty string
            // to `ssh`.
            container_id: row
                .dial_target
                .clone()
                .unwrap_or_else(|| row.hostname.clone()),
            name: row.hostname.clone(),
        })
        .collect()
}

/// Write the `isd ssh hosts` index cache so `isd ssh <N>` can resolve
/// a bare integer against the freshest render. Failures are a warning;
/// the table render already landed and a missing cache is recoverable
/// (`isd ssh hosts` again to refresh).
fn write_hosts_index_cache(rows: &[HostRow], context_name: &str) {
    let cache = IndexCache {
        captured_at: chrono::Utc::now(),
        command: "ssh hosts".to_string(),
        rows: build_hosts_index_rows(rows, context_name),
    };
    if let Err(e) = crate::index_cache::write(&cache) {
        eprintln!("isd: warning: could not write ssh hosts index cache: {e:#}");
    }
}

/// Fetch the controller's SSH CA pubkey and print it verbatim so it
/// can be piped into another machine's `TrustedUserCAKeys` file.
///
/// # Errors
///
/// Returns `Err` on HTTP failure, controller-less context, or JSON
/// decode error.
async fn run_ca_pubkey(context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/ssh/ca");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let body: CaPubkeyResponse = resp
        .error_for_status()
        .context("fetching SSH CA pubkey")?
        .json()
        .await
        .context("decoding SSH CA pubkey JSON")?;
    println!("{}", body.pubkey.trim_end());
    Ok(())
}

/// One row in the `GET /api/v1/ssh/audit` response. Mirrors the
/// dashboard's `SshAuditEntry`. Fields we do not render are tolerated
/// thanks to serde's default behaviour.
#[derive(Debug, Deserialize)]
struct AuditEntry {
    /// Event kind. Always `ssh.cert.*` for rows returned here.
    kind: String,
    /// RFC3339 timestamp in UTC.
    occurred_at: String,
    /// Decoded metadata payload (principals, fingerprint, ttl, etc.).
    #[serde(default)]
    metadata: serde_json::Value,
}

/// Fetch the SSH cert audit log and render it as a table.
///
/// # Errors
///
/// Returns `Err` on HTTP failure, controller-less context, or JSON
/// decode error.
async fn run_audit(args: AuditArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let mut url = format!("{controller_url}/api/v1/ssh/audit?limit={}", args.limit);
    if let Some(since) = &args.since {
        // URL-encode lightly: RFC3339 carries `:` and `+`, both of
        // which need percent-encoding to survive an HTTP query.
        let encoded = since
            .replace('+', "%2B")
            .replace(':', "%3A")
            .replace(' ', "%20");
        url.push_str(&format!("&since={encoded}"));
    }
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let entries: Vec<AuditEntry> = resp
        .error_for_status()
        .context("listing ssh audit")?
        .json()
        .await
        .context("decoding ssh audit JSON")?;
    print_audit_table(&entries);
    Ok(())
}

/// Column layout for `isd ssh audit`. Columns in spec order:
/// `WHEN`, `KIND`, `PRINCIPALS`, `FINGERPRINT`, `TTL`, `COMMENT`.
fn audit_columns() -> Vec<Column> {
    vec![
        Column::new("WHEN", Align::Left, CellStyle::Plain, 2, 20),
        Column::new("KIND", Align::Left, CellStyle::Plain, 5, 12),
        Column::new("PRINCIPALS", Align::Left, CellStyle::Plain, 4, 8),
        Column::new("FINGERPRINT", Align::Left, CellStyle::Dim, 3, 16),
        Column::new("TTL", Align::Right, CellStyle::Plain, 6, 4),
        Column::new("COMMENT", Align::Left, CellStyle::Plain, 1, 12),
    ]
}

/// Build the row matrix for `isd ssh audit`. Missing metadata fields
/// fall back to `-` so a malformed audit row still renders.
fn build_audit_row_cells(rows: &[AuditEntry]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            let when = format_audit_timestamp(&row.occurred_at);
            let principals = row
                .metadata
                .get("principals")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|| "-".into());
            let fingerprint = row
                .metadata
                .get("pubkey_fingerprint")
                .and_then(|v| v.as_str())
                .map(shorten_fingerprint)
                .unwrap_or_else(|| "-".into());
            let ttl = row
                .metadata
                .get("ttl_seconds")
                .and_then(|v| v.as_u64())
                .map(|s| format!("{s}s"))
                .unwrap_or_else(|| "-".into());
            let comment = row
                .metadata
                .get("comment")
                .and_then(|v| v.as_str())
                .unwrap_or("-")
                .to_string();
            vec![
                when,
                row.kind.clone(),
                principals,
                fingerprint,
                ttl,
                comment,
            ]
        })
        .collect()
}

/// Render the audit rows to stdout: boxed table on a TTY, tab-
/// separated plain on a pipe. Empty input still emits a header line
/// so pipeline consumers do not break on a quiet audit log.
fn print_audit_table(rows: &[AuditEntry]) {
    let table = RenderTable {
        columns: audit_columns(),
        rows: build_audit_row_cells(rows),
    };
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        let color = console::colors_enabled();
        println!("{}", render(&table, width, color));
    } else {
        println!("{}", render_plain(&table));
    }
}

/// Render the `occurred_at` timestamp as a compact UTC `Z` string. We
/// keep the second resolution because audit reads care about ordering
/// down to the issuance call, not sub-second precision. Falls back to
/// the raw string when parsing fails so a malformed row still renders.
fn format_audit_timestamp(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| {
            dt.with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
        .unwrap_or_else(|_| raw.to_string())
}

/// SHA-256 fingerprints are 50+ chars in OpenSSH's `SHA256:` form,
/// which blows out the table on narrow terminals. The first 16 chars
/// after the `SHA256:` prefix are unique enough for visual scanning
/// in a 50-row window. Operators who need the full value can pipe
/// through `jq` against the raw endpoint.
fn shorten_fingerprint(s: &str) -> String {
    let body = s.strip_prefix("SHA256:").unwrap_or(s);
    let short: String = body.chars().take(16).collect();
    if body.len() > 16 {
        format!("SHA256:{short}...")
    } else {
        format!("SHA256:{short}")
    }
}

/// Generate an ed25519 keypair at `privkey_path` if it does not exist.
/// No-op if the file is already present: never clobbers an existing
/// key. The pubkey is written by `ssh-keygen` to `<privkey_path>.pub`.
///
/// # Errors
///
/// Returns `Err` when `mkdir -p` of the parent directory fails, when
/// `ssh-keygen` cannot be spawned, or when `ssh-keygen` exits non-zero.
fn ensure_keypair_at(privkey_path: &Path) -> Result<()> {
    if privkey_path.exists() {
        return Ok(());
    }
    if let Some(parent) = privkey_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let comment = format!("operator@{} (isd-generated)", hostname_string());
    let status = std::process::Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(privkey_path)
        .arg("-C")
        .arg(&comment)
        .status()
        .context("spawning ssh-keygen for bootstrap")?;
    if !status.success() {
        return Err(anyhow!(
            "ssh-keygen -t ed25519 -f {} -> {}",
            privkey_path.display(),
            status
        ));
    }
    Ok(())
}

/// Ensure `~/.ssh/id_ed25519` exists, generating one when missing.
/// Called at the top of `run_dial` and `run_mint` so a fresh laptop
/// dialing the first time auto-bootstraps its keypair before the mint
/// HTTP call tries to read the pubkey.
fn ensure_local_keypair() -> Result<()> {
    let home = dirs::home_dir().context("home dir not found")?;
    let privkey = home.join(".ssh").join("id_ed25519");
    ensure_keypair_at(&privkey)
}

/// Default Unix user for the operator's laptop. Tries
/// `whoami::username()` first, falls back to the `$USER` env var, then
/// to an empty string. Callers in the dial/mint paths fail loudly when
/// the result is empty so the operator gets an actionable error
/// instead of a `ssh @host` invocation.
pub(super) fn default_user() -> String {
    let w = whoami::username();
    if !w.is_empty() {
        return w;
    }
    std::env::var("USER").unwrap_or_default()
}

/// Resolve the principal list for a mint call. Always includes the
/// default user; when the operator dialed `user@host`, the override is
/// appended (deduped against the default and any empty value). One
/// cert minted with `[default, override]` covers both `isd ssh host`
/// and `isd ssh override@host` during the TTL window. Audit identity
/// lives in `key_id`, not in the principal list, so multi-principal
/// certs do not blur attribution.
fn effective_principals(override_user: Option<&str>, default: &str) -> Vec<String> {
    let mut out = vec![default.to_string()];
    if let Some(o) = override_user
        && !o.is_empty()
        && o != default
    {
        out.push(o.to_string());
    }
    out
}

/// Split a `user@host` token into `(Some(user), host)`. A bare `host`
/// returns `(None, host)`. An empty user (`@host`) is also reported as
/// `None` so the dial path falls back to the default user instead of
/// shelling out `ssh @host` with no principal.
///
/// The last `@` is the splitter: this matches what OpenSSH does and
/// tolerates email-shaped principals correctly.
fn parse_user_host(token: &str) -> (Option<String>, String) {
    if let Some(idx) = token.rfind('@') {
        let user = &token[..idx];
        let host = &token[idx + 1..];
        if user.is_empty() {
            (None, host.to_string())
        } else {
            (Some(user.to_string()), host.to_string())
        }
    } else {
        (None, token.to_string())
    }
}

/// Default pubkey path: `~/.ssh/id_ed25519.pub`.
fn default_pubkey_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("home dir not found")?;
    Ok(home.join(".ssh").join("id_ed25519.pub"))
}

/// Default comment: `operator@<hostname> <UTC ISO8601>`. The hostname
/// is the local OS hostname, not the remote target: this identifies
/// the laptop the cert was minted on, which is what shows up in the
/// controller's audit log.
fn default_comment() -> String {
    let host = hostname_string();
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    format!("operator@{host} {now}")
}

/// Best-effort local hostname. Falls back to `unknown` so we never
/// panic on weird envs (CI, minimal containers).
fn hostname_string() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

/// Translate a pubkey path into the matching cert path. OpenSSH's
/// convention is `id_ed25519.pub` -> `id_ed25519-cert.pub`: same
/// stem, with `-cert` inserted before the `.pub` suffix. `ssh -i
/// id_ed25519` then picks the cert up automatically without a
/// `CertificateFile` override.
fn cert_path_for(pubkey_path: &Path) -> PathBuf {
    let parent = pubkey_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = pubkey_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "id".into());
    parent.join(format!("{stem}-cert.pub"))
}

/// Walk `~/.ssh/` looking for `id_*-cert.pub` files. Returns the
/// freshest match (by mtime). `Ok(None)` when no cert exists; `Err`
/// only when `~/.ssh/` itself cannot be opened.
fn locate_existing_cert() -> Result<Option<PathBuf>> {
    let home = dirs::home_dir().context("home dir not found")?;
    let ssh_dir = home.join(".ssh");
    let mut entries = match std::fs::read_dir(&ssh_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading {}", ssh_dir.display())));
        }
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Some(entry) = entries.next().transpose()? {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !(name.starts_with("id_") && name.ends_with("-cert.pub")) {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).ok();
        match (&best, modified) {
            (None, Some(m)) => best = Some((m, path)),
            (Some((cur, _)), Some(m)) if m > *cur => best = Some((m, path)),
            _ => {}
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Decide whether the local cert at `cert_path` needs refreshing.
/// Stale when: the cert file does not exist, when `ssh-keygen -L`
/// fails to parse it, or when the cert's `Valid: ... to <when>`
/// timestamp is within 5 minutes of now (or already past).
fn cert_is_stale(cert_path: &Path) -> bool {
    if !cert_path.exists() {
        return true;
    }
    let raw = match ssh_keygen_dump(cert_path) {
        Ok(s) => s,
        Err(_) => return true,
    };
    let parsed = match parse_keygen_dump(&raw) {
        Ok(p) => p,
        Err(_) => return true,
    };
    let cutoff = Utc::now() + chrono::Duration::minutes(5);
    parsed.valid_to <= cutoff
}

/// Run `ssh-keygen -L -f <cert>` and return its stdout. Surfaces a
/// useful error chain when ssh-keygen is missing or returns non-zero.
fn ssh_keygen_dump(cert_path: &Path) -> Result<String> {
    let output = std::process::Command::new("ssh-keygen")
        .arg("-L")
        .arg("-f")
        .arg(cert_path)
        .output()
        .context("spawning ssh-keygen")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(anyhow!(
            "ssh-keygen -L -f {} -> {}: {}",
            cert_path.display(),
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parsed `ssh-keygen -L` output. Only the fields the CLI surfaces
/// are captured; the rest are dropped.
#[derive(Debug, PartialEq, Eq)]
struct ParsedCert {
    /// SHA-256 fingerprint of the cert's public key.
    fingerprint: String,
    /// Free-form key id baked into the cert (matches the `comment`
    /// passed to the dashboard).
    key_id: String,
    /// Principals (Unix usernames) the cert is valid for.
    principals: Vec<String>,
    /// Lower bound of the cert's validity window.
    valid_from: DateTime<Utc>,
    /// Upper bound of the cert's validity window.
    valid_to: DateTime<Utc>,
}

/// Parse the `ssh-keygen -L` plain-text dump.
///
/// The format is stable across OpenSSH releases (the same parser
/// covers 7.x through 9.x output). Lines we care about:
///
/// ```text
///         Public key: ED25519-CERT SHA256:abc...
///         Key ID: "operator@host 2026-05-21T..."
///         Valid: from 2026-05-21T10:00:00 to 2026-05-21T11:00:00
///         Principals:
///                 isengard
/// ```
///
/// We do not require every line. Missing principals is allowed (the
/// cert is then unusable on agent hosts but `isd ssh status` still
/// prints what it found). Missing or unparseable `Valid:` is fatal:
/// without it we cannot decide whether the cert is stale.
fn parse_keygen_dump(raw: &str) -> Result<ParsedCert> {
    let mut fingerprint = String::new();
    let mut key_id = String::new();
    let mut principals: Vec<String> = Vec::new();
    let mut valid_from: Option<DateTime<Utc>> = None;
    let mut valid_to: Option<DateTime<Utc>> = None;

    let mut in_principals = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Public key:") {
            // "Public key: ED25519-CERT SHA256:..."
            // Capture the SHA256: token onward as the fingerprint.
            if let Some(idx) = trimmed.find("SHA256:") {
                fingerprint = trimmed[idx..].trim().to_string();
            }
            in_principals = false;
        } else if let Some(rest) = trimmed.strip_prefix("Key ID:") {
            // `Key ID: "operator@host 2026-..."` -> strip the
            // surrounding quotes if present.
            key_id = rest.trim().trim_matches('"').to_string();
            in_principals = false;
        } else if let Some(rest) = trimmed.strip_prefix("Valid:") {
            // `Valid: from 2026-... to 2026-...` (or `forever`).
            let (from, to) = parse_validity_line(rest.trim())?;
            valid_from = Some(from);
            valid_to = Some(to);
            in_principals = false;
        } else if trimmed.starts_with("Principals:") {
            in_principals = true;
        } else if in_principals {
            // Principals appear indented on their own lines until a
            // blank line or a new field starts. Section headers come
            // back unindented in `ssh-keygen -L` output:
            //
            //         Principals:
            //                 isengard
            //         Critical Options: (none)
            //
            // The principal line carries deeper indentation (8 vs
            // 16 leading spaces), so the simplest robust check is
            // "does this raw line carry one of the known field
            // labels?" rather than guessing from punctuation. The
            // labels are stable across OpenSSH versions. A blank
            // line, a `(none)` marker, or any of the known field
            // labels ends the principals block.
            const FIELD_LABELS: &[&str] = &[
                "Type:",
                "Public key:",
                "Signing CA:",
                "Key ID:",
                "Serial:",
                "Valid:",
                "Principals:",
                "Critical Options:",
                "Extensions:",
            ];
            let ends_block = trimmed.is_empty()
                || trimmed.starts_with("(none)")
                || FIELD_LABELS.iter().any(|label| trimmed.starts_with(label));
            if ends_block {
                in_principals = false;
            } else {
                principals.push(trimmed.to_string());
            }
        }
    }
    let valid_from =
        valid_from.ok_or_else(|| anyhow!("ssh-keygen output missing `Valid:` line"))?;
    let valid_to = valid_to.ok_or_else(|| anyhow!("ssh-keygen output missing `Valid:` to"))?;
    Ok(ParsedCert {
        fingerprint,
        key_id,
        principals,
        valid_from,
        valid_to,
    })
}

/// Parse `from <ts> to <ts>` from the `Valid:` line. OpenSSH writes
/// the timestamps in local time without a zone marker (e.g.
/// `2026-05-21T10:00:00`), so we interpret them as UTC: agents and
/// the controller both run with `TZ=UTC` per the deploy compose, and
/// the only consumer is the staleness check (a 5-minute fuzz absorbs
/// any rare drift on operator laptops in non-UTC zones).
fn parse_validity_line(rest: &str) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let after_from = rest
        .strip_prefix("from ")
        .ok_or_else(|| anyhow!("ssh-keygen Valid line missing `from`: {rest:?}"))?;
    let (from_str, to_str) = after_from
        .split_once(" to ")
        .ok_or_else(|| anyhow!("ssh-keygen Valid line missing ` to `: {rest:?}"))?;
    let from = parse_keygen_timestamp(from_str.trim())?;
    let to = parse_keygen_timestamp(to_str.trim())?;
    Ok((from, to))
}

/// Parse a single OpenSSH `Valid:` timestamp. Accepts the bare
/// `YYYY-MM-DDTHH:MM:SS` shape OpenSSH emits without a zone, and
/// also the modern `YYYY-MM-DDTHH:MM:SS+00:00` shape some forks add.
fn parse_keygen_timestamp(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(s) {
        return Ok(parsed.with_timezone(&Utc));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .with_context(|| format!("parsing ssh-keygen timestamp {s:?}"))?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_host_user_at_host() {
        assert_eq!(
            parse_user_host("root@edge-1"),
            (Some("root".into()), "edge-1".into())
        );
    }

    #[test]
    fn parse_user_host_bare_host() {
        assert_eq!(parse_user_host("edge-1"), (None, "edge-1".into()));
    }

    #[test]
    fn parse_user_host_dotted_host() {
        assert_eq!(
            parse_user_host("u@a.b.c"),
            (Some("u".into()), "a.b.c".into())
        );
    }

    #[test]
    fn parse_user_host_ipv4() {
        assert_eq!(
            parse_user_host("u@1.2.3.4"),
            (Some("u".into()), "1.2.3.4".into())
        );
    }

    #[test]
    fn effective_principals_no_override_returns_default() {
        let p = effective_principals(None, "dirdmaster");
        assert_eq!(p, vec!["dirdmaster".to_string()]);
    }

    #[test]
    fn effective_principals_with_override_returns_both() {
        let p = effective_principals(Some("root"), "dirdmaster");
        assert_eq!(p, vec!["dirdmaster".to_string(), "root".to_string()]);
    }

    #[test]
    fn effective_principals_dedup_when_override_matches_default() {
        let p = effective_principals(Some("dirdmaster"), "dirdmaster");
        assert_eq!(p, vec!["dirdmaster".to_string()]);
    }

    #[test]
    fn effective_principals_drops_empty_override() {
        let p = effective_principals(Some(""), "dirdmaster");
        assert_eq!(p, vec!["dirdmaster".to_string()]);
    }

    /// Skip ssh-keygen-driven tests when the binary is absent so CI
    /// envs without it (sandboxed builders) pass cleanly. The function
    /// is still exercised on the operator's laptop where ssh-keygen
    /// always exists.
    fn ssh_keygen_available() -> bool {
        which::which("ssh-keygen").is_ok()
    }

    #[test]
    fn ensure_keypair_at_generates_when_missing() {
        if !ssh_keygen_available() {
            eprintln!("skip: ssh-keygen not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let privkey = dir.path().join("id_ed25519");
        let pubkey = dir.path().join("id_ed25519.pub");
        ensure_keypair_at(&privkey).expect("generate");
        assert!(privkey.exists(), "private key not written");
        assert!(pubkey.exists(), "public key not written");
    }

    #[test]
    fn ensure_keypair_at_noop_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let privkey = dir.path().join("id_ed25519");
        std::fs::write(&privkey, "fake key bytes").unwrap();
        let mtime_before = std::fs::metadata(&privkey).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        ensure_keypair_at(&privkey).expect("noop");
        let mtime_after = std::fs::metadata(&privkey).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "file was overwritten");
        // Body untouched.
        let body = std::fs::read_to_string(&privkey).unwrap();
        assert_eq!(body, "fake key bytes");
    }

    #[test]
    fn cert_covers_principals_returns_false_when_cert_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope-cert.pub");
        assert!(!cert_covers_principals(&path, &["root".into()]));
    }

    #[test]
    fn default_user_yields_non_empty_in_a_normal_env() {
        // Exact value depends on the env, but every reasonable
        // dev/CI host has either `whoami::username()` or `$USER` set.
        let u = default_user();
        assert!(!u.is_empty(), "default_user returned empty");
    }

    #[test]
    fn parse_user_host_empty_user_treated_as_none() {
        // `@host` form: last-@ splitter yields user="". The function
        // reports that as `None` so the caller substitutes the default
        // user instead of attempting to ssh with an empty username.
        let (u, h) = parse_user_host("@host");
        assert_eq!(h, "host");
        assert!(u.is_none());
    }

    fn fixture_valid() -> String {
        // Minted with `ssh-keygen -L -f`. Stable across OpenSSH
        // releases: every line we parse is canonical OpenSSH output.
        let now = Utc::now() + chrono::Duration::hours(1);
        let from = (Utc::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let to = now.format("%Y-%m-%dT%H:%M:%S").to_string();
        format!(
            "/tmp/id_ed25519-cert.pub:\n        Type: ssh-ed25519-cert-v01@openssh.com user certificate\n        Public key: ED25519-CERT SHA256:abcdef0123456789\n        Signing CA: ED25519 SHA256:fedcba9876543210 (using ssh-ed25519)\n        Key ID: \"operator@laptop 2026-05-21T10:00:00Z\"\n        Serial: 0\n        Valid: from {from} to {to}\n        Principals: \n                isengard\n        Critical Options: (none)\n        Extensions: \n                permit-X11-forwarding\n                permit-agent-forwarding\n",
        )
    }

    fn fixture_expired() -> String {
        // Minted with the same shape but valid_to in the past.
        let from = (Utc::now() - chrono::Duration::hours(2))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let to = (Utc::now() - chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        format!(
            "/tmp/id_ed25519-cert.pub:\n        Type: ssh-ed25519-cert-v01@openssh.com user certificate\n        Public key: ED25519-CERT SHA256:abcdef0123456789\n        Signing CA: ED25519 SHA256:fedcba9876543210 (using ssh-ed25519)\n        Key ID: \"operator@laptop 2026-05-21T10:00:00Z\"\n        Serial: 0\n        Valid: from {from} to {to}\n        Principals: \n                isengard\n        Critical Options: (none)\n",
        )
    }

    #[test]
    fn parse_keygen_dump_extracts_fields() {
        let parsed = parse_keygen_dump(&fixture_valid()).expect("parse valid dump");
        assert_eq!(parsed.fingerprint, "SHA256:abcdef0123456789");
        assert_eq!(parsed.key_id, "operator@laptop 2026-05-21T10:00:00Z");
        assert_eq!(parsed.principals, vec!["isengard".to_string()]);
        assert!(parsed.valid_to > parsed.valid_from);
    }

    #[test]
    fn parse_keygen_dump_handles_rfc3339_timestamps() {
        let raw = "/tmp/id.pub:\n        Public key: ED25519-CERT SHA256:zzz\n        Key ID: \"ok\"\n        Valid: from 2026-05-21T10:00:00+00:00 to 2026-05-21T11:00:00+00:00\n        Principals: \n                isengard\n";
        let parsed = parse_keygen_dump(raw).expect("parse rfc3339 timestamps");
        assert_eq!(parsed.fingerprint, "SHA256:zzz");
    }

    #[test]
    fn parse_keygen_dump_errors_on_missing_valid_line() {
        let raw =
            "/tmp/id.pub:\n        Public key: ED25519-CERT SHA256:xxx\n        Key ID: \"ok\"\n";
        let err = parse_keygen_dump(raw).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Valid"), "msg: {msg}");
    }

    #[test]
    fn cert_is_stale_treats_missing_file_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope-cert.pub");
        assert!(cert_is_stale(&path));
    }

    #[test]
    fn cert_path_for_pairs_pubkey_to_cert_path() {
        let pub_path = PathBuf::from("/home/x/.ssh/id_ed25519.pub");
        let cert = cert_path_for(&pub_path);
        assert_eq!(cert, PathBuf::from("/home/x/.ssh/id_ed25519-cert.pub"));
    }

    #[test]
    fn cert_path_for_handles_other_key_types() {
        let pub_path = PathBuf::from("/home/x/.ssh/id_rsa.pub");
        let cert = cert_path_for(&pub_path);
        assert_eq!(cert, PathBuf::from("/home/x/.ssh/id_rsa-cert.pub"));
    }

    #[test]
    fn mint_args_default_has_one_hour_ttl_and_default_user_principal() {
        let a = MintArgs::default();
        assert_eq!(a.ttl, 3600);
        assert!(a.pubkey.is_none());
        assert_eq!(a.principals, vec![default_user()]);
        assert!(a.comment.is_none());
    }

    #[test]
    fn default_comment_carries_hostname_and_utc_timestamp() {
        let c = default_comment();
        assert!(c.starts_with("operator@"), "comment: {c}");
        assert!(
            c.contains(' '),
            "comment carries a timestamp after the host"
        );
    }

    #[test]
    fn parse_fixture_expired_cert_yields_past_valid_to() {
        let parsed = parse_keygen_dump(&fixture_expired()).expect("parse expired dump");
        assert!(parsed.valid_to < Utc::now());
    }

    /// Helper: render the hosts table via the unified renderer's
    /// non-TTY decay path so tests assert against stable plain text.
    fn hosts_plain(rows: &[HostRow]) -> String {
        let table = RenderTable {
            columns: hosts_columns(),
            rows: build_hosts_row_cells(rows),
        };
        render_plain(&table)
    }

    /// `isd ssh hosts` headers match the PR B spec: `#`, `NAME`,
    /// `DIAL TARGET`, `LAST SEEN`. The legacy `SSH USER` / `PRINCIPAL`
    /// columns are gone (the default user lives in the footer).
    #[test]
    fn render_hosts_table_has_spec_columns_only() {
        let t = hosts_plain(&[]);
        let header = t.lines().next().unwrap();
        // `render_plain` drops the `#` column by contract; the
        // remaining columns are `NAME\tDIAL TARGET\tLAST SEEN`.
        assert_eq!(header, "NAME\tDIAL TARGET\tLAST SEEN");
        // The legacy columns are gone.
        assert!(!t.contains("SSH USER"));
        assert!(!t.contains("PRINCIPAL"));
    }

    /// Boxed render carries the `#` column (TTY affordance) and all
    /// four spec headers in order.
    #[test]
    fn render_hosts_table_boxed_carries_index_column() {
        let table = RenderTable {
            columns: hosts_columns(),
            rows: build_hosts_row_cells(&[]),
        };
        let boxed = render(&table, 200, false);
        assert!(boxed.contains('╭'));
        assert!(boxed.contains("#"));
        assert!(boxed.contains("NAME"));
        assert!(boxed.contains("DIAL TARGET"));
        assert!(boxed.contains("LAST SEEN"));
    }

    /// Each input row surfaces in the rendered output with the
    /// hostname, the formatted timestamp, and the `-` placeholder
    /// when `last_seen_at` is missing. The dial target column carries
    /// the operator's address when set and `(unset)` when missing.
    #[test]
    fn render_hosts_table_renders_each_row() {
        let rows = vec![
            HostRow {
                id: "01HEDGE1".into(),
                hostname: "iso-edge-1".into(),
                last_seen_at: Some(
                    chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 21, 10, 0, 0).unwrap(),
                ),
                dial_target: Some("dirdmaster@10.17.0.125".into()),
            },
            HostRow {
                id: "01HEDGE2".into(),
                hostname: "iso-edge-2".into(),
                last_seen_at: None,
                dial_target: None,
            },
        ];
        let t = hosts_plain(&rows);
        assert!(t.contains("iso-edge-1"));
        assert!(t.contains("iso-edge-2"));
        assert!(t.contains("2026-05-21T10:00:00Z"));
        assert!(t.contains("dirdmaster@10.17.0.125"));
        assert!(t.contains("(unset)"));
        // The dash placeholder lives in the LAST SEEN column for the
        // host with no heartbeat.
        let lines: Vec<&str> = t.lines().collect();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("iso-edge-2\t") && l.contains('-')),
            "dash placeholder rendered: {t}"
        );
    }

    /// The footer line surfaces the default user and the override
    /// recipe so the operator sees how `isd ssh <host>` will dial
    /// without consuming a table column for it.
    #[test]
    fn hosts_footer_calls_out_default_user_and_override_recipe() {
        let line = hosts_footer_line("dirdmaster");
        assert!(line.contains("SSH user: dirdmaster"), "footer: {line}");
        assert!(line.contains("isd ssh user@<host>"), "footer: {line}");
    }

    fn sample_hosts() -> Vec<HostRow> {
        vec![
            HostRow {
                id: "01HEDGE1".into(),
                hostname: "edge-1".into(),
                last_seen_at: None,
                dial_target: Some("dirdmaster@10.17.0.125".into()),
            },
            HostRow {
                id: "01HEDGE2".into(),
                hostname: "edge-2".into(),
                last_seen_at: None,
                dial_target: None,
            },
        ]
    }

    /// `build_hosts_index_rows` packs the dial target into
    /// `container_id` so `isd ssh <N>` can resolve straight to the
    /// dialable address. Hosts with no dial target fall back to the
    /// hostname so the dial still works.
    #[test]
    fn index_rows_pack_dial_target_then_fall_back_to_hostname() {
        let rows = build_hosts_index_rows(&sample_hosts(), "lausanne");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 0);
        assert_eq!(rows[0].context, "lausanne");
        assert_eq!(rows[0].container_id, "dirdmaster@10.17.0.125");
        assert_eq!(rows[0].name, "edge-1");
        // Row 1 has no dial target: fall back to hostname.
        assert_eq!(rows[1].container_id, "edge-2");
        assert_eq!(rows[1].name, "edge-2");
    }

    /// `resolve_agent_token` accepts hostname, host id, and the
    /// existing dial target as selectors. Numeric tokens are handled
    /// by a separate index-cache code path tested independently.
    #[test]
    fn resolve_agent_token_matches_hostname_id_and_dial_target() {
        let hosts = sample_hosts();
        assert_eq!(resolve_agent_token(&hosts, "edge-1").unwrap(), "01HEDGE1");
        assert_eq!(resolve_agent_token(&hosts, "01HEDGE2").unwrap(), "01HEDGE2");
        assert_eq!(
            resolve_agent_token(&hosts, "dirdmaster@10.17.0.125").unwrap(),
            "01HEDGE1"
        );
    }

    /// `resolve_agent_token` errors with an actionable message when
    /// nothing matches.
    #[test]
    fn resolve_agent_token_errors_with_actionable_message() {
        let hosts = sample_hosts();
        let err = resolve_agent_token(&hosts, "ghost").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no host matched"), "{msg}");
        assert!(msg.contains("hostname"), "{msg}");
    }

    /// `isd ssh hosts` is parsed by clap as the bare `Hosts(HostsArgs)`
    /// variant with `command: None`. Guards against an accidental
    /// regression where the new `HostsArgs` indirection makes plain
    /// `isd ssh hosts` ambiguous.
    #[test]
    fn ssh_hosts_bare_parses_to_list() {
        use clap::Parser;
        // Build a tiny parser that mirrors the top-level dispatch path.
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SshCommand,
        }
        let parsed = Wrap::try_parse_from(["wrap", "hosts"]).expect("parses");
        match parsed.cmd {
            SshCommand::Hosts(HostsArgs { command: None }) => {}
            other => panic!("expected Hosts(no subcmd), got {other:?}"),
        }
    }

    /// `isd ssh hosts set <agent> --dial <target>` parses cleanly via
    /// the nested HostsCommand::Set variant.
    #[test]
    fn ssh_hosts_set_parses_with_dial_flag() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SshCommand,
        }
        let parsed =
            Wrap::try_parse_from(["wrap", "hosts", "set", "edge-1", "--dial", "op@10.0.0.7"])
                .expect("parses");
        match parsed.cmd {
            SshCommand::Hosts(HostsArgs {
                command: Some(HostsCommand::Set(args)),
            }) => {
                assert_eq!(args.agent, "edge-1");
                assert_eq!(args.dial.as_deref(), Some("op@10.0.0.7"));
                assert!(!args.clear);
            }
            other => panic!("expected Hosts(Set(...)), got {other:?}"),
        }
    }

    /// `isd ssh hosts set <agent> --clear` parses with the bool flag
    /// flipped and no `--dial` value.
    #[test]
    fn ssh_hosts_set_parses_with_clear_flag() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SshCommand,
        }
        let parsed =
            Wrap::try_parse_from(["wrap", "hosts", "set", "edge-1", "--clear"]).expect("parses");
        match parsed.cmd {
            SshCommand::Hosts(HostsArgs {
                command: Some(HostsCommand::Set(args)),
            }) => {
                assert_eq!(args.agent, "edge-1");
                assert!(args.dial.is_none());
                assert!(args.clear);
            }
            other => panic!("expected Hosts(Set(...)), got {other:?}"),
        }
    }

    /// `isd ssh hosts set <agent> --name <name>` parses with the
    /// operator name landing in `HostsSetArgs.name` and no dial
    /// flags touched.
    #[test]
    fn ssh_hosts_set_parses_with_name_flag() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SshCommand,
        }
        let parsed = Wrap::try_parse_from(["wrap", "hosts", "set", "1", "--name", "lausanne"])
            .expect("parses");
        match parsed.cmd {
            SshCommand::Hosts(HostsArgs {
                command: Some(HostsCommand::Set(args)),
            }) => {
                assert_eq!(args.agent, "1");
                assert_eq!(args.name.as_deref(), Some("lausanne"));
                assert!(args.dial.is_none());
                assert!(!args.clear);
            }
            other => panic!("expected Hosts(Set(...)), got {other:?}"),
        }
    }

    /// `--name` and `--dial` together both land on the parsed args so
    /// the operator can rename + retarget in one invocation.
    #[test]
    fn ssh_hosts_set_parses_name_and_dial_together() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SshCommand,
        }
        let parsed = Wrap::try_parse_from([
            "wrap",
            "hosts",
            "set",
            "edge-1",
            "--name",
            "lausanne",
            "--dial",
            "op@10.0.0.7",
        ])
        .expect("parses");
        match parsed.cmd {
            SshCommand::Hosts(HostsArgs {
                command: Some(HostsCommand::Set(args)),
            }) => {
                assert_eq!(args.agent, "edge-1");
                assert_eq!(args.name.as_deref(), Some("lausanne"));
                assert_eq!(args.dial.as_deref(), Some("op@10.0.0.7"));
                assert!(!args.clear);
            }
            other => panic!("expected Hosts(Set(...)), got {other:?}"),
        }
    }

    /// `resolve_index_to_dial` reads the on-disk cache and returns the
    /// row's `container_id` (which the hosts index writer packs with
    /// the dial target). Uses a temp `ISD_INDEX_CACHE` so the test does
    /// not touch the operator's real cache file.
    #[test]
    fn resolve_index_to_dial_reads_from_cache() {
        let _lock = crate::index_cache::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", dir.path());
        }
        let cache = IndexCache {
            captured_at: Utc::now(),
            command: "ssh hosts".into(),
            rows: vec![
                IndexRow {
                    index: 0,
                    context: "lausanne".into(),
                    container_id: "dirdmaster@10.17.0.125".into(),
                    name: "edge-1".into(),
                },
                IndexRow {
                    index: 1,
                    context: "lausanne".into(),
                    container_id: "edge-2".into(),
                    name: "edge-2".into(),
                },
            ],
        };
        crate::index_cache::write(&cache).unwrap();
        assert_eq!(resolve_index_to_dial(0).unwrap(), "dirdmaster@10.17.0.125");
        assert_eq!(resolve_index_to_dial(1).unwrap(), "edge-2");
        let err = resolve_index_to_dial(9).unwrap_err();
        assert!(format!("{err}").contains("out of range"));
        unsafe {
            std::env::remove_var("ISD_INDEX_CACHE");
        }
    }

    /// Regression test for the cross-command dial bug: writing a `ps`
    /// cache MUST NOT leak into `isd ssh <N>` resolution. The dial
    /// resolver looks only at the `ssh hosts` cache; an isolated `ps`
    /// cache must yield the friendly "no recent ssh hosts" error, not
    /// a long container hash.
    #[test]
    fn resolve_index_to_dial_ignores_ps_cache() {
        let _lock = crate::index_cache::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", dir.path());
        }
        // Write ONLY a ps cache (no ssh hosts cache present).
        let ps = IndexCache {
            captured_at: Utc::now(),
            command: "ps".into(),
            rows: vec![IndexRow {
                index: 0,
                context: "lausanne".into(),
                container_id: "deadbeefcafe1234567890abcdef".into(),
                name: "web-proxy".into(),
            }],
        };
        crate::index_cache::write(&ps).unwrap();
        let err = resolve_index_to_dial(0).unwrap_err().to_string();
        assert!(
            err.contains("no recent `isd ssh hosts`"),
            "should point at ssh hosts, not return a container hash: {err}"
        );
        // Belt-and-braces: the offending hash must NOT be the result.
        assert!(
            !err.contains("deadbeefcafe1234567890abcdef"),
            "ps container id must not leak into ssh dial: {err}"
        );
        unsafe {
            std::env::remove_var("ISD_INDEX_CACHE");
        }
    }

    /// Helper: render the audit table via the unified renderer's
    /// non-TTY decay path.
    fn audit_plain(rows: &[AuditEntry]) -> String {
        let table = RenderTable {
            columns: audit_columns(),
            rows: build_audit_row_cells(rows),
        };
        render_plain(&table)
    }

    /// Audit headers match the spec order, with no columns dropped.
    #[test]
    fn render_audit_table_has_header_columns() {
        let t = audit_plain(&[]);
        let header = t.lines().next().unwrap();
        assert_eq!(header, "WHEN\tKIND\tPRINCIPALS\tFINGERPRINT\tTTL\tCOMMENT");
    }

    /// Every projected metadata field surfaces in the rendered cells.
    #[test]
    fn render_audit_table_pulls_fields_from_metadata() {
        let rows = vec![AuditEntry {
            kind: "ssh.cert.issued".into(),
            occurred_at: "2026-05-21T10:30:00+00:00".into(),
            metadata: serde_json::json!({
                "pubkey_fingerprint": "SHA256:abcdef0123456789longersuffixhere",
                "principals": ["isengard", "root"],
                "ttl_seconds": 3600,
                "comment": "operator@laptop 2026-05-21T10:30:00Z",
            }),
        }];
        let t = audit_plain(&rows);
        assert!(t.contains("ssh.cert.issued"), "kind column: {t}");
        assert!(t.contains("2026-05-21T10:30:00Z"), "when column: {t}");
        assert!(t.contains("isengard,root"), "principals column: {t}");
        assert!(
            t.contains("SHA256:abcdef0123456789..."),
            "fingerprint column (shortened): {t}"
        );
        assert!(t.contains("3600s"), "ttl column: {t}");
        assert!(t.contains("operator@laptop"), "comment column: {t}");
    }

    /// Missing metadata falls back to `-` placeholders so a malformed
    /// audit row still renders.
    #[test]
    fn render_audit_table_handles_missing_metadata_gracefully() {
        let rows = vec![AuditEntry {
            kind: "ssh.cert.issued".into(),
            occurred_at: "2026-05-21T10:30:00+00:00".into(),
            metadata: serde_json::Value::Null,
        }];
        let t = audit_plain(&rows);
        assert!(t.contains("ssh.cert.issued"));
        // At least one `-` placeholder is present across the empty
        // metadata columns. Tab-separated output keeps adjacent dashes
        // distinct, so the row contains literal `-` tokens.
        let row_line = t.lines().nth(1).unwrap();
        assert!(
            row_line.contains('-'),
            "placeholder is rendered: {row_line}"
        );
    }

    #[test]
    fn shorten_fingerprint_truncates_long_sha() {
        let short = shorten_fingerprint("SHA256:abcdef0123456789longersuffix");
        assert_eq!(short, "SHA256:abcdef0123456789...");
    }

    #[test]
    fn shorten_fingerprint_passes_short_value_through() {
        let short = shorten_fingerprint("SHA256:abc");
        assert_eq!(short, "SHA256:abc");
    }

    #[test]
    fn format_audit_timestamp_normalises_to_z_suffix() {
        let t = format_audit_timestamp("2026-05-21T10:30:00+00:00");
        assert_eq!(t, "2026-05-21T10:30:00Z");
    }

    #[test]
    fn format_audit_timestamp_falls_back_on_garbage() {
        let raw = "not-a-timestamp";
        let t = format_audit_timestamp(raw);
        assert_eq!(t, raw);
    }
}
