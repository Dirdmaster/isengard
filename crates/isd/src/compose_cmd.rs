//! `isd stack up` / `isd stack diff` / `isd stack edit` (v0.3d
//! compose-as-truth).
//!
//! Talk to the dashboard REST surface:
//!
//!  - `GET  /api/v1/stacks/{id}/compose` (read the current YAML + sha256)
//!  - `POST /api/v1/stacks/{id}/diff`    (preview a reconcile plan)
//!  - `PUT  /api/v1/stacks/{id}/compose` (apply with optimistic concurrency)
//!  - `POST /api/v1/stacks`              (create a fresh stack from compose)
//!
//! `isd stack up` is the operator's "ship this" verb: list stacks, find by
//! name, GET the current YAML for the diff, POST diff for the plan, prompt
//! y/N, PUT the new YAML. If the stack doesn't exist yet, POST
//! /stacks creates + writes in a single round-trip. `isd stack diff` stops
//! at the plan render. `isd stack edit` is `isd stack up` driven from
//! `$EDITOR` against the controller's current YAML.
//!
//! Internal names still use the historical "deploy" terminology
//! (`DeployArgs`, `run_deploy`, `DeployPlan`) since the semantics
//! (deploying a compose stack to the controller) are unchanged. Only the
//! CLI verb shortened.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::session::Session;
use crate::watch;

/// CLI flags for `isd stack up`.
#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Path to compose file or directory with stack.toml.
    pub path: Option<PathBuf>,
    /// Stack name override.
    #[arg(long)]
    pub stack: Option<String>,
    /// Target host ULID for first-time deploys.
    #[arg(long)]
    pub host_id: Option<String>,
    /// Skip the interactive y/N prompt.
    #[arg(long)]
    pub yes: bool,
    /// Force overwrite even on hash drift.
    #[arg(long)]
    pub force: bool,
    /// Deploy every subdir containing a stack.toml.
    #[arg(long)]
    pub all: bool,
    /// Root for --all (defaults to cwd).
    #[arg(long, requires = "all")]
    pub root: Option<PathBuf>,
    /// Apply a named overlay from the manifest.
    #[arg(long)]
    pub overlay: Option<String>,
    /// Override the manifest's strategy for this run.
    #[arg(long)]
    pub strategy: Option<String>,
    /// With --all, stop at the first failing stack.
    #[arg(long)]
    pub fail_fast: bool,
    /// Print the plan and exit without applying.
    #[arg(long)]
    pub diff: bool,
    /// Don't stream state transitions after deploy.
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

/// CLI flags for `isd stack diff`.
#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Stack name.
    pub stack: String,
    /// Optional local compose.yaml to diff against.
    pub path: Option<PathBuf>,
}

/// CLI flags for `isd stack edit`.
#[derive(Debug, Args)]
pub struct EditArgs {
    /// Stack name.
    pub stack: String,
    /// Skip the y/N prompt after the editor exits.
    #[arg(long)]
    pub yes: bool,
}

/// Subset of the dashboard's stack DTO used for name -> id lookup.
#[derive(Debug, Deserialize)]
struct StackDto {
    /// Stringified surrogate key.
    id: String,
    /// Operator-facing stack name.
    name: String,
}

// The dashboard's compose endpoint emits stack_id as an integer while the
// stacks-list endpoint emits it as a string (StackId is a typed wrapper that
// serializes differently in the two paths). Skip stack_id entirely here:
// the caller already knows the ID it requested. Same story for stack_name +
// imported_at; we just need the YAML and the sha for optimistic concurrency.
/// Response shape of `GET /api/v1/stacks/<id>/compose`.
#[derive(Debug, Deserialize)]
struct ComposeResponse {
    /// The stack's compose YAML, verbatim.
    compose_yaml: String,
    /// SHA-256 of the YAML. Used as the `If-Match` ETag.
    sha256: String,
}

/// Reconcile plan returned by `POST /api/v1/stacks/<id>/diff`.
#[derive(Debug, Deserialize, Serialize)]
struct ReconcilePlan {
    /// Stack name.
    stack: String,
    /// Per-service operations the controller will execute.
    ops: Vec<ServiceOp>,
}

