//! `isd context create | use | list | rm | show`.
//!
//! Each context records a [`Backend`](crate::credentials::Backend): either
//! an HTTP URL the operator can reach directly, or an SSH target whose
//! `~/.ssh/config` handles authentication and the controller's dashboard
//! port is tunneled per-command. Modeled on `docker context create`.

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
    /// Save a new context.
    Create(CreateArgs),
    /// Set the default context.
    Use(UseArgs),
    /// Show every saved context.
    List,
    /// Delete a context.
    Rm(RmArgs),
    /// Print one context's full backend details.
    Show(ShowArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Context name.
    pub name: String,

    /// SSH target (e.g. user@host).
    #[arg(long, conflicts_with = "http")]
    pub ssh: Option<String>,

    /// Dashboard URL (e.g. http://127.0.0.1:9418).
    #[arg(long, conflicts_with = "ssh")]
    pub http: Option<String>,

    /// Remote dashboard port to forward over SSH.
    #[arg(long, default_value_t = 9418, requires = "ssh")]
    pub dashboard_port: u16,

    /// Set as the default context.
    #[arg(long)]
    pub r#use: bool,

    /// Docker endpoint (ssh://, tcp://, unix://, or local).
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
    /// Context name to print (defaults to current).
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

    file.upsert(ctx);
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
    Ok(())
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
}
