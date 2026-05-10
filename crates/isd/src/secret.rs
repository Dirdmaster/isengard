//! `isd secret put` / `isd secret list` / `isd secret rm` (v0.3.6
//! managed-secrets store).
//!
//! Talks to the dashboard's `/api/v1/secrets[/<name>]` endpoints. There is
//! intentionally NO `isd secret get`: secrets are write-only from the
//! operator side. The agent is the only consumer that ever sees the
//! plaintext (over the FetchSecret mTLS RPC).
//!
//! All three subcommands reuse the [`pinned_session`](crate::login::pinned_session)
//! pattern from `compose_cmd.rs`: load the credentials file, pin the CA
//! fingerprint, send the request.
//!
//! Phase 0.15 adds a `--scope` flag (`fleet` | `global`). `fleet` (the
//! default) keeps the historical single-context behaviour; `global` walks
//! every context saved in the credentials file and applies the operation
//! to each. Semantics are best-effort: per-context failures don't abort
//! the run, and a summary line at the end names which contexts failed and
//! why. Each fleet keeps its own encryption: the secret is re-encrypted
//! under each controller's master.key as it lands. See
//! `docs/RELEASE_NOTES_PHASE_0_15.md` for the full design.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use comfy_table::{ContentArrangement, Table, presets::NOTHING};

use crate::credentials::{self, ContextEntry};
use crate::session::Session;

/// Where a `put` / `rm` / `ls` applies.
///
/// `Fleet` is the historical single-context behaviour (default). `Global`
/// iterates every context in the credentials file so the secret lands in
/// every fleet the operator manages from this machine. Each fleet still
/// encrypts the value under its own master.key; the global scope is a
/// fan-out at the operator's CLI layer, NOT a shared keystore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Scope {
    /// Apply to the currently-selected (or `--context`-overridden) fleet
    /// only. No fan-out. This is the v0.3.6 behaviour.
    #[default]
    Fleet,
    /// Apply to every fleet in `~/.config/isengard/credentials.toml`. Each
    /// controller gets its own encrypted copy. Best-effort: per-fleet
    /// failures are reported in a summary line; the run does not abort.
    Global,
}

#[derive(Debug, Args)]
pub struct SecretArgs {
    #[command(subcommand)]
    pub command: SecretCommand,
}

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Upsert a secret value. Reads from stdin by default; `--from-file`
    /// reads the named file. The value is encrypted by the controller
    /// before it touches disk.
    Put(PutArgs),
    /// List secret names + timestamps. NEVER prints values: secrets are
    /// write-only from the operator's CLI.
    List(ListArgs),
    /// Delete a secret by name. Idempotent in spirit but errors if the
    /// name doesn't exist (so a typo doesn't silently no-op).
    Rm(RmArgs),
}

#[derive(Debug, Args)]
pub struct PutArgs {
    /// Secret name. Allowed chars: `[A-Za-z0-9._-]`, max 64.
    pub name: String,
    /// Read the value from this file. Mutually exclusive with stdin.
    /// When omitted, the value is read from stdin (must not be a TTY).
    #[arg(long)]
    pub from_file: Option<PathBuf>,
    /// Where the secret lands. `fleet` (default) writes to the current
    /// `--context`; `global` writes to every saved context, encrypted
    /// per-fleet under each controller's master.key. Best-effort: a
    /// summary line at the end names per-context failures.
    #[arg(long, value_enum, default_value_t = Scope::Fleet)]
    pub scope: Scope,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Secret name to delete.
    pub name: String,
    /// Where the deletion applies. `fleet` (default) removes from the
    /// current `--context`; `global` removes from every saved context.
    /// Best-effort: per-context misses (404) are reported but don't fail
    /// the run, since "remove everywhere" is naturally idempotent.
    #[arg(long, value_enum, default_value_t = Scope::Fleet)]
    pub scope: Scope,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Where the listing reads from. `fleet` (default) lists secrets in
    /// the current `--context`; `global` aggregates across every saved
    /// context, marking each name as `global` (present in every fleet
    /// with the same name), `fleet` (only one fleet has it), or
    /// `partial` (some fleets have it, others don't). Per-context fetch
    /// errors are reported on stderr and the context is omitted from
    /// aggregation.
    #[arg(long, value_enum, default_value_t = Scope::Fleet)]
    pub scope: Scope,
}

