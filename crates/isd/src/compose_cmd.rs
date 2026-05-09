//! `isd apply` / `isd diff` / `isd edit` (v0.3d compose-as-truth).
//!
//! All three subcommands talk to the same dashboard REST surface:
//!
//!  - `GET  /api/v1/stacks/{id}/compose` (read the current YAML + sha256)
//!  - `POST /api/v1/stacks/{id}/diff`    (preview a reconcile plan)
//!  - `PUT  /api/v1/stacks/{id}/compose` (apply with optimistic concurrency)
//!
//! `isd apply` and `isd edit` chain GET -> POST diff -> show plan ->
//! prompt y/N -> PUT. `isd diff` stops at the plan render (no PUT).

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::session::Session;

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Path to the local compose.yaml. Use `-` to read from stdin.
    pub path: PathBuf,
    /// Stack name override. Defaults to the file's parent directory.
    #[arg(long)]
    pub stack: Option<String>,
    /// Skip the interactive y/N prompt. CI and scripted use.
    #[arg(long)]
    pub yes: bool,
    /// Force overwrite even when the on-disk file has drifted from the
    /// hash the dashboard / controller last saw. Dangerous: blows away
    /// concurrent operator edits.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Stack name (matches the compose project / `isengard.stack` label).
    pub stack: String,
    /// Optional path to a local compose.yaml. When set, diff `path` vs
    /// the controller's stored copy. When omitted, diff the controller's
    /// copy vs an empty compose (i.e. "what would isd apply on an empty
    /// file remove?").
    pub path: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Stack name. The current compose.yaml is fetched into a temp
    /// file, opened in `$EDITOR` (default `vi`), and applied on save.
    pub stack: String,
    /// Skip the interactive y/N prompt after the editor exits. Useful
    /// for `EDITOR='sed -i ...' isd edit hello --yes`.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Deserialize)]
struct StackDto {
    id: String,
    name: String,
}

