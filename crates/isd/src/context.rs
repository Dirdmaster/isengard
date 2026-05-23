//! `isd context`: list, use, show, create, rm. All operations delegate to
//! docker's context store at `~/.docker/contexts/`: there is no parallel
//! state. `create` and `rm` shell out to the `docker` CLI so operators
//! never run docker commands directly. `list` / `use` / `show` read +
//! write the store via `crate::docker_context`.

use crate::render::{Align, CellStyle, Column, Table, render, render_plain};
use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};

use crate::docker_context;

/// CLI flags for `isd context`.
#[derive(Debug, Args)]
pub struct ContextArgs {
    /// Resolved sub-verb.
    #[command(subcommand)]
    pub command: ContextCommand,
}

/// Sub-verbs under `isd context`.
#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    /// List docker contexts. `*` marks the current one.
    Ls,
    /// Set the current context (same effect as `docker context use <name>`).
    Use(UseArgs),
    /// Print one context's docker endpoint.
    Show(ShowArgs),
    /// Add a new docker context. Thin wrapper over `docker context create`.
    Add(CreateArgs),
    /// Remove a docker context. Thin wrapper over `docker context rm`.
    Rm(RmArgs),
}

/// CLI flags for `isd context use`.
#[derive(Debug, Args)]
pub struct UseArgs {
    /// Context name to mark current.
    pub name: String,
}

/// CLI flags for `isd context show`.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Context name. Defaults to the current context.
    pub name: Option<String>,
}

/// CLI flags for `isd context add`.
#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Context name.
    pub name: String,
    /// Docker endpoint URL (e.g. `ssh://user@host`, `tcp://host:2375`).
    ///
    /// Also accepts `unix:///var/run/docker.sock`. Passed straight to
    /// `docker context create --docker host=<url>`.
    #[arg(long)]
    pub docker: String,
    /// Optional description recorded on the context.
    #[arg(long)]
    pub description: Option<String>,
    /// Set as the current context after creation.
    #[arg(long)]
    pub r#use: bool,
}

/// CLI flags for `isd context rm`.
#[derive(Debug, Args)]
pub struct RmArgs {
    /// Context name(s) to remove.
    #[arg(required = true)]
    pub names: Vec<String>,
    /// Force removal even when the context is current.
    #[arg(long, short = 'f')]
    pub force: bool,
}

/// Dispatch to the matching `context` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
pub async fn run(args: ContextArgs) -> Result<()> {
    match args.command {
        ContextCommand::Ls => run_list().await,
        ContextCommand::Use(a) => run_use(a).await,
        ContextCommand::Show(a) => run_show(a).await,
        ContextCommand::Add(a) => run_create(a).await,
        ContextCommand::Rm(a) => run_rm(a).await,
    }
}

/// Print every docker context as a boxed table, marking the current
/// one with `▸` in the first column.
async fn run_list() -> Result<()> {
    let contexts = docker_context::list_contexts()?;
    let rows: Vec<Vec<String>> = contexts
        .iter()
        .map(|ctx| {
            vec![
                if ctx.current {
                    "▸".into()
                } else {
                    String::new()
                },
                ctx.name.clone(),
                ctx.kind.to_string(),
                ctx.target.clone(),
            ]
        })
        .collect();
    let table = Table {
        columns: vec![
            Column::new("", Align::Right, CellStyle::Cyan, 9, 1),
            Column::new("NAME", Align::Left, CellStyle::Emphasis, 1, 6),
            Column::new("KIND", Align::Left, CellStyle::Dim, 6, 4),
            Column::new("TARGET", Align::Left, CellStyle::Plain, 4, 14),
        ],
        rows,
    };
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        println!("{}", render(&table, width, console::colors_enabled()));
    } else {
        println!("{}", render_plain(&table));
    }
    Ok(())
}

/// Write `currentContext` in `~/.docker/config.json` to `args.name`.
async fn run_use(args: UseArgs) -> Result<()> {
    docker_context::set_current_context(&args.name)?;
    println!("Current docker context is now {:?}.", args.name);
    Ok(())
}

/// Print one context's name, kind, and docker endpoint. Defaults to
/// the current context when `args.name` is unset.
async fn run_show(args: ShowArgs) -> Result<()> {
    let name = match args.name {
        Some(n) => n,
        None => docker_context::current_context_name()?,
    };
    let meta = docker_context::read_context_meta(&name)?;
    println!("name:   {}", meta.name);
    println!("kind:   docker");
    println!("docker: {}", meta.endpoints.docker.host);
    Ok(())
}

/// Shell out to `docker context create`, optionally setting the new
/// context as current. Thin wrapper so the docker CLI's own input
/// validation drives the experience.
async fn run_create(args: CreateArgs) -> Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("context")
        .arg("create")
        .arg(&args.name)
        .arg("--docker")
        .arg(format!("host={}", args.docker));
    if let Some(desc) = &args.description {
        cmd.arg("--description").arg(desc);
    }
    let status = cmd
        .status()
        .await
        .context("spawning `docker context create`")?;
    if !status.success() {
        return Err(anyhow!(
            "docker context create failed (exit {:?})",
            status.code()
        ));
    }
    if args.r#use {
        docker_context::set_current_context(&args.name)?;
        println!("Current docker context is now {:?}.", args.name);
    }
    Ok(())
}

/// Shell out to `docker context rm` with the resolved names and the
/// optional `--force` flag.
async fn run_rm(args: RmArgs) -> Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("context").arg("rm");
    if args.force {
        cmd.arg("--force");
    }
    for name in &args.names {
        cmd.arg(name);
    }
    let status = cmd.status().await.context("spawning `docker context rm`")?;
    if !status.success() {
        return Err(anyhow!(
            "docker context rm failed (exit {:?})",
            status.code()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn list_use_show_parse() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from(["x", "ls"]).unwrap();
        assert!(matches!(w.c, ContextCommand::Ls));

        let w = Wrap::try_parse_from(["x", "use", "lausanne"]).unwrap();
        match w.c {
            ContextCommand::Use(a) => assert_eq!(a.name, "lausanne"),
            other => panic!("expected Use, got {other:?}"),
        }

        let w = Wrap::try_parse_from(["x", "show"]).unwrap();
        match w.c {
            ContextCommand::Show(a) => assert!(a.name.is_none()),
            other => panic!("expected Show, got {other:?}"),
        }

        let w = Wrap::try_parse_from(["x", "show", "lausanne"]).unwrap();
        match w.c {
            ContextCommand::Show(a) => assert_eq!(a.name.as_deref(), Some("lausanne")),
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn create_parses_with_required_args() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from([
            "x",
            "add",
            "lausanne",
            "--docker",
            "ssh://user@host",
            "--use",
        ])
        .unwrap();
        match w.c {
            ContextCommand::Add(a) => {
                assert_eq!(a.name, "lausanne");
                assert_eq!(a.docker, "ssh://user@host");
                assert!(a.r#use);
                assert!(a.description.is_none());
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn rm_parses_multiple_names() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        let w = Wrap::try_parse_from(["x", "rm", "a", "b", "c", "--force"]).unwrap();
        match w.c {
            ContextCommand::Rm(a) => {
                assert_eq!(a.names, vec!["a", "b", "c"]);
                assert!(a.force);
            }
            other => panic!("expected Rm, got {other:?}"),
        }
    }
}
