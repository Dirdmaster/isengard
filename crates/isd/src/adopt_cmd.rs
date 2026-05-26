//! `isd stack adopt`: re-synthesize a stack's compose from the live
//! container state and accept it as operator-owned truth.
//!
//! The first-adoption path is automatic (the controller's
//! `compose_autoadopt` debouncer fires synthesis on a stable
//! heartbeat). This verb is the *re*-adoption path: the operator
//! ran `docker exec`, edited a container in place, or otherwise
//! drifted from the stored compose, and wants the live state to
//! become the new stored truth.
//!
//! v0.1 ships only `--refresh`. The spec describes a `--release`
//! mode that drops stored compose entirely (returning the stack to
//! discovery-only); that ships in v0.2 when a second operator asks.
//!
//! Commands for adopting live containers into Isengard-managed compose.
//! "When `isd stack adopt` is still a verb".
//!
//! ## Flow with `--refresh`
//!
//! ```text
//!  1. Look up the stack id by name from `GET /api/v1/stacks`.
//!  2. GET /api/v1/stacks/<id>/compose?source=synthesized
//!     -> fresh live synthesis (no DB write happens server-side).
//!  3. Render a diff against the stored compose so the operator
//!     sees what will land. (If there's no stored compose, the
//!     "before" side is empty.)
//!  4. Prompt y/N unless --yes was passed.
//!  5. PUT /api/v1/stacks/<id>/compose with the synthesized body.
//!     The server-side handler writes compose_source = operator_written;
//!     no special header needed.
//! ```

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde::Deserialize;

use crate::session::Session;

/// CLI flags for `isd stack adopt`.
///
/// `--refresh` is required in v0.1; running `isd stack adopt <name>`
/// without it returns a clear "v0.1 only supports --refresh" error
/// pointing at the deferred v0.2 `--release` mode.
#[derive(Debug, Args)]
pub struct AdoptArgs {
    /// Stack name on the controller. Must already exist in the
    /// inventory (the controller's adoption is auto for first sight;
    /// this verb is the re-adoption path).
    pub name: String,
    /// Re-synthesize from current containers and overwrite the stored
    /// compose with the result, tagged as operator-written. Required
    /// in v0.1.
    #[arg(long)]
    pub refresh: bool,
    /// Skip the y/N confirmation prompt. The diff still prints.
    #[arg(long)]
    pub yes: bool,
}

/// Dispatch `isd stack adopt`. Errors when `--refresh` is absent
/// (the only mode shipped in v0.1).
///
/// # Errors
///
/// Returns `Err` when:
/// - `--refresh` was not passed (v0.1 only supports `--refresh`),
/// - the stack does not exist on the controller,
/// - the live-synth endpoint returned a non-2xx,
/// - the operator answered "no" at the prompt,
/// - the PUT to apply the synthesized compose failed.
pub async fn run(args: AdoptArgs, context: Option<&str>) -> Result<()> {
    if !args.refresh {
        bail!(
            "v0.1 of `isd stack adopt` only supports `--refresh`. \
             v0.2 will add `--release` for dropping stored compose."
        );
    }
    let session = Session::open(context).await?;
    run_refresh(&session, &args.name, args.yes).await
}

/// `--refresh` path. Public so tests can exercise the flow without
/// going through clap; the spec test plan exercises this directly.
pub async fn run_refresh(session: &Session, stack_name: &str, yes: bool) -> Result<()> {
    let stack_id = resolve_stack_id(session, stack_name).await?;
    let stored = fetch_stored_compose(session, &stack_id).await?;
    let synth = fetch_live_synth(session, &stack_id).await?;
    let new_yaml = synth.compose_yaml;

    print_diff_block(stack_name, stored.as_deref(), &new_yaml);

    if !yes && !prompt_confirm(stack_name)? {
        println!("isd: aborted, nothing written.");
        return Ok(());
    }

    put_compose_yaml(session, &stack_id, &new_yaml).await?;
    println!(
        "isd: stack {:?} re-adopted: stored compose flipped to operator_written.",
        stack_name
    );
    Ok(())
}

/// Look up a stack id by name. Returns a clear error when not found
/// so the operator gets "stack not found" instead of a 404 dump.
async fn resolve_stack_id(session: &Session, name: &str) -> Result<String> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks");
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

/// GET the stored compose. Returns `Ok(None)` on 204 (the stack
/// exists but has no compose row yet); errors on 4xx/5xx.
async fn fetch_stored_compose(session: &Session, stack_id: &str) -> Result<Option<String>> {
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
    Ok(Some(cr.compose_yaml))
}

/// GET the live-synth: `?source=synthesized` returns a freshly-built
/// YAML that ignores the stored row. Used by `--refresh` to compute
/// what to write.
///
/// 503 here means the server-side synthesizer hasn't been wired in
/// this build (the parallel
/// `feat-controller-compose-synthesizer` PR is the carrier). Surface
/// it as a clear error so the operator knows to wait for the followup.
async fn fetch_live_synth(session: &Session, stack_id: &str) -> Result<ComposeResponse> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks/{stack_id}/compose?source=synthesized");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "live synthesis not wired yet on the controller (HTTP 503). \
             Detail: {body}\n\
             This usually means the controller is running a build that pre-dates \
             the compose-synthesizer feature. Retry once the controller is upgraded."
        );
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("GET {url} -> {status}: {body}");
    }
    let cr: ComposeResponse = resp.json().await.context("decoding compose response")?;
    Ok(cr)
}