/// One operation in a reconcile plan.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ServiceOp {
    /// Bring up a service that wasn't previously running.
    Start {
        /// Service name.
        service: String,
        /// Image to pull/run.
        image: String,
    },
    /// Replace the existing container with a new one (image / env /
    /// labels changed).
    Recreate {
        /// Service name.
        service: String,
        /// Target image.
        image: String,
        /// Operator-readable reasons the service needs to be
        /// recreated.
        reasons: Vec<String>,
    },
    /// Tear down a service that was removed from the compose.
    Stop {
        /// Service name.
        service: String,
    },
    /// Service is converged; nothing to do.
    NoChange {
        /// Service name.
        service: String,
    },
}

impl ServiceOp {
    /// Pull the service name out of any variant. Used by render
    /// helpers and tests.
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

/// 2xx body from `PUT /compose`.
#[derive(Debug, Deserialize)]
struct PutOk {
    /// SHA-256 of the YAML the controller has stored.
    written_sha256: String,
}

/// 409 body: concurrent edit detected.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // surfaced verbatim in user-facing error messages
struct PutConflict {
    /// Operator-readable error message.
    error: String,
    /// SHA-256 the controller currently sees.
    current_sha256: String,
    /// YAML body the controller currently holds.
    current_yaml: String,
}

/// Entry point for `isd stack up`. Classifies the args into one of three
/// modes (manifest, single compose, `--all`) and dispatches.
///
/// # Errors
///
/// Returns `Err` on classification failure, file IO, controller HTTP
/// failure, or operator abort.
pub async fn run_deploy(args: DeployArgs, context: Option<&str>) -> Result<()> {
    // Dispatch. `--all` walks subdirs; otherwise we may
    // have a manifest (`stack.toml` in cwd or the supplied dir) or a
    // bare compose file (legacy).
    let plan = resolve_deploy_plan(&args)?;
    match plan {
        DeployPlan::All { root } => return run_all_deploy(args, root, context).await,
        DeployPlan::Manifest { manifest_path } => {
            return run_manifest_deploy(args, manifest_path, context).await;
        }
        DeployPlan::Single { compose_path } => {
            // Fall through to the legacy single-compose path below.
            run_single_compose(args, compose_path, context).await
        }
    }
}

/// Classify what `isd stack up` is being asked to do.
#[derive(Debug)]
pub enum DeployPlan {
    /// `--all`: walk immediate subdirs of `root` and deploy each
    /// stack found.
    All {
        /// Root to enumerate from (cwd by default).
        root: PathBuf,
    },
    /// A `stack.toml` was located; deploy from it.
    Manifest {
        /// Path to the located `stack.toml`.
        manifest_path: PathBuf,
    },
    /// A bare compose file was supplied; legacy single-file path.
    Single {
        /// Path to the compose YAML (or `-` for stdin).
        compose_path: PathBuf,
    },
}

/// Classify the args. Precedence:
///   1. `--all` set: All { root: cwd or `--root` }
///   2. positional `-`: Single { compose_path: "-" } (stdin)
///   3. positional is an existing directory: probe for `stack.toml`,
///      `compose.toml`, `compose.yml`, `compose.yaml` in that order;
///      first match wins. Dir exists but no manifest -> explicit error.
///   4. positional is an existing file named `stack.toml`: Manifest
///   5. positional is some other existing file: Single (legacy compose)
///   6. positional has a path separator or `.`/`/` prefix but does NOT
///      exist: explicit error (treated as path, not stack name)
///   7. positional is a bare name (no separator) that does NOT exist:
///      explicit error suggesting `./<name>` or stack-name lookup
///   8. no positional and `./stack.toml` exists: Manifest { cwd/stack.toml }
///   9. no positional and no `./stack.toml`: error
///
/// Wave 5.A: bare names that match no on-disk file or dir now error
/// explicitly instead of silently falling through to the legacy Single
/// compose path with a "No such file or directory" message. Operator
/// either passes `./<name>` (the path-resolver does the right thing) or
/// gets a clear hint to do so.
pub fn resolve_deploy_plan(args: &DeployArgs) -> Result<DeployPlan> {
    if args.all {
        let root = args
            .root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        return Ok(DeployPlan::All { root });
    }
    match args.path.as_deref() {
        Some(p) if p == std::path::Path::new("-") => Ok(DeployPlan::Single {
            compose_path: PathBuf::from("-"),
        }),
        Some(p) => resolve_positional_arg(p),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let manifest = cwd.join("stack.toml");
            if manifest.exists() {
                Ok(DeployPlan::Manifest {
                    manifest_path: manifest,
                })
            } else {
                Err(anyhow!(
                    "no stack.toml in cwd; pass a path or run `isd stack up` \
                     to deploy a single compose file"
                ))
            }
        }
    }
}