#[derive(Debug, Serialize)]
struct PutBody {
    value: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // surfaced verbatim in user-facing error messages
struct ErrorBody {
    error: String,
}

#[derive(Debug, Deserialize)]
struct SecretEntry {
    name: String,
    created_at: String,
    updated_at: String,
}

pub async fn run(args: SecretArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        SecretCommand::Put(a) => run_put(a, context).await,
        SecretCommand::List(a) => run_list(a, context).await,
        SecretCommand::Rm(a) => run_rm(a, context).await,
    }
}

async fn run_put(args: PutArgs, context: Option<&str>) -> Result<()> {
    let value = read_value(args.from_file.as_deref())?;
    if value.is_empty() {
        return Err(anyhow!(
            "value is empty; refusing to store an empty secret. Pipe data on stdin or pass --from-file <path>."
        ));
    }
    match args.scope {
        Scope::Fleet => {
            let session = Session::open(context).await?;
            put_secret(&session, &args.name, value).await?;
            // Echo nothing about the value. Confirmation by name only.
            println!("Stored secret {:?}.", args.name);
            Ok(())
        }
        Scope::Global => run_put_global(&args.name, value).await,
    }
}

/// Walk every saved context and PUT the secret to each. Best-effort:
/// per-context failures are reported in the summary line; the run does
/// not abort. The summary mirrors the format used by `secret rm --scope
/// global`.
async fn run_put_global(name: &str, value: String) -> Result<()> {
    let contexts = load_all_contexts()?;
    if contexts.is_empty() {
        return Err(anyhow!(
            "no saved contexts; run `isd context create <name> --ssh <target>` first"
        ));
    }
    let mut report = ScopeReport::new();
    for ctx in &contexts {
        match put_to_context(ctx, name, value.clone()).await {
            Ok(()) => report.push_ok(&ctx.name),
            Err(e) => report.push_err(&ctx.name, e),
        }
    }
    report.print(&format!("Stored secret {name:?}"));
    if report.has_failures() {
        // Exit non-zero so scripts can detect partial application; the
        // detail already printed.
        return Err(anyhow!(
            "scope=global: {} of {} contexts failed",
            report.failures.len(),
            contexts.len()
        ));
    }
    Ok(())
}

async fn put_to_context(ctx: &ContextEntry, name: &str, value: String) -> Result<()> {
    let session = Session::from_context(ctx.clone()).await?;
    put_secret(&session, name, value).await
}

async fn run_list(args: ListArgs, context: Option<&str>) -> Result<()> {
    match args.scope {
        Scope::Fleet => run_list_fleet(context).await,
        Scope::Global => run_list_global().await,
    }
}

async fn run_list_fleet(context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let entries = list_secrets(&session).await?;
    if entries.is_empty() {
        println!("No secrets stored.");
        return Ok(());
    }
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["NAME", "CREATED", "UPDATED"]);
    for e in &entries {
        table.add_row(vec![
            e.name.clone(),
            short_ts(&e.created_at),
            short_ts(&e.updated_at),
        ]);
    }
    println!("{table}");
    Ok(())
}

/// Aggregate secrets across every saved context. The scope column is
/// derived purely from coverage: a secret present in every reachable
/// context is `global`; in some but not all is `partial`; in exactly
/// one is `fleet`. This is name-only: values are not fetched (and
/// couldn't be — secrets are write-only from the operator side), so
/// "partial" does NOT detect value divergence; it only detects name
/// coverage. The operator decides what to do.
async fn run_list_global() -> Result<()> {
    let contexts = load_all_contexts()?;
    if contexts.is_empty() {
        return Err(anyhow!(
            "no saved contexts; run `isd context create <name> --ssh <target>` first"
        ));
    }
    let snapshot = collect_global_snapshot(&contexts).await;
    print_global_listing(&snapshot);
    if snapshot.unreachable_count() == contexts.len() {
        return Err(anyhow!(
            "scope=global: every context failed; nothing to aggregate"
        ));
    }
    Ok(())
}

