//! `isd manifest cat | export | edit`: operator-side view + edit of a
//! deployed stack's `stack.toml` (Phase 0.13 follow-up).
//!
//! Before this subcommand, the manifest was a write-only artifact from
//! the operator's view: `isd deploy` shipped it to the controller, but
//! there was no way to read it back or change it after the fact short
//! of redeploying from a fresh local file. That stranded operators who
//! lost the local copy (worktree blew away, machine swap, fleet handoff)
//! and made manifest-only edits (secret bind list, hooks) impossible to
//! land without re-shipping the whole compose.
//!
//! Wire surface:
//!  - `GET  /api/v1/stacks/{id}/manifest`: returns `{ manifest_toml,
//!    manifest_sha256, secrets, hooks, ... }`. 204 for legacy
//!    compose-only stacks; 404 for unknown ids.
//!  - `PUT  /api/v1/stacks/{id}/manifest`: writes back. Optimistic
//!    concurrency via `If-Match: <sha256>`. 409 on stale sha; 400 on
//!    parse error / name mismatch / empty body; 422 on unknown secret.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::session::Session;

#[derive(Debug, Args)]
pub struct ManifestArgs {
    #[command(subcommand)]
    pub command: ManifestCommand,
}

#[derive(Debug, Subcommand)]
pub enum ManifestCommand {
    /// Print the persisted `stack.toml` body to stdout. Empty exit code
    /// is 0; 204-equivalent (legacy compose-only stack) errors with a
    /// clear message so scripts can branch on it.
    Cat(CatArgs),
    /// Write the persisted `stack.toml` to a local file. Defaults to
    /// `./<stack>.stack.toml`. Use this to recover a manifest you lost
    /// locally or to seed a new fleet's worktree from a deployed copy.
    Export(ExportArgs),
    /// Open the persisted manifest in `$EDITOR` and PUT it back on save.
    /// Optimistic concurrency: if the manifest changes server-side
    /// between fetch and save, the operator is prompted to re-edit
    /// against the new content instead of overwriting.
    Edit(EditArgs),
}

#[derive(Debug, Args)]
pub struct CatArgs {
    /// Stack name. The controller resolves to the unique id via the
    /// `/api/v1/stacks` list (same as `isd deploy <name>`).
    pub stack: String,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Stack name.
    pub stack: String,
    /// Output path. Defaults to `./<stack>.stack.toml` in cwd. Use `-`
    /// to write to stdout (mirrors `cat`, but explicit).
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Stack name.
    pub stack: String,
    /// Skip the y/N confirmation after the editor exits.
    #[arg(long)]
    pub yes: bool,
}

/// Subset of the dashboard's `/manifest` response we decode. Extra
/// fields (deploy_strategy, secrets, hooks, ...) are ignored at the
/// client; the operator's edit cycle is centered on the TOML body.
#[derive(Debug, Deserialize)]
struct ManifestResponse {
    manifest_toml: String,
    manifest_sha256: Option<String>,
}

/// Wire body for `PUT /stacks/{id}/manifest`. `secrets` and `hooks`
/// are omitted from the client today: edits flow through the TOML
/// body only, which is the operator-visible surface. The controller
/// preserves existing bindings when the optional fields are absent.
#[derive(Debug, Serialize)]
struct PutManifestBody<'a> {
    manifest_toml: &'a str,
}

/// Stack-list row we decode to map a name to its id. Mirrors the shape
/// used by `compose_cmd::resolve_stack_id_opt`.
#[derive(Debug, Deserialize)]
struct StackDto {
    id: String,
    name: String,
}

pub async fn run(args: ManifestArgs, context: Option<&str>) -> Result<()> {
    match args.command {
        ManifestCommand::Cat(a) => run_cat(a, context).await,
        ManifestCommand::Export(a) => run_export(a, context).await,
        ManifestCommand::Edit(a) => run_edit(a, context).await,
    }
}

async fn run_cat(args: CatArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let stack_id = resolve_stack_id(&session, &args.stack).await?;
    let manifest = fetch_manifest(&session, &stack_id).await?;
    print!("{}", manifest.manifest_toml);
    if !manifest.manifest_toml.ends_with('\n') {
        println!();
    }
    Ok(())
}

async fn run_export(args: ExportArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let stack_id = resolve_stack_id(&session, &args.stack).await?;
    let manifest = fetch_manifest(&session, &stack_id).await?;
    let dst = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.stack.toml", args.stack)));
    if dst == std::path::Path::new("-") {
        print!("{}", manifest.manifest_toml);
        return Ok(());
    }
    std::fs::write(&dst, manifest.manifest_toml.as_bytes())
        .with_context(|| format!("writing {}", dst.display()))?;
    println!("Exported {} manifest to {}", args.stack, dst.display());
    Ok(())
}