/// Wave 5.A: resolve a positional `<path>` arg with explicit error
/// messages for the bare-name + non-existent cases. Splits out of
/// [`resolve_deploy_plan`] to keep the table-of-precedence readable.
/// Resolve a positional path arg to a [`DeployPlan`].
///
/// Probe order: stack.toml -> compose.toml -> compose.yml ->
/// compose.yaml. Bare names that match no on-disk artifact return a
/// `did you mean ./<name>` error; explicit paths (with `./`, `/`, or
/// a separator) return a generic "does not exist" error.
fn resolve_positional_arg(p: &std::path::Path) -> Result<DeployPlan> {
    // Directory case: probe for stack.toml, then compose.{toml,yml,yaml}.
    if p.is_dir() {
        let manifest = p.join("stack.toml");
        if manifest.exists() {
            return Ok(DeployPlan::Manifest {
                manifest_path: manifest,
            });
        }
        for filename in ["compose.toml", "compose.yml", "compose.yaml"] {
            let candidate = p.join(filename);
            if candidate.exists() {
                return Ok(DeployPlan::Single {
                    compose_path: candidate,
                });
            }
        }
        return Err(anyhow!(
            "{} is a directory but contains no stack.toml or compose.{{toml,yml,yaml}}; \
             add a stack.toml or pass an explicit compose file path",
            p.display()
        ));
    }

    // Existing file case.
    if p.exists() {
        if p.file_name().and_then(|s| s.to_str()) == Some("stack.toml") {
            return Ok(DeployPlan::Manifest {
                manifest_path: p.to_path_buf(),
            });
        }
        return Ok(DeployPlan::Single {
            compose_path: p.to_path_buf(),
        });
    }

    // Path does not exist. Two diagnostics depending on whether the
    // operator clearly intended a path (has a separator or `.`/`/`
    // prefix) or a bare name (likely meant as a stack name or a
    // subdir-of-cwd lookup).
    if looks_like_explicit_path(p) {
        return Err(anyhow!(
            "{} does not exist; pass a path to a stack.toml, a compose file, \
             or a directory containing one",
            p.display()
        ));
    }
    let display = p.display();
    Err(anyhow!(
        "no file or directory named {display:?} in cwd; \
         did you mean `isd stack up ./{display}`? \
         (bare names are not yet looked up as stack names by `isd stack up`; \
         the path resolver expects a stack.toml, a compose file, or a \
         directory containing one)"
    ))
}

/// Wave 5.A: a path "looks like an explicit path" when it has any
/// component separator or starts with `.` / `..` / `/`. Used to pick
/// between the two non-existent-path error messages.
/// Classify whether `p` reads as an explicit path. Used to pick
/// between the two non-existent-path error messages.
fn looks_like_explicit_path(p: &std::path::Path) -> bool {
    let s = match p.to_str() {
        Some(s) => s,
        None => return true, // non-UTF8: treat as a path, not a name
    };
    s.contains('/')
        || s.contains(std::path::MAIN_SEPARATOR)
        || s.starts_with('.')
        || std::path::Path::new(s).is_absolute()
}