/// Snapshot of secret presence across reachable contexts. Used by
/// `secret ls --scope global`. `unreachable` carries the contexts whose
/// listing failed so the summary can mention them on stderr.
pub(crate) struct GlobalSnapshot {
    /// Number of contexts that responded successfully. Used to decide
    /// `global` vs `partial` (a name is `global` only when present in
    /// every reachable context, NOT every saved context, so an offline
    /// fleet doesn't flip every entry to `partial`).
    pub(crate) reachable: usize,
    /// `name -> list of context names that hold it`, sorted by name for
    /// stable output.
    pub(crate) coverage: Vec<(String, Vec<String>)>,
    /// Contexts whose listing failed, with the cause for the summary.
    pub(crate) unreachable: Vec<(String, anyhow::Error)>,
}

impl GlobalSnapshot {
    pub(crate) fn unreachable_count(&self) -> usize {
        self.unreachable.len()
    }
}

async fn collect_global_snapshot(contexts: &[ContextEntry]) -> GlobalSnapshot {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unreachable: Vec<(String, anyhow::Error)> = Vec::new();
    let mut reachable = 0usize;
    for ctx in contexts {
        match list_from_context(ctx).await {
            Ok(entries) => {
                reachable += 1;
                for e in entries {
                    by_name.entry(e.name).or_default().push(ctx.name.clone());
                }
            }
            Err(e) => unreachable.push((ctx.name.clone(), e)),
        }
    }
    let coverage: Vec<_> = by_name.into_iter().collect();
    GlobalSnapshot {
        reachable,
        coverage,
        unreachable,
    }
}

async fn list_from_context(ctx: &ContextEntry) -> Result<Vec<SecretEntry>> {
    let session = Session::from_context(ctx.clone()).await?;
    list_secrets(&session).await
}

/// Render `name / scope / fleets` table for `secret ls --scope global`.
/// Pure formatter so it stays testable without any HTTP scaffolding.
pub(crate) fn print_global_listing(snapshot: &GlobalSnapshot) {
    if snapshot.coverage.is_empty() {
        if snapshot.unreachable.is_empty() {
            println!("No secrets stored in any context.");
        } else {
            println!("No secrets stored in any reachable context.");
        }
    } else {
        let mut table = Table::new();
        table
            .load_preset(NOTHING)
            .set_content_arrangement(ContentArrangement::Disabled)
            .set_header(vec!["NAME", "SCOPE", "FLEETS"]);
        for (name, fleets) in &snapshot.coverage {
            let scope = classify_coverage(fleets.len(), snapshot.reachable);
            table.add_row(vec![name.clone(), scope.into(), fleets.join(", ")]);
        }
        println!("{table}");
    }
    if !snapshot.unreachable.is_empty() {
        let parts: Vec<_> = snapshot
            .unreachable
            .iter()
            .map(|(name, err)| format!("{name} ({})", short_err(err)))
            .collect();
        eprintln!(
            "warning: {} of {} context(s) unreachable: {}",
            snapshot.unreachable.len(),
            snapshot.reachable + snapshot.unreachable.len(),
            parts.join(", ")
        );
    }
}

/// Classify a name's coverage relative to the reachable context count.
/// - `fleet`: only one reachable context has the secret (or there's only
///   one context to consider in total).
/// - `global`: present in every reachable context, and there's more than
///   one reachable context.
/// - `partial`: present in some but not all of multiple reachable
///   contexts.
pub(crate) fn classify_coverage(present_in: usize, reachable: usize) -> &'static str {
    // With zero or one reachable context, "global" loses meaning;
    // treat everything as "fleet" so the operator isn't misled into
    // thinking a single-fleet secret is synced anywhere.
    if reachable <= 1 || present_in <= 1 {
        "fleet"
    } else if present_in >= reachable {
        "global"
    } else {
        "partial"
    }
}

