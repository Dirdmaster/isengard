//! `isd deploy` / `isd diff` / `isd edit` (v0.3d compose-as-truth).
//!
//! Talk to the dashboard REST surface:
//!
//!  - `GET  /api/v1/stacks/{id}/compose` (read the current YAML + sha256)
//!  - `POST /api/v1/stacks/{id}/diff`    (preview a reconcile plan)
//!  - `PUT  /api/v1/stacks/{id}/compose` (apply with optimistic concurrency)
//!  - `POST /api/v1/stacks`              (create a fresh stack from compose)
//!
//! `isd deploy` is the operator's "ship this" verb: list stacks, find by
//! name, GET the current YAML for the diff, POST diff for the plan, prompt
//! y/N, PUT the new YAML — OR, if the stack doesn't exist yet, POST
//! /stacks to create + write in a single round-trip. `isd diff` stops at
//! the plan render. `isd edit` is `isd deploy` driven from `$EDITOR`
//! against the controller's current YAML.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use isengard_agent::compose_reconciler::parse_compose_path;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::session::Session;
use crate::watch;

#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Path to a local stack file (TOML or YAML). Optional: when
    /// omitted, `isd deploy` looks for `./stack.toml`, then `./stack.yml`,
    /// then `./compose.{toml,yml}` in the current directory. The new
    /// one-file stack model expects a top-level `name:` key in the
    /// stack file itself.
    pub path: Option<PathBuf>,
    /// Target host ULID for first-time deploys. Optional: when omitted
    /// and exactly one host is enrolled (the homelab single-host case),
    /// the controller auto-picks. Ignored on subsequent deploys (the
    /// stack already has a host).
    #[arg(long)]
    pub host_id: Option<String>,
    /// Skip the interactive y/N prompt. CI and scripted use.
    #[arg(long)]
    pub yes: bool,
    /// Force overwrite even when the on-disk file has drifted from the
    /// hash the dashboard / controller last saw. Dangerous: blows away
    /// concurrent operator edits.
    #[arg(long)]
    pub force: bool,
    /// Manifest-era flags. Hidden from clap while the teardown is in
    /// flight (Task 10 deletes them entirely). Kept as struct fields so
    /// the surrounding code compiles unchanged this commit.
    #[arg(skip)]
    #[allow(dead_code)]
    pub stack: Option<String>,
    #[arg(skip)]
    #[allow(dead_code)]
    pub all: bool,
    #[arg(skip)]
    #[allow(dead_code)]
    pub root: Option<PathBuf>,
    #[arg(skip)]
    #[allow(dead_code)]
    pub overlay: Option<String>,
    #[arg(skip)]
    #[allow(dead_code)]
    pub strategy: Option<String>,
    #[arg(skip)]
    #[allow(dead_code)]
    pub fail_fast: bool,
    #[arg(skip)]
    #[allow(dead_code)]
    pub diff: bool,
    /// v0.5.2: stream per-service state transitions until every service
    /// reaches a terminal state. ON by default. Pass `--detach` to
    /// revert to the pre-v0.5.2 fire-and-forget shape. Polls
    /// `GET /api/v1/services?stack_id=...` every 1s and renders one
    /// cliclack line per observed transition. Ctrl+C detaches without
    /// canceling the deploy on the agent side.
    #[arg(long)]
    pub detach: bool,
}

impl DeployArgs {
    /// True when state transitions should be streamed to the terminal.
    /// Default: on (operator opts out via `--detach`).
    pub fn watch(&self) -> bool {
        !self.detach
    }
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

pub async fn run_deploy(args: DeployArgs, context: Option<&str>) -> Result<()> {
    // One-file stack model (Track A, 2026-05-15): resolve the stack
    // file, parse it via the agent's canonical parser, read the
    // top-level `name:`, ship the file body to the controller as
    // YAML (TOML gets translated on the operator side). The old
    // manifest layer (overlays, multi-file compose lists, hooks,
    // stack-level strategy) is gone; per-service strategy lives in
    // the stack file's `services.<name>.strategy` key now.
    let path = match args.path.as_deref() {
        Some(p) if p.as_os_str() == "-" => {
            return Err(anyhow!(
                "stdin (`-`) input is no longer supported; pass a file path or run \
                 `isd deploy` in the stack dir"
            ));
        }
        Some(p) => p.to_path_buf(),
        None => default_stack_file()?,
    };

    // Parse the file to extract the stack name. Reuses the agent's
    // canonical parser so the wire shape stays consistent.
    let dc = parse_compose_path(&path).with_context(|| format!("parse {}", path.display()))?;
    let stack_name = dc.name.clone().ok_or_else(|| {
        anyhow!(
            "stack file {} is missing a top-level `name` key",
            path.display()
        )
    })?;

    // Read the on-disk body. TOML stack files get translated to YAML
    // for the wire (the agent persists YAML only); YAML passes through.
    let body = read_compose_path(&path)?;

    let session = Session::open(context).await?;
    ship_stack(
        &session,
        &stack_name,
        &body,
        args.host_id.as_deref(),
        args.yes,
        args.force,
        args.watch(),
    )
    .await
}

/// Resolve the default stack file when `isd deploy` is invoked without
/// a positional path. Probes `stack.toml`, `stack.yml`, `compose.toml`,
/// `compose.yml` in that order. The new one-file shape lives in
/// `stack.{toml,yml}`; `compose.{toml,yml}` is a soft fallback for
/// legacy bare composes (they need a top-level `name:` to parse).
fn default_stack_file() -> Result<PathBuf> {
    for candidate in &["stack.toml", "stack.yml", "compose.toml", "compose.yml"] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "no stack file found in the current directory (looked for stack.toml, \
         stack.yml, compose.toml, compose.yml)"
    ))
}