/// PUT the synthesized YAML back so the row flips to
/// `operator_written`. Uses the `application/yaml` body shape; no
/// `If-Match` header because `--refresh` is explicitly destructive
/// (the operator just asked to overwrite).
async fn put_compose_yaml(session: &Session, stack_id: &str, yaml: &str) -> Result<()> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks/{stack_id}/compose?force=true");
    let resp = session
        .client
        .put(&url)
        .header("Content-Type", "application/yaml")
        .body(yaml.to_string())
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("PUT {url} -> {status}: {body}");
    }
    Ok(())
}

/// Print a side-by-side-ish summary: header + the new YAML, with a
/// note when the stored compose is missing (first-time adoption that
/// somehow missed the auto-adopt path) or non-empty (the actual
/// re-adoption case).
fn print_diff_block(stack_name: &str, stored: Option<&str>, synthesized: &str) {
    println!("isd: re-adoption preview for stack {:?}:", stack_name);
    match stored {
        Some(s) if s.trim().is_empty() => {
            println!("  stored compose: (empty)");
        }
        Some(_) => {
            println!("  stored compose: present");
        }
        None => {
            println!("  stored compose: none yet");
        }
    }
    println!(
        "  synthesized:    {} byte(s), {} line(s)",
        synthesized.len(),
        synthesized.lines().count(),
    );
    println!();
    println!("---- synthesized compose ----");
    println!("{synthesized}");
    println!("---- end ----");
}

/// Prompt the operator. Stdin must be a TTY; non-TTY callers must
/// pass `--yes`. We do the TTY check explicitly so a missing
/// `--yes` in CI surfaces as a clear error rather than an EOF read
/// that silently aborts.
fn prompt_confirm(stack_name: &str) -> Result<bool> {
    let mut stderr = std::io::stderr();
    if !std::io::stdin().is_terminal() {
        bail!(
            "isd: stdin is not a TTY; pass --yes to confirm re-adoption of {:?} non-interactively",
            stack_name
        );
    }
    write!(
        stderr,
        "Overwrite stored compose for {stack_name:?}? [y/N] "
    )?;
    stderr.flush()?;
    let mut line = String::new();
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let n = handle.read_line(&mut line)?;
    if n == 0 {
        return Ok(false);
    }
    let trimmed = line.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

/// Minimal mirror of the dashboard's stacks list entry.
#[derive(Debug, Deserialize)]
struct StackDto {
    /// Stringified surrogate key.
    id: String,
    /// Operator-facing stack name.
    name: String,
}

/// Mirror of the dashboard's `GET /stacks/<id>/compose` body.
/// Keeps only the fields the adopt verb cares about; new fields on
/// the response (e.g. `compose_source`) are ignored at deserialize
/// time so the client tolerates server-side additions.
#[derive(Debug, Deserialize)]
struct ComposeResponse {
    /// The stack's compose YAML, verbatim.
    compose_yaml: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// Minimal clap parent so we can exercise `AdoptArgs` parsing
    /// without booting the full `isd` CLI.
    #[derive(Debug, Parser)]
    struct TestRoot {
        #[command(flatten)]
        adopt: AdoptArgs,
    }

    #[test]
    fn adopt_requires_a_name() {
        let err = TestRoot::try_parse_from(["isd-test"]).unwrap_err();
        assert!(
            err.kind() == clap::error::ErrorKind::MissingRequiredArgument,
            "expected missing name, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn adopt_parses_with_refresh_and_yes() {
        let root =
            TestRoot::try_parse_from(["isd-test", "servarr", "--refresh", "--yes"]).expect("parse");
        assert_eq!(root.adopt.name, "servarr");
        assert!(root.adopt.refresh);
        assert!(root.adopt.yes);
    }

    #[test]
    fn adopt_parses_with_refresh_only() {
        let root = TestRoot::try_parse_from(["isd-test", "plex", "--refresh"]).expect("parse");
        assert_eq!(root.adopt.name, "plex");
        assert!(root.adopt.refresh);
        assert!(!root.adopt.yes);
    }

    #[test]
    fn adopt_without_refresh_parses_but_run_rejects() {
        // Clap doesn't require --refresh at parse time (it's a bool
        // flag, not a required positional); the runtime check in
        // `run` catches the bare-adopt path with a v0.2-pointing
        // message.
        let root = TestRoot::try_parse_from(["isd-test", "plex"]).expect("parse");
        assert_eq!(root.adopt.name, "plex");
        assert!(!root.adopt.refresh);

        // The async `run` is the gate. Exercise its v0.1 guard by
        // calling it under a tokio runtime and asserting the error
        // mentions `--refresh`.
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                super::run(
                    AdoptArgs {
                        name: "plex".into(),
                        refresh: false,
                        yes: false,
                    },
                    None,
                )
                .await
            })
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--refresh"),
            "expected --refresh in error, got {msg:?}"
        );
        assert!(
            msg.contains("v0.2"),
            "expected v0.2 pointer in error, got {msg:?}"
        );
    }

    #[test]
    fn print_diff_block_renders_header_and_body() {
        // Smoke: the diff block writes to stdout. We don't capture it
        // here; the assertion is that it returns without panicking.
        // (A full capture would mean wiring an alternate writer.)
        print_diff_block(
            "servarr",
            Some("services:\n  web:\n    image: nginx:1.0\n"),
            "services:\n  web:\n    image: nginx:1.1\n",
        );
    }

    /// Sanity: clap derives the help text under the args. The
    /// expected names show up so a `cargo doc` rebuild of the help
    /// text catches accidental renames.
    #[test]
    fn help_text_lists_name_and_refresh_and_yes() {
        let mut cmd = TestRoot::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("name"), "help: {help}");
        assert!(help.contains("--refresh"), "help: {help}");
        assert!(help.contains("--yes"), "help: {help}");
    }
}