async fn run_rm(args: RmArgs, context: Option<&str>) -> Result<()> {
    match args.scope {
        Scope::Fleet => {
            let session = Session::open(context).await?;
            delete_secret(&session, &args.name).await?;
            println!("Removed secret {:?}.", args.name);
            Ok(())
        }
        Scope::Global => run_rm_global(&args.name).await,
    }
}

/// Walk every saved context and DELETE the secret in each. Best-effort:
/// 404s are treated as "already gone" (idempotent) and reported as
/// successes; transport / other failures land in the failure summary.
async fn run_rm_global(name: &str) -> Result<()> {
    let contexts = load_all_contexts()?;
    if contexts.is_empty() {
        return Err(anyhow!(
            "no saved contexts; run `isd context create <name> --ssh <target>` first"
        ));
    }
    let mut report = ScopeReport::new();
    for ctx in &contexts {
        match rm_from_context(ctx, name).await {
            Ok(RmOutcome::Removed) => report.push_ok(&ctx.name),
            Ok(RmOutcome::AlreadyGone) => report.push_already_gone(&ctx.name),
            Err(e) => report.push_err(&ctx.name, e),
        }
    }
    report.print(&format!("Removed secret {name:?}"));
    if report.has_failures() {
        return Err(anyhow!(
            "scope=global: {} of {} contexts failed",
            report.failures.len(),
            contexts.len()
        ));
    }
    Ok(())
}

/// Distinguish "we actually deleted it" from "the server said 404 so it
/// was already gone." Both are successes for global-scope rm; the report
/// labels them differently so the operator can tell whether anything
/// actually changed.
enum RmOutcome {
    Removed,
    AlreadyGone,
}

async fn rm_from_context(ctx: &ContextEntry, name: &str) -> Result<RmOutcome> {
    let session = Session::from_context(ctx.clone()).await?;
    // delete_secret returns an error on 404. For global scope that's a
    // success-ish state (already gone). Re-classify by re-checking the
    // status here so callers can attribute it.
    let url = format!("{}/api/v1/secrets/{name}", session.controller_url());
    let resp = session
        .client
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(RmOutcome::AlreadyGone);
    }
    if status.is_success() {
        return Ok(RmOutcome::Removed);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("DELETE {url} -> {status}: {body}"))
}

fn read_value(from_file: Option<&std::path::Path>) -> Result<String> {
    match from_file {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("reading {}", p.display()))
            .map(|s| s.trim_end_matches('\n').to_string()),
        None => {
            // Refuse to read from a TTY: prevents a fat-fingered
            // `isd secret put cf_token` from blocking forever waiting
            // on the operator. They almost certainly meant to pipe.
            if std::io::stdin().is_terminal() {
                return Err(anyhow!(
                    "stdin is a TTY; pipe a value or pass --from-file <path>"
                ));
            }
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading value from stdin")?;
            Ok(buf.trim_end_matches('\n').to_string())
        }
    }
}

async fn put_secret(session: &Session, name: &str, value: String) -> Result<()> {
    let url = format!("{}/api/v1/secrets/{name}", session.controller_url());
    let resp = session
        .client
        .put(&url)
        .json(&PutBody { value })
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("PUT {url} -> {status}: {body}"))
}

async fn list_secrets(session: &Session) -> Result<Vec<SecretEntry>> {
    let url = format!("{}/api/v1/secrets", session.controller_url());
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let entries: Vec<SecretEntry> = resp.error_for_status()?.json().await?;
    Ok(entries)
}

async fn delete_secret(session: &Session, name: &str) -> Result<()> {
    let url = format!("{}/api/v1/secrets/{name}", session.controller_url());
    let resp = session
        .client
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!("secret {name:?} not found"));
    }
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("DELETE {url} -> {status}: {body}"))
}