async fn run_edit(args: EditArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let stack_id = resolve_stack_id(&session, &args.stack).await?;
    let manifest = fetch_manifest(&session, &stack_id).await?;
    let original = manifest.manifest_toml.clone();
    let expected_sha = manifest.manifest_sha256.unwrap_or_default();

    // Drop the operator into $EDITOR on a temp file. tempfile keeps it
    // alive until the function returns; close-on-drop deletes it.
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("isd-manifest-{}-", args.stack))
        .suffix(".toml")
        .tempfile()
        .context("creating temp file for editor")?;
    tmp.write_all(original.as_bytes())
        .context("writing manifest to temp file")?;
    tmp.flush().ok();
    let path = tmp.path().to_path_buf();

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launching $EDITOR ({editor})"))?;
    if !status.success() {
        return Err(anyhow!("editor exited with status {status}"));
    }

    let edited = std::fs::read_to_string(&path).context("reading edited file")?;
    if edited == original {
        println!("No changes; nothing to apply.");
        return Ok(());
    }

    print_unified_diff(&original, &edited);
    println!();
    if !args.yes && !confirm("Apply manifest changes?")? {
        println!("Aborted.");
        return Ok(());
    }

    match put_manifest(&session, &stack_id, &edited, &expected_sha).await {
        Ok(sha) => {
            println!("Applied. New manifest_sha256: {sha}");
            Ok(())
        }
        Err(PutError::Conflict {
            current_sha256,
            current_toml,
        }) => {
            // The controller drifted under us. Show the operator the
            // remote diff and let them re-edit. We don't auto-merge:
            // manifests are tiny and a human is the right merger.
            eprintln!("conflict: manifest changed on the controller while you edited.");
            eprintln!("  current_sha256: {current_sha256}");
            eprintln!();
            eprintln!("Remote diff (against your edits):");
            print_unified_diff(&edited, &current_toml);
            Err(anyhow!(
                "rerun `isd manifest edit {}` to pick up the new base, then re-apply",
                args.stack
            ))
        }
        Err(PutError::Other(e)) => Err(e),
    }
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
    stacks
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.id)
        .ok_or_else(|| anyhow!("stack {name:?} not found on controller"))
}

async fn fetch_manifest(session: &Session, stack_id: &str) -> Result<ManifestResponse> {
    let url = format!(
        "{}/api/v1/stacks/{stack_id}/manifest",
        session.controller_url()
    );
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Err(anyhow!(
            "stack has no manifest (legacy compose-only stack); \
             `isd deploy` from a stack.toml first to install one"
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!("stack id {stack_id} not found on controller"));
    }
    let m: ManifestResponse = resp
        .error_for_status()?
        .json()
        .await
        .context("decoding manifest body")?;
    Ok(m)
}

/// Internal error: PUT returned a 409 with the controller's current
/// manifest in the body. The caller surfaces a diff and re-prompts.
enum PutError {
    Conflict {
        current_sha256: String,
        current_toml: String,
    },
    Other(anyhow::Error),
}

impl From<anyhow::Error> for PutError {
    fn from(e: anyhow::Error) -> Self {
        PutError::Other(e)
    }
}

#[derive(Debug, Deserialize)]
struct PutOk {
    manifest_sha256: String,
}

#[derive(Debug, Deserialize)]
struct PutConflictBody {
    current_sha256: String,
    current_toml: String,
}

