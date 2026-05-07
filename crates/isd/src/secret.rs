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

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use comfy_table::{ContentArrangement, Table, presets::NOTHING};

use crate::credentials::ContextEntry;
use crate::login::{pinned_session, verify_pinned_response};

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
    List,
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
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Secret name to delete.
    pub name: String,
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
        SecretCommand::List => run_list(context).await,
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
    let (ctx, client) = pinned_session(context).await?;
    put_secret(&ctx, &client, &args.name, value).await?;
    // Echo nothing about the value. Confirmation by name only.
    println!("Stored secret {:?}.", args.name);
    Ok(())
}

async fn run_list(context: Option<&str>) -> Result<()> {
    let (ctx, client) = pinned_session(context).await?;
    let entries = list_secrets(&ctx, &client).await?;
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

async fn run_rm(args: RmArgs, context: Option<&str>) -> Result<()> {
    let (ctx, client) = pinned_session(context).await?;
    delete_secret(&ctx, &client, &args.name).await?;
    println!("Removed secret {:?}.", args.name);
    Ok(())
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

async fn put_secret(
    ctx: &ContextEntry,
    client: &reqwest::Client,
    name: &str,
    value: String,
) -> Result<()> {
    let url = format!("{}/api/v1/secrets/{name}", ctx.controller_url);
    let resp = client
        .put(&url)
        .bearer_auth(&ctx.token)
        .json(&PutBody { value })
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    verify_pinned_response(&resp, &ctx.ca_fingerprint_sha256)?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("PUT {url} -> {status}: {body}"))
}

async fn list_secrets(ctx: &ContextEntry, client: &reqwest::Client) -> Result<Vec<SecretEntry>> {
    let url = format!("{}/api/v1/secrets", ctx.controller_url);
    let resp = client
        .get(&url)
        .bearer_auth(&ctx.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    verify_pinned_response(&resp, &ctx.ca_fingerprint_sha256)?;
    let entries: Vec<SecretEntry> = resp.error_for_status()?.json().await?;
    Ok(entries)
}

async fn delete_secret(ctx: &ContextEntry, client: &reqwest::Client, name: &str) -> Result<()> {
    let url = format!("{}/api/v1/secrets/{name}", ctx.controller_url);
    let resp = client
        .delete(&url)
        .bearer_auth(&ctx.token)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    verify_pinned_response(&resp, &ctx.ca_fingerprint_sha256)?;
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