/// Load every saved context. Used by `--scope global` to fan-out a
/// put / rm / ls across the operator's fleets. Returns an empty Vec when
/// the credentials file is missing (the caller surfaces a friendlier
/// message).
pub(crate) fn load_all_contexts() -> Result<Vec<ContextEntry>> {
    let path = credentials::default_credentials_path()?;
    let file = credentials::load(&path)?;
    Ok(file.contexts)
}

/// Per-context status accumulator for `--scope global` operations.
/// Tracks three buckets so the summary line can distinguish "synced"
/// from "already gone" (for rm) from "failed".
pub(crate) struct ScopeReport {
    pub(crate) successes: Vec<String>,
    pub(crate) already_gone: Vec<String>,
    pub(crate) failures: Vec<(String, anyhow::Error)>,
}

impl ScopeReport {
    pub(crate) fn new() -> Self {
        Self {
            successes: Vec::new(),
            already_gone: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub(crate) fn push_ok(&mut self, ctx: &str) {
        self.successes.push(ctx.to_string());
    }

    pub(crate) fn push_already_gone(&mut self, ctx: &str) {
        self.already_gone.push(ctx.to_string());
    }

    pub(crate) fn push_err(&mut self, ctx: &str, err: anyhow::Error) {
        self.failures.push((ctx.to_string(), err));
    }

    pub(crate) fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    /// One-line summary in the form
    /// `<verb> globally: synced to 3/4 contexts (a, b, c); failed: d (timeout)`.
    /// Already-gone entries (rm) are folded into the "synced" count but
    /// noted in parentheses.
    pub(crate) fn print(&self, verb: &str) {
        let total = self.successes.len() + self.already_gone.len() + self.failures.len();
        let ok_count = self.successes.len() + self.already_gone.len();
        let ok_list = self
            .successes
            .iter()
            .chain(self.already_gone.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let mut line = format!("{verb} globally: synced to {ok_count}/{total} contexts");
        if !ok_list.is_empty() {
            line.push_str(&format!(" ({ok_list})"));
        }
        if !self.already_gone.is_empty() {
            line.push_str(&format!(
                "; already-gone in: {}",
                self.already_gone.join(", ")
            ));
        }
        if !self.failures.is_empty() {
            let failed = self
                .failures
                .iter()
                .map(|(name, err)| format!("{name} ({})", short_err(err)))
                .collect::<Vec<_>>()
                .join(", ");
            line.push_str(&format!("; failed: {failed}"));
        }
        println!("{line}");
    }
}

/// Render an error chain as one short line for the summary. Walks the
/// `source()` chain only enough to surface the most actionable string
/// (e.g. "timeout", "connection refused") without dumping a stack.
fn short_err(err: &anyhow::Error) -> String {
    // Find the deepest cause that contains useful text; fall back to the
    // top-level message. Keeps the summary readable when reqwest wraps
    // an io::Error wrapping an os error.
    let mut tip = err.to_string();
    let mut cur: &(dyn std::error::Error + 'static) = err.as_ref();
    while let Some(src) = cur.source() {
        tip = src.to_string();
        cur = src;
    }
    // Trim newlines and excessive length.
    let one_line: String = tip.replace('\n', " ");
    if one_line.len() > 80 {
        format!("{}...", &one_line[..77])
    } else {
        one_line
    }
}

/// Truncate a timestamp to `YYYY-MM-DD HH:MM` for terse table output.
fn short_ts(ts: &str) -> String {
    // RFC3339: 2026-05-08T10:34:56+00:00. Replace 'T' with space, drop
    // seconds + offset.
    let with_space = ts.replacen('T', " ", 1);
    if with_space.len() >= 16 {
        with_space[..16].to_string()
    } else {
        with_space
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ts_truncates_rfc3339_to_minutes() {
        assert_eq!(short_ts("2026-05-08T10:34:56+00:00"), "2026-05-08 10:34");
        assert_eq!(short_ts("2026-05-08T10:34:56Z"), "2026-05-08 10:34");
    }

    #[test]
    fn short_ts_passes_through_short_strings() {
        assert_eq!(short_ts("2026"), "2026");
    }

    #[test]
    fn put_args_parse() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "put", "cf_token"]).unwrap();
        match w.c {
            SecretCommand::Put(a) => {
                assert_eq!(a.name, "cf_token");
                assert!(a.from_file.is_none());
            }
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn put_args_with_from_file() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "put", "cf_token", "--from-file", "/tmp/x"]).unwrap();
        match w.c {
            SecretCommand::Put(a) => assert_eq!(a.from_file.unwrap().to_str().unwrap(), "/tmp/x"),
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn put_args_scope_defaults_to_fleet() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "put", "cf_token"]).unwrap();
        match w.c {
            SecretCommand::Put(a) => assert_eq!(a.scope, Scope::Fleet),
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn put_args_scope_global_parses() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "put", "cf_token", "--scope", "global"]).unwrap();
        match w.c {
            SecretCommand::Put(a) => assert_eq!(a.scope, Scope::Global),
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn list_args_scope_defaults_to_fleet() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "list"]).unwrap();
        match w.c {
            SecretCommand::List(a) => assert_eq!(a.scope, Scope::Fleet),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn list_args_scope_global_parses() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "list", "--scope", "global"]).unwrap();
        match w.c {
            SecretCommand::List(a) => assert_eq!(a.scope, Scope::Global),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn classify_coverage_global_when_present_everywhere() {
        assert_eq!(classify_coverage(3, 3), "global");
        assert_eq!(classify_coverage(2, 2), "global");
    }

    #[test]
    fn classify_coverage_partial_when_some_missing() {
        assert_eq!(classify_coverage(2, 3), "partial");
    }

    #[test]
    fn classify_coverage_fleet_when_only_one() {
        // Single context: everything is "fleet" (no concept of global with N=1).
        assert_eq!(classify_coverage(1, 1), "fleet");
        // Multi context, only one has it.
        assert_eq!(classify_coverage(1, 3), "fleet");
    }

    #[test]
    fn rm_args_scope_global_parses() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: SecretCommand,
        }
        let w = Wrap::try_parse_from(["x", "rm", "cf_token", "--scope", "global"]).unwrap();
        match w.c {
            SecretCommand::Rm(a) => {
                assert_eq!(a.scope, Scope::Global);
                assert_eq!(a.name, "cf_token");
            }
            other => panic!("expected Rm, got {other:?}"),
        }
    }

    #[test]
    fn scope_report_summary_lists_successes_and_failures() {
        let mut r = ScopeReport::new();
        r.push_ok("alice");
        r.push_ok("bob");
        r.push_err("carol", anyhow!("connection refused"));
        assert!(r.has_failures());
        // Don't snapshot the exact line (println goes to stdout); just
        // verify the buckets sized correctly.
        assert_eq!(r.successes.len(), 2);
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].0, "carol");
    }

    #[test]
    fn scope_report_already_gone_counts_as_success() {
        let mut r = ScopeReport::new();
        r.push_ok("alice");
        r.push_already_gone("bob");
        assert!(!r.has_failures());
        assert_eq!(r.successes.len(), 1);
        assert_eq!(r.already_gone.len(), 1);
    }

    #[test]
    fn short_err_collapses_newlines_and_caps_length() {
        let e = anyhow!("first line\nsecond line\nthird");
        let s = short_err(&e);
        assert!(!s.contains('\n'));
        let long = anyhow!("{}", "x".repeat(200));
        let s2 = short_err(&long);
        assert!(s2.len() <= 80);
    }

    #[test]
    fn read_value_rejects_empty_file() {
        // Reading is fine; the upstream check enforces non-empty.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty");
        std::fs::write(&f, "").unwrap();
        let v = read_value(Some(&f)).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn read_value_strips_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("v");
        std::fs::write(&f, "hello\n").unwrap();
        let v = read_value(Some(&f)).unwrap();
        assert_eq!(v, "hello");
    }
}
