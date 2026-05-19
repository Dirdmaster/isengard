//! Track H: `isd context` reads docker's context store. `list`, `use`,
//! `show` delegate to `docker_context`. `create` / `import` / `rm`
//! are gone: operator runs `docker context create/rm` directly.

use anyhow::Result;
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};

use crate::docker_context;

#[derive(Debug, Args)]
pub struct ContextArgs {
    #[command(subcommand)]
    pub command: ContextCommand,
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    /// List docker contexts. `*` marks the current one.
    List,
    /// Set the current context (same effect as `docker context use <name>`).
    Use(UseArgs),
    /// Print one context's docker endpoint.
    Show(ShowArgs),
}

#[derive(Debug, Args)]
pub struct UseArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub name: Option<String>,
}

pub async fn run(args: ContextArgs) -> Result<()> {
    match args.command {
        ContextCommand::List => run_list().await,
        ContextCommand::Use(a) => run_use(a).await,
        ContextCommand::Show(a) => run_show(a).await,
    }
}

async fn run_list() -> Result<()> {
    let contexts = docker_context::list_contexts()?;
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["", "NAME", "KIND", "TARGET"]);
    for ctx in &contexts {
        t.add_row(vec![
            if ctx.current {
                "*".to_string()
            } else {
                "".to_string()
            },
            ctx.name.clone(),
            ctx.kind.to_string(),
            ctx.target.clone(),
        ]);
    }
    println!("{t}");
    Ok(())
}

async fn run_use(args: UseArgs) -> Result<()> {
    docker_context::set_current_context(&args.name)?;
    println!("Current docker context is now {:?}.", args.name);
    Ok(())
}

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
        let w = Wrap::try_parse_from(["x", "list"]).unwrap();
        assert!(matches!(w.c, ContextCommand::List));

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
    fn create_import_rm_are_not_accepted_anymore() {
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextCommand,
        }
        assert!(Wrap::try_parse_from(["x", "create", "lausanne", "--docker", "u"]).is_err());
        assert!(Wrap::try_parse_from(["x", "import", "lausanne"]).is_err());
        assert!(Wrap::try_parse_from(["x", "rm", "lausanne"]).is_err());
    }
}