// The dashboard's compose endpoint emits stack_id as an integer while the
// stacks-list endpoint emits it as a string (StackId is a typed wrapper that
// serializes differently in the two paths). Skip stack_id entirely here:
// the caller already knows the ID it requested. Same story for stack_name +
// imported_at; we just need the YAML and the sha for optimistic concurrency.
#[derive(Debug, Deserialize)]
struct ComposeResponse {
    compose_yaml: String,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReconcilePlan {
    stack: String,
    ops: Vec<ServiceOp>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ServiceOp {
    Start {
        service: String,
        image: String,
    },
    Recreate {
        service: String,
        image: String,
        reasons: Vec<String>,
    },
    Stop {
        service: String,
    },
    NoChange {
        service: String,
    },
}

impl ServiceOp {
    #[allow(dead_code)] // used by render helpers + tests
    fn service(&self) -> &str {
        match self {
            ServiceOp::Start { service, .. }
            | ServiceOp::Recreate { service, .. }
            | ServiceOp::Stop { service }
            | ServiceOp::NoChange { service } => service,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PutOk {
    written_sha256: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // surfaced verbatim in user-facing error messages
struct PutConflict {
    error: String,
    current_sha256: String,
    current_yaml: String,
}

pub async fn run_apply(args: ApplyArgs, context: Option<&str>) -> Result<()> {
    let body = read_compose_path(&args.path)?;
    let stack = match args.stack.as_deref() {
        Some(s) => s.to_string(),
        None => stack_from_path(&args.path)?,
    };
    let session = Session::open(context).await?;
    let stack_id = resolve_stack_id(&session, &stack).await?;
    let current = fetch_compose(&session, &stack_id).await?;
    let plan = preview_diff(&session, &stack_id, &body).await?;

    println!("Stack: {} (id {})", stack, stack_id);
    println!();
    print_unified_diff(
        current
            .as_ref()
            .map(|c| c.compose_yaml.as_str())
            .unwrap_or(""),
        &body,
    );
    println!();
    print_plan(&plan);
    if plan
        .ops
        .iter()
        .all(|o| matches!(o, ServiceOp::NoChange { .. }))
    {
        println!("Nothing to apply.");
        return Ok(());
    }

    if !args.yes && !confirm("Apply?")? {
        println!("Aborted.");
        return Ok(());
    }

    let expected = current
        .as_ref()
        .map(|c| c.sha256.clone())
        .unwrap_or_default();
    let outcome = put_compose(&session, &stack_id, &body, &expected, args.force).await?;
    println!("Applied. New sha256: {}", outcome.written_sha256);
    Ok(())
}

pub async fn run_diff(args: DiffArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let stack_id = resolve_stack_id(&session, &args.stack).await?;
    let current = fetch_compose(&session, &stack_id).await?;
    let proposed = match args.path.as_ref() {
        Some(p) => read_compose_path(p)?,
        None => String::new(),
    };
    let current_yaml = current
        .as_ref()
        .map(|c| c.compose_yaml.as_str())
        .unwrap_or("");

    print_unified_diff(current_yaml, &proposed);
    let plan = preview_diff(&session, &stack_id, &proposed).await?;
    println!();
    print_plan(&plan);
    Ok(())
}

pub async fn run_edit(args: EditArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let stack_id = resolve_stack_id(&session, &args.stack).await?;
    let current = fetch_compose(&session, &stack_id).await?;
    let current_yaml = current
        .as_ref()
        .map(|c| c.compose_yaml.clone())
        .unwrap_or_default();
    let expected = current
        .as_ref()
        .map(|c| c.sha256.clone())
        .unwrap_or_default();

    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("isd-{}-", args.stack))
        .suffix(".yaml")
        .tempfile()
        .context("creating temp file for editor")?;
    tmp.write_all(current_yaml.as_bytes())
        .context("writing current YAML to temp file")?;
    let path = tmp.path().to_path_buf();
    tmp.flush().ok();

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launching $EDITOR ({editor})"))?;
    if !status.success() {
        return Err(anyhow!("editor exited with status {status}"));
    }

    let edited = std::fs::read_to_string(&path).context("reading edited file")?;
    if edited == current_yaml {
        println!("No changes; nothing to apply.");
        return Ok(());
    }

    let plan = preview_diff(&session, &stack_id, &edited).await?;
    print_unified_diff(&current_yaml, &edited);
    println!();
    print_plan(&plan);
    if plan
        .ops
        .iter()
        .all(|o| matches!(o, ServiceOp::NoChange { .. }))
    {
        println!("Nothing to apply (edits had no observable effect).");
        return Ok(());
    }

    if !args.yes && !confirm("Apply?")? {
        println!("Aborted.");
        return Ok(());
    }
    let outcome = put_compose(&session, &stack_id, &edited, &expected, false).await?;
    println!("Applied. New sha256: {}", outcome.written_sha256);
    Ok(())
}

fn read_compose_path(path: &std::path::Path) -> Result<String> {
    if path == std::path::Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading compose YAML from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }
}

fn stack_from_path(path: &std::path::Path) -> Result<String> {
    if path == std::path::Path::new("-") {
        return Err(anyhow!(
            "stack name cannot be inferred from stdin; pass --stack <name>"
        ));
    }
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str());
    parent.map(str::to_string).ok_or_else(|| {
        anyhow!(
            "could not infer stack name from {}; pass --stack",
            path.display()
        )
    })
}

async fn resolve_stack_id(session: &Session, name: &str) -> Result<String> {
    let url = format!("{}/api/v1/stacks", session.controller_url());
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let stacks: Vec<StackDto> = resp.error_for_status()?.json().await?;
    let stack = stacks
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow!("stack {name:?} not found on controller"))?;
    Ok(stack.id)
}

async fn fetch_compose(session: &Session, stack_id: &str) -> Result<Option<ComposeResponse>> {
    let url = format!(
        "{}/api/v1/stacks/{stack_id}/compose",
        session.controller_url()
    );
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    let cr: ComposeResponse = resp.error_for_status()?.json().await?;
    Ok(Some(cr))
}

async fn preview_diff(session: &Session, stack_id: &str, proposed: &str) -> Result<ReconcilePlan> {
    let url = format!("{}/api/v1/stacks/{stack_id}/diff", session.controller_url());
    let resp = session
        .client
        .post(&url)
        .header("Content-Type", "application/yaml")
        .body(proposed.to_string())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let plan: ReconcilePlan = resp.error_for_status()?.json().await?;
    Ok(plan)
}

async fn put_compose(
    session: &Session,
    stack_id: &str,
    body: &str,
    expected_sha256: &str,
    force: bool,
) -> Result<PutOk> {
    let mut url = format!(
        "{}/api/v1/stacks/{stack_id}/compose",
        session.controller_url()
    );
    if force {
        url.push_str("?force=true");
    }
    let mut req = session
        .client
        .put(&url)
        .header("Content-Type", "application/yaml")
        .body(body.to_string());
    if !expected_sha256.is_empty() {
        req = req.header("If-Match", expected_sha256);
    }
    let resp = req.send().await.with_context(|| format!("PUT {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::CONFLICT {
        let conflict: PutConflict = resp.json().await.context("decoding 409 body")?;
        return Err(anyhow!(
            "conflict: {}\n  current_sha256: {}\n  rerun with --force to overwrite (loses concurrent edits)",
            conflict.error,
            conflict.current_sha256,
        ));
    }
    let ok: PutOk = resp.error_for_status()?.json().await?;
    Ok(ok)
}

fn print_unified_diff(current: &str, proposed: &str) {
    let diff = TextDiff::from_lines(current, proposed);
    let mut wrote_anything = false;
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        if matches!(change.tag(), similar::ChangeTag::Equal) {
            // Skip context lines for compactness; v0.3d's `isd diff`
            // is meant to show what changed, not the full file. The
            // dashboard renders the full diff side-by-side.
            continue;
        }
        wrote_anything = true;
        print!("{prefix}{change}");
    }
    if !wrote_anything {
        println!("(no textual changes)");
    }
}

fn print_plan(plan: &ReconcilePlan) {
    println!("Reconcile plan ({} ops):", plan.ops.len());
    for op in &plan.ops {
        match op {
            ServiceOp::NoChange { service } => {
                println!("  ~ {service:<24} no change");
            }
            ServiceOp::Start { service, image } => {
                println!("  + {service:<24} start ({image})");
            }
            ServiceOp::Recreate {
                service,
                image,
                reasons,
            } => {
                println!("  ! {service:<24} recreate ({image})");
                for r in reasons {
                    println!("      reason: {r}");
                }
            }
            ServiceOp::Stop { service } => {
                println!("  - {service:<24} stop");
            }
        }
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "no TTY for confirmation; pass --yes to skip the prompt"
        ));
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(
        buf.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_from_path_uses_parent_directory() {
        let p = std::path::PathBuf::from("/etc/isengard/stacks/hello/compose.yaml");
        assert_eq!(stack_from_path(&p).unwrap(), "hello");
    }

    #[test]
    fn stack_from_path_rejects_stdin() {
        let p = std::path::PathBuf::from("-");
        assert!(stack_from_path(&p).is_err());
    }

    #[test]
    fn print_plan_renders_op_kinds() {
        let plan = ReconcilePlan {
            stack: "hello".into(),
            ops: vec![
                ServiceOp::Start {
                    service: "web".into(),
                    image: "nginx".into(),
                },
                ServiceOp::Stop {
                    service: "old".into(),
                },
                ServiceOp::Recreate {
                    service: "api".into(),
                    image: "api:v2".into(),
                    reasons: vec!["image: api:v1 -> api:v2".into()],
                },
                ServiceOp::NoChange {
                    service: "db".into(),
                },
            ],
        };
        // Just verify the function doesn't panic. The output goes to stdout.
        print_plan(&plan);
    }
}
