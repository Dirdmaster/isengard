//! `isd context create | use | list | rm | show`.
//!
//! Replaces the old `isd login` command. Each context records a
//! [`Backend`](crate::credentials::Backend) — either an HTTP URL the
//! operator can reach directly, or an SSH target whose `~/.ssh/config`
//! handles authentication and the controller's dashboard port is
//! tunneled per-command. Modeled on `docker context create`.

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};

use crate::credentials::{self, Backend, ContextEntry};

#[derive(Debug, Args)]
pub struct ContextArgs {
    #[command(subcommand)]
    pub command: ContextCommand,
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    /// Save a new context. Choose `--ssh` for remote homelabs (the
    /// canonical path) or `--http` for local dev / direct-reachable
    /// dashboards.
    Create(CreateArgs),
    /// Set the default context. Subsequent `isd` commands without
    /// `--context <name>` use this one.
    Use(UseArgs),
    /// Show every saved context, with the default one starred.
    List,
    /// Delete a context. Idempotent: errors if the name doesn't exist
    /// (so a typo doesn't silently no-op).
    Rm(RmArgs),
    /// Print one context's full backend details. Defaults to the
    /// current default if no name is given.
    Show(ShowArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Context name. Used in `--context <name>` and as the key in the
    /// credentials file. Allowed chars: `[A-Za-z0-9._-]{1,64}`.
    pub name: String,

    /// SSH target: anything `ssh` understands. Examples:
    /// `dirdmaster@10.17.0.125`, `lausanne` (resolved via
    /// `~/.ssh/config`), `user@host:2222`. Mutually exclusive with
    /// `--http`.
    #[arg(long, conflicts_with = "http")]
    pub ssh: Option<String>,

    /// HTTP/HTTPS URL of the dashboard, including scheme and port.
    /// Example: `http://127.0.0.1:9418`. Mutually exclusive with
    /// `--ssh`.
    #[arg(long, conflicts_with = "ssh")]
    pub http: Option<String>,

    /// Dashboard port on the remote (SSH backend only). The tunnel
    /// forwards `127.0.0.1:<dashboard-port>` on the remote to a local
    /// ephemeral port for each `isd` command.
    #[arg(long, default_value_t = 9418, requires = "ssh")]
    pub dashboard_port: u16,

    /// Set this context as the default after creating it. Without this
    /// flag, the first context created becomes the default; subsequent
    /// `create` invocations leave the default alone.
    #[arg(long)]
    pub r#use: bool,

    /// Phase 0.15: after creating the context, list every secret name in
    /// `--sync-from` that is missing from the newly-added fleet so the
    /// operator can backfill them. Names only: the operator-side CLI
    /// cannot read plaintext (secrets are write-only) so this prints a
    /// to-do list rather than copying values automatically. Pair with
    /// `isd secret put NAME --scope global` to apply each one.
    #[arg(long)]
    pub sync_secrets: bool,

    /// Phase 0.15: source context to compare against when
    /// `--sync-secrets` is set. Defaults to the file's current default
    /// context. Errors if it matches the new context or doesn't exist.
    #[arg(long, requires = "sync_secrets")]
    pub sync_from: Option<String>,

    /// Phase 0.20: Docker endpoint for direct-bollard access. Accepts
    /// `ssh://user@host`, `tcp://host:port`, or `unix:///path`. When
    /// set, `isd ps --backend docker` (Phase 0.20) and the default
    /// container surface (Phase 0.21) use this instead of going through
    /// the Isengard controller's REST API.
    #[arg(long)]
    pub docker: Option<String>,
}

#[derive(Debug, Args)]
pub struct UseArgs {
    /// Context name to mark as default.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Context name to print. Defaults to the file's `default_context`.
    pub name: Option<String>,
}

pub async fn run(args: ContextArgs) -> Result<()> {
    match args.command {
        ContextCommand::Create(a) => run_create(a).await,
        ContextCommand::Use(a) => run_use(a).await,
        ContextCommand::List => run_list().await,
        ContextCommand::Rm(a) => run_rm(a).await,
        ContextCommand::Show(a) => run_show(a).await,
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(anyhow!(
            "name length must be 1..=64 chars (got {})",
            name.len()
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
            return Err(anyhow!(
                "invalid char {c:?} in context name {name:?} (allowed: [A-Za-z0-9._-])"
            ));
        }
    }
    Ok(())
}

async fn run_create(args: CreateArgs) -> Result<()> {
    validate_name(&args.name)?;

    // Phase 0.20: a docker-only context (no controller) is valid for the
    // direct-bollard path. When `--docker` is the only transport given,
    // we still need a Backend value to satisfy ContextEntry's shape; we
    // pick an Http placeholder that the controller-using verbs will
    // reject explicitly when invoked against a no-controller context.
    let backend = match (args.ssh.as_deref(), args.http.as_deref()) {
        (Some(target), None) => Backend::Ssh {
            target: target.to_string(),
            dashboard_port: args.dashboard_port,
        },
        (None, Some(url)) => {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(anyhow!("--http URL must start with http:// or https://"));
            }
            Backend::Http {
                url: url.trim_end_matches('/').to_string(),
            }
        }
        (Some(_), Some(_)) => {
            // clap's conflicts_with should have caught this; defensive
            // belt for the unlikely case.
            return Err(anyhow!("--ssh and --http are mutually exclusive"));
        }
        (None, None) => {
            if args.docker.is_none() {
                return Err(anyhow!(
                    "exactly one of --ssh <target>, --http <url>, or --docker <uri> is required"
                ));
            }
            // Docker-only context. Placeholder Http URL so the existing
            // controller-using code paths fail loudly rather than reach
            // a partial backend; Phase 0.21 will lift the requirement.
            Backend::Http {
                url: "http://no-controller.invalid".to_string(),
            }
        }
    };

    let path = credentials::default_credentials_path()?;
    let mut file = credentials::load(&path)?;
    let ctx = ContextEntry {
        name: args.name.clone(),
        backend,
        docker: args.docker.clone(),
    };
    let was_first = file.contexts.is_empty();

    // Resolve sync source BEFORE upsert so the "default" is the
    // pre-existing default, not the newly-created context.
    let sync_source = if args.sync_secrets {
        let source_name = match args.sync_from.as_deref() {
            Some(s) => s.to_string(),
            None => file
                .default_context
                .clone()
                .ok_or_else(|| {
                    anyhow!(
                        "--sync-secrets needs a source context: pass --sync-from <name> or set a default context first"
                    )
                })?,
        };
        if source_name == args.name {
            return Err(anyhow!(
                "--sync-from {source_name:?} matches the new context; pick a different source"
            ));
        }
        Some(file.default_or_named(Some(&source_name))?.clone())
    } else {
        None
    };

    file.upsert(ctx.clone());
    if args.r#use {
        file.set_default(&args.name)?;
    }
    credentials::save(&path, &file)?;