/// Ship a parsed stack to the controller: create if first-time, diff +
/// apply otherwise. Body is the on-disk YAML (TOML stack files arrive
/// here pre-translated by `read_compose_path`). No manifest fields ship
/// on the wire; per-service strategy is encoded in the YAML itself.
async fn ship_stack(
    session: &Session,
    stack: &str,
    body: &str,
    host_id: Option<&str>,
    yes: bool,
    force: bool,
    watch: bool,
) -> Result<()> {
    // First-time deploy: stack isn't in the controller's inventory yet.
    // POST /stacks creates the row + ships the YAML to the agent in one
    // round-trip. No useful diff against "nothing"; the operator's
    // confirmation on first deploy is implicit in running the command.
    let stack_id = match resolve_stack_id_opt(session, stack).await? {
        Some(id) => id,
        None => {
            if !yes && !confirm(&format!("Stack {stack:?} doesn't exist; create + deploy?"))? {
                println!("Aborted.");
                return Ok(());
            }
            let outcome = create_stack(session, stack, body, host_id).await?;
            println!(
                "Created stack {:?} (id {}, host {}). New sha256: {}",
                outcome.name, outcome.id, outcome.host_id, outcome.written_sha256,
            );
            if watch {
                watch::run_watch(session, &outcome.id).await?;
            }
            return Ok(());
        }
    };

    // Subsequent deploy: diff vs current, prompt y/N, PUT.
    let current = fetch_compose(session, &stack_id).await?;
    let plan = preview_diff(session, &stack_id, body).await?;

    println!("Stack: {} (id {})", stack, stack_id);
    println!();
    print_unified_diff(
        current
            .as_ref()
            .map(|c| c.compose_yaml.as_str())
            .unwrap_or(""),
        body,
    );
    println!();
    print_plan(&plan);
    if plan
        .ops
        .iter()
        .all(|o| matches!(o, ServiceOp::NoChange { .. }))
    {
        println!("Nothing to deploy.");
        return Ok(());
    }

    if !yes && !confirm("Deploy?")? {
        println!("Aborted.");
        return Ok(());
    }

    let expected = current
        .as_ref()
        .map(|c| c.sha256.clone())
        .unwrap_or_default();
    let outcome = put_compose(session, &stack_id, body, &expected, force).await?;
    println!("Deployed. New sha256: {}", outcome.written_sha256);
    if watch {
        watch::run_watch(session, &stack_id).await?;
    }
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
            .context("reading compose from stdin")?;
        Ok(buf)
    } else {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        // TOML compose: convert to YAML on the operator side so the
        // wire and on-agent persistence stay YAML-only. Agent never
        // sees TOML. Lossy for comments (already true after any
        // parser round-trip).
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            return toml_compose_to_yaml(&content)
                .with_context(|| format!("converting {} to yaml", path.display()));
        }
        Ok(content)
    }
}

fn toml_compose_to_yaml(toml_content: &str) -> Result<String> {
    let value: toml::Value = toml::from_str(toml_content).context("parsing toml")?;
    if !matches!(value, toml::Value::Table(_)) {
        return Err(anyhow!("compose.toml root must be a table"));
    }
    // 2026-05-15 stack file model: the TOML shape mirrors the YAML
    // shape exactly (a top-level `services` table plus top-level
    // `name` / `secrets` / `networks` / `volumes` keys). A straight
    // structural translation is enough; the agent's parser does the
    // real decode.
    let json = toml_value_to_json(value);
    serde_yaml::to_string(&json).context("serializing yaml")
}

fn toml_value_to_json(v: toml::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.into_iter().map(toml_value_to_json).collect()),
        toml::Value::Table(tbl) => Value::Object(
            tbl.into_iter()
                .map(|(k, v)| (k, toml_value_to_json(v)))
                .collect(),
        ),
    }
}

async fn resolve_stack_id(session: &Session, name: &str) -> Result<String> {
    resolve_stack_id_opt(session, name)
        .await?
        .ok_or_else(|| anyhow!("stack {name:?} not found on controller"))
}

/// Like [`resolve_stack_id`] but returns `Ok(None)` for the not-found
/// case instead of erroring. Used by `isd deploy` so it can branch into
/// the create-from-scratch path when the operator deploys a stack that
/// isn't yet in the controller's inventory.
async fn resolve_stack_id_opt(session: &Session, name: &str) -> Result<Option<String>> {
    let url = format!("{}/api/v1/stacks", session.controller_url());
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let stacks: Vec<StackDto> = resp.error_for_status()?.json().await?;
    Ok(stacks.into_iter().find(|s| s.name == name).map(|s| s.id))
}

#[derive(Debug, Serialize)]
struct CreateStackBody<'a> {
    name: &'a str,
    compose_yaml: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct CreateStackOk {
    id: String,
    name: String,
    host_id: String,
    written_sha256: String,
}

async fn create_stack(
    session: &Session,
    name: &str,
    compose_yaml: &str,
    host_id: Option<&str>,
) -> Result<CreateStackOk> {
    let url = format!("{}/api/v1/stacks", session.controller_url());
    let resp = session
        .client
        .post(&url)
        .json(&CreateStackBody {
            name,
            compose_yaml,
            host_id,
        })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("POST {url} -> {status}: {body}"));
    }
    let ok: CreateStackOk = resp
        .json()
        .await
        .context("decoding create-stack response")?;
    Ok(ok)
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
