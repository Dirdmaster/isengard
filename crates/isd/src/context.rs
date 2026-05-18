//! `isd context create | use | list | rm | show | import`.
//!
//! Each context records a docker endpoint URL ([`Backend::Docker`]); the
//! controller is discovered by `io.isengard.role=controller` label on
//! that host. Modeled on `docker context create --docker host=ssh://...`.

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
    /// Import a docker context by name as a `Backend::Docker` entry.
    Import(ImportArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Context name.
    pub name: String,

    /// Docker endpoint (ssh://, tcp://, unix://, or local).
    #[arg(long)]
    pub docker: String,

    /// Set as the default context.
    #[arg(long)]
    pub r#use: bool,
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

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Docker context name (matches `docker context ls` first column).
    pub name: String,
    /// Mark the imported context as the default.
    #[arg(long)]
    pub r#use: bool,
}

pub async fn run(args: ContextArgs) -> Result<()> {
    match args.command {
        ContextCommand::Create(a) => run_create(a).await,
        ContextCommand::Use(a) => run_use(a).await,
        ContextCommand::List => run_list().await,
        ContextCommand::Rm(a) => run_rm(a).await,
        ContextCommand::Show(a) => run_show(a).await,
        ContextCommand::Import(a) => run_import(a).await,
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

    let backend = Backend::Docker {
        url: args.docker.clone(),
    };

    let path = credentials::default_credentials_path()?;
    let mut file = credentials::load(&path)?;
    let ctx = ContextEntry {
        name: args.name.clone(),
        backend,
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
        println!("No contexts saved. Create one with `isd context create <name> --docker <uri>`.");
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
        let (kind, target) = render_kind_and_target(ctx);
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

fn render_kind_and_target(ctx: &ContextEntry) -> (&'static str, String) {
    match &ctx.backend {
        Backend::Docker { url } => ("docker", url.clone()),
    }
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

async fn run_import(args: ImportArgs) -> Result<()> {
    validate_name(&args.name)?;
    let docker_config = std::env::var("DOCKER_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .expect("home directory available")
                .join(".docker")
        });
    let entry = crate::context_import::import_from_docker(&args.name, &docker_config)?;
    let path = credentials::default_credentials_path()?;
    let mut file = credentials::load(&path)?;
    let was_first = file.contexts.is_empty();
    file.upsert(entry);
    if args.r#use {
        file.set_default(&args.name)?;
    }
    credentials::save(&path, &file)?;
    let suffix = if was_first || args.r#use {
        " (set as default)"
    } else {
        ""
    };
    println!("Imported docker context {:?}{suffix}.", args.name);
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
        Backend::Docker { url } => {
            println!("kind:    docker");
            println!("docker:  {url}");
            println!("controller: auto (discovered via io.isengard.role label)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn create_with_docker_parses() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "create",
            "lausanne",
            "--docker",
            "ssh://dirdmaster@10.17.0.125",
        ])
        .unwrap();
        match w.c {
            ContextCommand::Create(a) => {
                assert_eq!(a.name, "lausanne");
                assert_eq!(a.docker, "ssh://dirdmaster@10.17.0.125");
                assert!(!a.r#use);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_with_use_sets_default_flag() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "create",
            "local",
            "--docker",
            "unix:///var/run/docker.sock",
            "--use",
        ])
        .unwrap();
        match w.c {
            ContextCommand::Create(a) => {
                assert_eq!(a.docker, "unix:///var/run/docker.sock");
                assert!(a.r#use);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_requires_docker_flag() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let res = Wrap::try_parse_from(["x", "create", "x"]);
        assert!(res.is_err(), "missing --docker should fail clap parse");
    }

    #[test]
    fn validate_name_rejects_bad_chars() {
        assert!(validate_name("ok-1.name").is_ok());
        assert!(validate_name("nope/slash").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }
}