    let suffix = if was_first || args.r#use {
        " (set as default)"
    } else {
        ""
    };
    println!("Saved context {:?}{suffix}.", args.name);

    if let Some(source_ctx) = sync_source {
        report_sync_secrets(&source_ctx, &ctx).await?;
    }
    Ok(())
}

/// Phase 0.15: list secrets present in `source` but missing from `dest`
/// so the operator can `isd secret put NAME --scope global` for each.
/// Name-only by design: the operator-side CLI cannot read plaintext from
/// either fleet (secrets are write-only over the dashboard API; the
/// agent's FetchSecret mTLS RPC is the only consumer that ever sees
/// plaintext). Auto-copy is intentionally not supported.
async fn report_sync_secrets(source: &ContextEntry, dest: &ContextEntry) -> Result<()> {
    use crate::session::Session;
    let source_session = Session::from_context(source.clone())
        .await
        .with_context(|| format!("opening session for sync-from context {:?}", source.name))?;
    let dest_session = Session::from_context(dest.clone())
        .await
        .with_context(|| format!("opening session for new context {:?}", dest.name))?;

    let source_entries = fetch_secret_names(&source_session)
        .await
        .with_context(|| format!("listing secrets in {:?}", source.name))?;
    let dest_entries = fetch_secret_names(&dest_session)
        .await
        .with_context(|| format!("listing secrets in {:?}", dest.name))?;

    let dest_set: std::collections::BTreeSet<&str> =
        dest_entries.iter().map(String::as_str).collect();
    let missing: Vec<&String> = source_entries
        .iter()
        .filter(|name| !dest_set.contains(name.as_str()))
        .collect();

    if missing.is_empty() {
        println!(
            "Sync check: every secret in {:?} is already present in {:?}.",
            source.name, dest.name
        );
        return Ok(());
    }

    println!();
    println!(
        "Sync check: {} secret(s) in {:?} not yet in {:?}:",
        missing.len(),
        source.name,
        dest.name
    );
    for name in &missing {
        println!("  - {name}");
    }
    println!();
    println!(
        "To backfill, re-run `isd secret put <name> --scope global` for each (the operator-side CLI can't read plaintext, so we list names only)."
    );
    Ok(())
}

async fn fetch_secret_names(session: &crate::session::Session) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
    }
    let url = format!("{}/api/v1/secrets", session.controller_url());
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let entries: Vec<Entry> = resp.json().await.with_context(|| format!("decode {url}"))?;
    Ok(entries.into_iter().map(|e| e.name).collect())
}

