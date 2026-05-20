//! `isd join-token`: mint a join-token via the controller and print
//! the operator-visible `isd join ...` invocation.
//!
//! Runs `docker exec iso-controller isengard controller token mint
//! --role agent --format joincmd` against the operator's current
//! docker context. The command output is a single line ready to paste.

use anyhow::{Context, Result, anyhow};
use bollard::exec::{CreateExecOptions, StartExecResults};
use clap::Args;
use futures_util::StreamExt;

/// CLI flags for `isd join-token`.
#[derive(Debug, Args)]
pub struct JoinTokenArgs {
    /// TTL for the minted token (e.g. `15m`, `1h`).
    #[arg(long, default_value = "15m")]
    pub ttl: humantime::Duration,
}

/// Mint a join-token via the controller and print the operator-visible
/// `isd join ...` invocation on stdout.
///
/// Resolves the operator's docker context, attaches to iso-controller,
/// and runs `isengard controller token mint --format joincmd`. The
/// stream is validated (`isd join`-prefixed, contains `--token`) before
/// printing so a broken controller binary can't poison the operator's
/// copy-paste.
///
/// # Errors
///
/// Returns `Err` when no controller is running on the current context,
/// when docker exec fails, or when the controller emits an output that
/// doesn't parse as a `isd join` command.
pub async fn run(args: JoinTokenArgs, context: Option<&str>) -> Result<()> {
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    let docker = isd_runtime::DockerBackend::from_uri(&docker_uri).await?;

    let ttl_str = format!("{}", args.ttl);
    let exec = docker
        .client()
        .create_exec(
            "iso-controller",
            CreateExecOptions::<String> {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec![
                    "isengard".into(),
                    "controller".into(),
                    "token".into(),
                    "mint".into(),
                    "--role".into(),
                    "agent".into(),
                    "--ttl".into(),
                    ttl_str,
                    "--format".into(),
                    "joincmd".into(),
                ]),
                ..Default::default()
            },
        )
        .await
        .context("creating exec for join-token mint")?;

    let mut output = String::new();
    if let StartExecResults::Attached {
        output: mut stream, ..
    } = docker.client().start_exec(&exec.id, None).await?
    {
        while let Some(item) = stream.next().await {
            let chunk = item.context("reading join-token output")?;
            output.push_str(&chunk.to_string());
        }
    }
    let line = output.trim().to_string();
    if !line.starts_with("isd join") || !line.contains("--token") {
        return Err(anyhow!(
            "controller did not return a valid join command (got {} bytes: {:?}); is the controller running on this context and was MintFormat::Joincmd matched?",
            line.len(),
            line.lines().next().unwrap_or("")
        ));
    }
    println!("{line}");
    Ok(())
}