/// Deploy from a `stack.toml`. Merges overlays, builds the
/// JSON body with the manifest fields, and POSTs (or PUTs) via the
/// existing dashboard endpoint.
/// Deploy from a `stack.toml` manifest. Loads the manifest, merges
/// overlay compose files, then either PUTs (existing stack) or POSTs
/// (fresh stack) the full bundle.
async fn run_manifest_deploy(
    args: DeployArgs,
    manifest_path: PathBuf,
    context: Option<&str>,
) -> Result<()> {
    let manifest = isengard_manifest::StackManifest::load(&manifest_path)
        .with_context(|| format!("loading {}", manifest_path.display()))?;
    let stack_name = manifest.name.clone();
    let compose_paths = manifest
        .resolved_compose_paths(args.overlay.as_deref())
        .with_context(|| "resolving compose paths from manifest")?;

    // Read every compose file and merge them in order.
    if compose_paths.is_empty() {
        return Err(anyhow!("manifest's compose list resolved to nothing"));
    }
    let mut compose_bodies = Vec::with_capacity(compose_paths.len());
    for p in &compose_paths {
        let body = read_compose_path(p)?;
        compose_bodies.push(body);
    }
    let (base, overlays) = compose_bodies.split_first().expect("non-empty");
    let merged = isengard_manifest::merge_compose_yaml(base, overlays)
        .map_err(|e| anyhow!("merging compose overlays: {e}"))?;

    let session = Session::open(context).await?;

    let manifest_toml_body = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let strategy_override = args.strategy.clone();
    let secrets: Vec<String> = manifest.secrets.clone();
    let hooks: Vec<JsonHook> = manifest
        .hooks
        .iter()
        .map(|h| JsonHook {
            on: h.on.as_str().to_string(),
            cmd: h.cmd.clone(),
            timeout_ms: h.timeout.as_millis() as u64,
            on_error: h.on_error.as_str().to_string(),
        })
        .collect();

    if args.diff {
        println!("(dry-run) would deploy stack {stack_name}");
        println!("  manifest: {}", manifest_path.display());
        println!("  compose:  {} file(s)", compose_paths.len());
        println!(
            "  strategy: {}",
            strategy_override.unwrap_or_else(|| manifest.strategy.as_str().to_string())
        );
        println!("  secrets:  {}", secrets.len());
        println!("  hooks:    {}", hooks.len());
        return Ok(());
    }

    let stack_id_for_watch: String = match resolve_stack_id_opt(&session, &stack_name).await? {
        Some(stack_id) => {
            // Existing stack: PUT compose with the JSON content-type
            // variant so manifest body, secrets, and hooks propagate
            // alongside the compose. Pre-follow-up isd shipped only the
            // YAML body shape and dropped manifest changes on the floor;
            // operators had to delete + recreate stacks to push a new
            // manifest. With the JSON variant we round-trip the full
            // bundle every time.
            let body = PutComposeJsonBody {
                compose: merged,
                manifest_toml: Some(manifest_toml_body),
                secrets: if secrets.is_empty() {
                    None
                } else {
                    Some(secrets)
                },
                hooks: if hooks.is_empty() { None } else { Some(hooks) },
                force: if args.force { Some(true) } else { None },
                compose_sha256: None,
                manifest_sha256: None,
            };
            let outcome = put_compose_json(&session, &stack_id, &body).await?;
            println!(
                "Deployed {}. sha256: {}",
                stack_name, outcome.written_sha256
            );
            stack_id
        }
        None => {
            let body = CreateStackManifestBody {
                name: stack_name.clone(),
                compose_yaml: merged,
                host_id: args.host_id.clone(),
                manifest_toml: Some(manifest_toml_body),
                secrets: if secrets.is_empty() {
                    None
                } else {
                    Some(secrets)
                },
                hooks: if hooks.is_empty() { None } else { Some(hooks) },
            };
            let outcome = create_stack_with_manifest(&session, &body).await?;
            println!(
                "Created stack {} (id {}, host {}). sha256: {}",
                outcome.name, outcome.id, outcome.host_id, outcome.written_sha256,
            );
            outcome.id
        }
    };
    if args.watch() {
        watch::run_watch(&session, &stack_id_for_watch).await?;
    }
    Ok(())
}

/// Walk the immediate subdirs of `root` (lexical order) and deploy
/// each `stack.toml` manifest in turn.
///
/// Per-stack failures are collected and summarised after the loop.
/// `--fail-fast` stops at the first failure.
async fn run_all_deploy(args: DeployArgs, root: PathBuf, context: Option<&str>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("stack.toml").exists())
        .collect();
    entries.sort();

    if entries.is_empty() {
        return Err(anyhow!(
            "no stack.toml found in immediate subdirs of {}",
            root.display()
        ));
    }

    let mut succeeded = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for dir in entries {
        let manifest_path = dir.join("stack.toml");
        let label = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<stack>")
            .to_string();
        // Build a single-stack args clone for the inner call.
        // `--watch` propagates: with `--all --watch` each stack is
        // deployed and then watched sequentially before moving to the
        // next. Concurrent watching of N stacks would require multiplexed
        // cliclack output; deferred until there's an operator-reported
        // need for it.
        let inner = DeployArgs {
            path: Some(manifest_path.clone()),
            stack: args.stack.clone(),
            host_id: args.host_id.clone(),
            yes: true,
            force: args.force,
            all: false,
            root: None,
            overlay: args.overlay.clone(),
            strategy: args.strategy.clone(),
            fail_fast: false,
            diff: args.diff,
            detach: args.detach,
        };
        match run_manifest_deploy(inner, manifest_path, context).await {
            Ok(()) => {
                println!("  {label:<24} ok");
                succeeded += 1;
            }
            Err(e) => {
                println!("  {label:<24} FAILED  ({e})");
                failed.push((label, format!("{e}")));
                if args.fail_fast {
                    break;
                }
            }
        }
    }
    println!();
    println!("Summary: {succeeded} deployed, {} failed.", failed.len());
    if !failed.is_empty() {
        println!();
        for (name, err) in &failed {
            println!("{name}: {err}");
        }
        return Err(anyhow!("{} stack(s) failed", failed.len()));
    }
    Ok(())
}