async fn run_use(args: UseArgs) -> Result<()> {
    let path = credentials::default_credentials_path()?;
    let mut file = credentials::load(&path)?;
    file.set_default(&args.name)?;
    credentials::save(&path, &file)?;
    println!("Default context is now {:?}.", args.name);
    Ok(())
}

async fn run_list() -> Result<()> {
    let path = credentials::default_credentials_path()?;
    let file = credentials::load(&path)?;
    if file.contexts.is_empty() {
        println!("No contexts saved. Create one with `isd context create <name> --ssh <target>`.");
        return Ok(());
    }
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["", "NAME", "KIND", "TARGET"]);
    let default = file.default_context.as_deref();
    for ctx in &file.contexts {
        let star = if Some(ctx.name.as_str()) == default {
            "*"
        } else {
            ""
        };
        let (kind, target) = match &ctx.backend {
            Backend::Http { url } => ("http", url.clone()),
            Backend::Ssh {
                target,
                dashboard_port,
            } => ("ssh", format!("{target}  (forward :{dashboard_port})")),
        };
        table.add_row(vec![
            star.to_string(),
            ctx.name.clone(),
            kind.into(),
            target,
        ]);
    }
    println!("{table}");
    Ok(())
}

async fn run_rm(args: RmArgs) -> Result<()> {
    let path = credentials::default_credentials_path()?;
    let mut file = credentials::load(&path)?;
    if !file.remove(&args.name) {
        return Err(anyhow!("no saved context named {:?}", args.name));
    }
    credentials::save(&path, &file)?;
    println!("Removed context {:?}.", args.name);
    Ok(())
}

async fn run_show(args: ShowArgs) -> Result<()> {
    let path = credentials::default_credentials_path()?;
    let file = credentials::load(&path)?;
    let ctx = file
        .default_or_named(args.name.as_deref())
        .context("resolving context")?;
    println!("name:    {}", ctx.name);
    match &ctx.backend {
        Backend::Http { url } => {
            println!("kind:    http");
            println!("url:     {url}");
        }
        Backend::Ssh {
            target,
            dashboard_port,
        } => {
            println!("kind:    ssh");
            println!("target:  {target}");
            println!("forward: 127.0.0.1:<ephemeral> -> {target}:{dashboard_port}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn create_with_ssh_parses() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w =
            Wrap::try_parse_from(["x", "create", "lausanne", "--ssh", "dirdmaster@10.17.0.125"])
                .unwrap();
        match w.c {
            ContextCommand::Create(a) => {
                assert_eq!(a.name, "lausanne");
                assert_eq!(a.ssh.as_deref(), Some("dirdmaster@10.17.0.125"));
                assert_eq!(a.dashboard_port, 9418);
                assert!(!a.r#use);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_with_http_parses() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "create",
            "local",
            "--http",
            "http://127.0.0.1:9418",
            "--use",
        ])
        .unwrap();
        match w.c {
            ContextCommand::Create(a) => {
                assert_eq!(a.http.as_deref(), Some("http://127.0.0.1:9418"));
                assert!(a.r#use);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_rejects_both_backends() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let res = Wrap::try_parse_from([
            "x",
            "create",
            "x",
            "--ssh",
            "user@host",
            "--http",
            "http://x",
        ]);
        assert!(res.is_err(), "ssh + http should conflict");
    }

    #[test]
    fn validate_name_rejects_bad_chars() {
        assert!(validate_name("ok-1.name").is_ok());
        assert!(validate_name("nope/slash").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn create_args_sync_secrets_parses() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "create",
            "bob",
            "--ssh",
            "user@bob",
            "--sync-secrets",
            "--sync-from",
            "alice",
        ])
        .unwrap();
        match w.c {
            ContextCommand::Create(a) => {
                assert!(a.sync_secrets);
                assert_eq!(a.sync_from.as_deref(), Some("alice"));
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_args_sync_from_alone_is_rejected() {
        // --sync-from without --sync-secrets is meaningless; clap should
        // refuse via `requires`.
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let res = Wrap::try_parse_from([
            "x",
            "create",
            "bob",
            "--ssh",
            "user@bob",
            "--sync-from",
            "alice",
        ]);
        assert!(res.is_err(), "--sync-from requires --sync-secrets");
    }

    #[test]
    fn create_args_sync_secrets_alone_parses_with_implicit_default_source() {
        // --sync-secrets without --sync-from is allowed: run_create
        // falls back to the file's default context as the source.
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from(["x", "create", "bob", "--ssh", "user@bob", "--sync-secrets"])
            .unwrap();
        match w.c {
            ContextCommand::Create(a) => {
                assert!(a.sync_secrets);
                assert!(a.sync_from.is_none());
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }
}