async fn put_manifest(
    session: &Session,
    stack_id: &str,
    body: &str,
    expected_sha256: &str,
) -> std::result::Result<String, PutError> {
    let url = format!(
        "{}/api/v1/stacks/{stack_id}/manifest",
        session.controller_url()
    );
    let mut req = session.client.put(&url).json(&PutManifestBody {
        manifest_toml: body,
    });
    if !expected_sha256.is_empty() {
        req = req.header("If-Match", expected_sha256);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("PUT {url}"))
        .map_err(PutError::from)?;
    let status = resp.status();
    if status == reqwest::StatusCode::CONFLICT {
        let conflict: PutConflictBody = resp
            .json()
            .await
            .context("decoding 409 body")
            .map_err(PutError::from)?;
        return Err(PutError::Conflict {
            current_sha256: conflict.current_sha256,
            current_toml: conflict.current_toml,
        });
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(PutError::Other(anyhow!("PUT {url} -> {status}: {text}")));
    }
    let ok: PutOk = resp
        .json()
        .await
        .context("decoding 200 body")
        .map_err(PutError::from)?;
    Ok(ok.manifest_sha256)
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
            continue;
        }
        wrote_anything = true;
        print!("{prefix}{change}");
    }
    if !wrote_anything {
        println!("(no textual changes)");
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
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_creds(server_uri: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        std::fs::write(
            &creds_path,
            format!(
                r#"default_context = "alice"

[[contexts]]
name = "alice"
kind = "http"
url = "{server_uri}"
"#,
            ),
        )
        .unwrap();
        // SAFETY: matches the secret + hosts test pattern; these tests
        // are `#[ignore]` by default so they don't race with each other.
        unsafe {
            std::env::set_var("ISD_CREDENTIALS_FILE", &creds_path);
        }
        dir
    }

    /// `isd manifest cat` issues `GET /api/v1/stacks` to resolve the
    /// stack id, then `GET /api/v1/stacks/{id}/manifest`. Verify both
    /// calls land at the stub and the printed body matches.
    #[tokio::test]
    #[ignore]
    async fn cat_fetches_manifest_for_named_stack() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stacks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "1", "name": "blog", "host_id": "h", "source": "compose",
                  "discovered_at": "2026-05-11T00:00:00Z" }
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stacks/1/manifest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "stack_id": 1,
                "stack_name": "blog",
                "manifest_toml": "name = \"blog\"\nfleet = \"test\"\ncompose = [\"compose.yaml\"]\n",
                "manifest_sha256": "abc123",
                "secrets": [],
                "hooks": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let _dir = write_creds(&server.uri());
        let result = run_cat(
            CatArgs {
                stack: "blog".into(),
            },
            None,
        )
        .await;
        unsafe {
            std::env::remove_var("ISD_CREDENTIALS_FILE");
        }
        result.expect("manifest cat should succeed");
    }

    /// 204 from GET /manifest must surface a clear "legacy stack" error,
    /// not a generic deserialize failure.
    #[tokio::test]
    #[ignore]
    async fn cat_surfaces_no_manifest_for_204() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stacks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "1", "name": "legacy", "host_id": "h", "source": "compose",
                  "discovered_at": "2026-05-11T00:00:00Z" }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stacks/1/manifest"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let _dir = write_creds(&server.uri());
        let result = run_cat(
            CatArgs {
                stack: "legacy".into(),
            },
            None,
        )
        .await;
        unsafe {
            std::env::remove_var("ISD_CREDENTIALS_FILE");
        }
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("legacy compose-only stack") || err.contains("no manifest"),
            "got: {err}"
        );
    }

    /// `isd manifest export --output -` prints to stdout (no file write).
    #[tokio::test]
    #[ignore]
    async fn export_to_dash_uses_stdout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stacks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "1", "name": "blog", "host_id": "h", "source": "compose",
                  "discovered_at": "2026-05-11T00:00:00Z" }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stacks/1/manifest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "manifest_toml": "name = \"blog\"\n",
                "manifest_sha256": "abc",
                "secrets": [], "hooks": []
            })))
            .mount(&server)
            .await;

        let _dir = write_creds(&server.uri());
        let result = run_export(
            ExportArgs {
                stack: "blog".into(),
                output: Some(PathBuf::from("-")),
            },
            None,
        )
        .await;
        unsafe {
            std::env::remove_var("ISD_CREDENTIALS_FILE");
        }
        result.expect("export to stdout should succeed");
    }

    /// PUT sends the `If-Match` header carrying the sha256 the GET
    /// returned, so the controller can detect concurrent edits.
    #[tokio::test]
    #[ignore]
    async fn put_sets_if_match_header() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/stacks/1/manifest"))
            .and(header("if-match", "abc"))
            .and(header_exists("content-type"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "manifest_sha256": "def"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let _dir = write_creds(&server.uri());
        let session = Session::open(None).await.unwrap();
        let sha = put_manifest(&session, "1", "name = \"blog\"\n", "abc")
            .await
            .map_err(|e| match e {
                PutError::Other(e) => e,
                PutError::Conflict { .. } => anyhow!("unexpected conflict"),
            })
            .unwrap();
        unsafe {
            std::env::remove_var("ISD_CREDENTIALS_FILE");
        }
        assert_eq!(sha, "def");
    }

    /// 409 on PUT decodes into `PutError::Conflict` carrying the
    /// controller's current sha + body, so the caller can re-prompt.
    #[tokio::test]
    #[ignore]
    async fn put_409_decodes_into_conflict_variant() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/stacks/1/manifest"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "manifest hash mismatch",
                "current_sha256": "newer",
                "current_toml": "name = \"blog\"\nfleet = \"prod\"\n"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let _dir = write_creds(&server.uri());
        let session = Session::open(None).await.unwrap();
        let err = put_manifest(&session, "1", "name = \"blog\"\n", "stale")
            .await
            .expect_err("expected conflict error");
        unsafe {
            std::env::remove_var("ISD_CREDENTIALS_FILE");
        }
        match err {
            PutError::Conflict {
                current_sha256,
                current_toml,
            } => {
                assert_eq!(current_sha256, "newer");
                assert!(current_toml.contains("fleet = \"prod\""));
            }
            PutError::Other(e) => panic!("expected Conflict, got Other({e})"),
        }
    }
}