/// Legacy single-file compose deploy. Existing v0.3d shape.
/// Legacy path: a single compose file, no manifest. Either POSTs a
/// fresh stack or PUTs a diff against the controller's current YAML.
async fn run_single_compose(
    args: DeployArgs,
    compose_path: PathBuf,
    context: Option<&str>,
) -> Result<()> {
    let body = read_compose_path(&compose_path)?;
    let stack = match args.stack.as_deref() {
        Some(s) => s.to_string(),
        None => stack_from_path(&compose_path)?,
    };
    let session = Session::open(context).await?;

    // First-time deploy: stack isn't in the controller's inventory yet.
    // POST /stacks creates the row + ships the YAML to the agent in one
    // round-trip. We can't show a diff against "nothing" usefully, so
    // skip the plan-preview step here; the operator's confirmation on
    // first deploy is implicit in their having run the command.
    let stack_id = match resolve_stack_id_opt(&session, &stack).await? {
        Some(id) => id,
        None => {
            if !args.yes && !confirm(&format!("Stack {stack:?} doesn't exist; create + deploy?"))? {
                println!("Aborted.");
                return Ok(());
            }
            let outcome = create_stack(&session, &stack, &body, args.host_id.as_deref()).await?;
            println!(
                "Created stack {:?} (id {}, host {}). New sha256: {}",
                outcome.name, outcome.id, outcome.host_id, outcome.written_sha256,
            );
            if args.watch() {
                watch::run_watch(&session, &outcome.id).await?;
            }
            return Ok(());
        }
    };

    // Subsequent deploy: diff vs current, prompt y/N, PUT.
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
        println!("Nothing to deploy.");
        return Ok(());
    }

    if !args.yes && !confirm("Deploy?")? {
        println!("Aborted.");
        return Ok(());
    }

    let expected = current
        .as_ref()
        .map(|c| c.sha256.clone())
        .unwrap_or_default();
    let outcome = put_compose(&session, &stack_id, &body, &expected, args.force).await?;
    println!("Deployed. New sha256: {}", outcome.written_sha256);
    if args.watch() {
        watch::run_watch(&session, &stack_id).await?;
    }
    Ok(())
}

/// Entry point for `isd stack diff`. Fetches current YAML, runs the
/// preview-diff endpoint, prints both.
///
/// # Errors
///
/// Returns `Err` on controller HTTP failure or local file IO.
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

/// Entry point for `isd stack edit`. Drops the operator into `$EDITOR` on
/// the controller's current YAML; diff + plan + confirm + PUT on save.
///
/// # Errors
///
/// Returns `Err` on editor failure, controller HTTP failure, or
/// operator abort.
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

/// Read a compose file from disk or stdin. TOML compose is converted
/// to YAML so the wire and the controller never see TOML.
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

/// Translate a TOML compose body into YAML. The TOML shape mirrors
/// YAML exactly so a straight structural translation is enough; the
/// agent's parser does the real decode.
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

/// Convert a `toml::Value` into a `serde_json::Value` for the
/// TOML -> JSON -> YAML pipeline. Datetimes get rendered as strings;
/// floats outside JSON's representable range collapse to `Null`.
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

/// Derive a stack name from the parent directory of a compose file
/// (e.g. `~/stacks/blog/compose.yaml` -> `blog`). Stdin (`-`) has no
/// inferable name; the operator must pass `--stack`.
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

/// Resolve a stack name to its id, erroring when not found.
async fn resolve_stack_id(session: &Session, name: &str) -> Result<String> {
    resolve_stack_id_opt(session, name)
        .await?
        .ok_or_else(|| anyhow!("stack {name:?} not found on controller"))
}

/// Like [`resolve_stack_id`] but returns `Ok(None)` for the not-found
/// case instead of erroring. Used by `isd stack up` so it can branch into
/// the create-from-scratch path when the operator deploys a stack that
/// isn't yet in the controller's inventory.
async fn resolve_stack_id_opt(session: &Session, name: &str) -> Result<Option<String>> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let stacks: Vec<StackDto> = resp.error_for_status()?.json().await?;
    Ok(stacks.into_iter().find(|s| s.name == name).map(|s| s.id))
}

/// POST body for the legacy single-compose create path.
#[derive(Debug, Serialize)]
struct CreateStackBody<'a> {
    /// Stack name.
    name: &'a str,
    /// Compose YAML body.
    compose_yaml: &'a str,
    /// Optional host pin for first-time placement.
    #[serde(skip_serializing_if = "Option::is_none")]
    host_id: Option<&'a str>,
}

/// 200 body from `POST /api/v1/stacks`.
#[derive(Debug, Deserialize)]
struct CreateStackOk {
    /// New stack's surrogate id.
    id: String,
    /// Stack name (echoed back).
    name: String,
    /// Host the controller placed the stack on.
    host_id: String,
    /// SHA-256 of the YAML the controller stored.
    written_sha256: String,
}

/// POST a new stack with only a compose body (legacy single-file
/// deploy).
async fn create_stack(
    session: &Session,
    name: &str,
    compose_yaml: &str,
    host_id: Option<&str>,
) -> Result<CreateStackOk> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks");
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

/// Hook shape on the create-stack POST body.
#[derive(Debug, Serialize, Clone)]
pub struct JsonHook {
    /// Event the hook fires on (`pre_deploy`, `post_deploy`, ...).
    pub on: String,
    /// Command + args.
    pub cmd: Vec<String>,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
    /// Behaviour when the hook errors (`fail`, `ignore`).
    pub on_error: String,
}

/// Extended POST /stacks body with manifest fields.
#[derive(Debug, Serialize)]
pub struct CreateStackManifestBody {
    /// Stack name.
    pub name: String,
    /// Compose YAML body.
    pub compose_yaml: String,
    /// Optional host pin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Full `stack.toml` body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_toml: Option<String>,
    /// Secret names the stack needs bound at deploy time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,
    /// Lifecycle hooks declared in the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<JsonHook>>,
}

/// POST a stack with manifest body. Surfaces controller's
/// 422 (unknown secrets) verbatim so the operator sees the missing
/// names without an extra round-trip.
/// POST a stack with the full manifest bundle (compose, TOML, secrets,
/// hooks). Replaces the manifest-less `create_stack` path when
/// `isd stack up` runs against a `stack.toml`.
async fn create_stack_with_manifest(
    session: &Session,
    body: &CreateStackManifestBody,
) -> Result<CreateStackOk> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks");
    let resp = session
        .client
        .post(&url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("POST {url} -> {status}: {text}"));
    }
    let ok: CreateStackOk = resp
        .json()
        .await
        .context("decoding create-stack response")?;
    Ok(ok)
}

/// Body for the JSON content-type
/// variant of `PUT /api/v1/stacks/:id/compose`. Mirrors the controller's
/// `PutComposeJsonBody` shape. Used by `isd stack up` so a second deploy
/// with manifest changes actually propagates, instead of silently
/// dropping the new bindings.
#[derive(Debug, Serialize)]
pub struct PutComposeJsonBody {
    /// Compose YAML body.
    pub compose: String,
    /// Updated manifest TOML (when the operator edited a manifest).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_toml: Option<String>,
    /// Secrets the manifest declares (replaces existing bindings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,
    /// Hooks the manifest declares (replaces existing bindings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<JsonHook>>,
    /// Bypass optimistic concurrency. Sent only when `--force` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    /// Expected compose SHA-256. When set, the controller returns 409
    /// on mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_sha256: Option<String>,
    /// Expected manifest SHA-256. Same optimistic concurrency
    /// behaviour as `compose_sha256`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
}

/// PUT a compose + manifest bundle via
/// the JSON variant. Surfaces the controller's 409 / 422 / 400 bodies
/// verbatim so the operator sees the underlying error (e.g. the missing
/// secret name) without an extra round-trip.
/// PUT the JSON variant of `compose`: lets manifest changes, secrets,
/// and hooks ride along with the compose body. The legacy
/// `application/yaml` PUT silently drops these fields, so any deploy
/// from a `stack.toml` must go through this path.
async fn put_compose_json(
    session: &Session,
    stack_id: &str,
    body: &PutComposeJsonBody,
) -> Result<PutOk> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks/{stack_id}/compose");
    let resp = session
        .client
        .put(&url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::CONFLICT {
        let conflict: PutConflict = resp.json().await.context("decoding 409 body")?;
        return Err(anyhow!(
            "conflict: {}\n  current_sha256: {}\n  rerun with --force to overwrite (loses concurrent edits)",
            conflict.error,
            conflict.current_sha256,
        ));
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("PUT {url} -> {status}: {text}"));
    }
    let ok: PutOk = resp.json().await.context("decoding 200 body")?;
    Ok(ok)
}

/// `GET /api/v1/stacks/<id>/compose`. 204 means the stack has no
/// compose yet (legacy or freshly created); `Ok(None)` lets the caller
/// drive an empty-base diff.
async fn fetch_compose(session: &Session, stack_id: &str) -> Result<Option<ComposeResponse>> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks/{stack_id}/compose");
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

/// `POST /api/v1/stacks/<id>/diff` with `proposed` as the body.
/// Returns the controller's per-service reconcile plan.
async fn preview_diff(session: &Session, stack_id: &str, proposed: &str) -> Result<ReconcilePlan> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks/{stack_id}/diff");
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

/// `PUT /api/v1/stacks/<id>/compose` with optimistic concurrency.
/// 409 surfaces the controller's current sha so the caller can show
/// the operator a `--force` retry hint.
async fn put_compose(
    session: &Session,
    stack_id: &str,
    body: &str,
    expected_sha256: &str,
    force: bool,
) -> Result<PutOk> {
    let controller_url = session.require_controller()?;
    let mut url = format!("{controller_url}/api/v1/stacks/{stack_id}/compose");
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

/// Print a compact unified diff: skip equal lines, prefix
/// inserts with `+` and deletes with `-`. Same shape as
/// the sibling helper in `manifest_cmd`, duplicated here to
/// avoid leaking compose internals through the manifest module.
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
            // Skip context lines for compactness; v0.3d's `isd stack diff`
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

/// Print the reconcile plan to stdout with one line per op.
/// `+` for Start, `!` for Recreate, `-` for Stop, `~` for NoChange.
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

/// Y/N prompt. Refuses to prompt when stdin isn't a TTY: the operator
/// should pass `--yes` instead of piping `y`.
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

    fn args_with_path(path: Option<PathBuf>) -> DeployArgs {
        DeployArgs {
            path,
            stack: None,
            host_id: None,
            yes: true,
            force: false,
            all: false,
            root: None,
            overlay: None,
            strategy: None,
            fail_fast: false,
            diff: false,
            detach: true,
        }
    }

    #[test]
    fn resolve_plan_with_all_flag_returns_all_at_cwd_by_default() {
        let args = DeployArgs {
            all: true,
            ..args_with_path(None)
        };
        let plan = resolve_deploy_plan(&args).unwrap();
        assert!(matches!(plan, DeployPlan::All { .. }));
    }

    #[test]
    fn resolve_plan_with_compose_file_returns_single() {
        let tmp = tempfile::tempdir().unwrap();
        let compose = tmp.path().join("compose.yaml");
        std::fs::write(&compose, "services:\n").unwrap();
        let args = args_with_path(Some(compose.clone()));
        let plan = resolve_deploy_plan(&args).unwrap();
        match plan {
            DeployPlan::Single { compose_path } => assert_eq!(compose_path, compose),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn resolve_plan_with_stack_toml_returns_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_dir = tmp.path().join("servarr");
        std::fs::create_dir_all(&stack_dir).unwrap();
        let manifest = stack_dir.join("stack.toml");
        std::fs::write(&manifest, "name = \"servarr\"\ncompose = [\"c.toml\"]\n").unwrap();
        // Passed as directory:
        let plan = resolve_deploy_plan(&args_with_path(Some(stack_dir.clone()))).unwrap();
        assert!(matches!(plan, DeployPlan::Manifest { .. }));
        // Passed as the manifest path itself:
        let plan2 = resolve_deploy_plan(&args_with_path(Some(manifest))).unwrap();
        assert!(matches!(plan2, DeployPlan::Manifest { .. }));
    }

    #[test]
    fn resolve_plan_with_directory_lacking_stack_toml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("nothing");
        std::fs::create_dir_all(&empty).unwrap();
        let err = resolve_deploy_plan(&args_with_path(Some(empty)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("stack.toml"), "got: {err}");
    }

    /// Wave 5.A: bare name that matches no on-disk file or directory
    /// must error with a clear hint, not silently drop into the legacy
    /// Single compose path.
    #[test]
    fn resolve_plan_with_bare_name_no_match_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Use cwd inside tmp so the name resolves relative to a known-
        // empty directory. We don't actually chdir here: we construct
        // the path with an obviously-not-present bare basename and let
        // PathBuf::from("nope").is_dir() report false from cwd.
        let _guard = tmp;
        let nope = PathBuf::from("isd-deploy-bare-name-test-nope-XYZ123");
        let err = resolve_deploy_plan(&args_with_path(Some(nope)))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("did you mean") || err.contains("does not exist"),
            "got: {err}"
        );
    }

    /// Wave 5.A: an explicit path (with `./` prefix) that doesn't
    /// exist gets the "path does not exist" diagnostic, not the
    /// "did you mean ./<name>" hint.
    #[test]
    fn resolve_plan_with_explicit_path_no_match_errors() {
        let nope = PathBuf::from("./isd-deploy-explicit-test-nope-XYZ123");
        let err = resolve_deploy_plan(&args_with_path(Some(nope)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "got: {err}");
        // The bare-name hint should NOT appear when the operator
        // already used `./`.
        assert!(!err.contains("did you mean"), "got: {err}");
    }

    /// Wave 5.A: a bare name that matches a subdir with stack.toml
    /// resolves to Manifest (the operator-facing path-first case).
    #[test]
    fn resolve_plan_with_bare_name_matching_subdir_returns_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_dir = tmp.path().join("hello");
        std::fs::create_dir_all(&stack_dir).unwrap();
        std::fs::write(
            stack_dir.join("stack.toml"),
            "name = \"hello\"\ncompose = [\"compose.yaml\"]\n",
        )
        .unwrap();
        // Pass the directory path (not a bare-name resolved from cwd,
        // since cwd shouldn't be mutated in unit tests). The dir-probe
        // path is what `isd stack up hello` exercises when `hello` is in
        // cwd: PathBuf::from("hello").is_dir() takes the same branch.
        let plan = resolve_deploy_plan(&args_with_path(Some(stack_dir))).unwrap();
        assert!(matches!(plan, DeployPlan::Manifest { .. }));
    }

    /// Wave 5.A: a directory with a compose file but no stack.toml
    /// resolves to Single (legacy compose path) via the new probe
    /// order (stack.toml -> compose.toml -> compose.yml -> compose.yaml).
    #[test]
    fn resolve_plan_with_directory_containing_compose_yaml_returns_single() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_dir = tmp.path().join("legacy");
        std::fs::create_dir_all(&stack_dir).unwrap();
        let compose = stack_dir.join("compose.yaml");
        std::fs::write(&compose, "services:\n  web:\n    image: nginx\n").unwrap();
        let plan = resolve_deploy_plan(&args_with_path(Some(stack_dir))).unwrap();
        match plan {
            DeployPlan::Single { compose_path } => assert_eq!(compose_path, compose),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    /// Wave 5.A: a directory with both stack.toml and compose.yaml
    /// prefers stack.toml (precedence ordering documented in
    /// `resolve_positional_arg`).
    #[test]
    fn resolve_plan_dir_with_both_prefers_stack_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_dir = tmp.path().join("both");
        std::fs::create_dir_all(&stack_dir).unwrap();
        std::fs::write(
            stack_dir.join("stack.toml"),
            "name = \"both\"\ncompose = [\"compose.yaml\"]\n",
        )
        .unwrap();
        std::fs::write(stack_dir.join("compose.yaml"), "services:\n").unwrap();
        let plan = resolve_deploy_plan(&args_with_path(Some(stack_dir.clone()))).unwrap();
        match plan {
            DeployPlan::Manifest { manifest_path } => {
                assert_eq!(manifest_path, stack_dir.join("stack.toml"))
            }
            other => panic!("expected Manifest, got {other:?}"),
        }
    }

    /// Wave 5.A: `looks_like_explicit_path` classifier.
    #[test]
    fn looks_like_explicit_path_classifier() {
        assert!(looks_like_explicit_path(std::path::Path::new("./hello")));
        assert!(looks_like_explicit_path(std::path::Path::new("../hello")));
        assert!(looks_like_explicit_path(std::path::Path::new("/abs/hello")));
        assert!(looks_like_explicit_path(std::path::Path::new(
            "hello/stack.toml"
        )));
        assert!(!looks_like_explicit_path(std::path::Path::new("hello")));
        assert!(!looks_like_explicit_path(std::path::Path::new(
            "stack-name"
        )));
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
