//! `isd stack doctor`: audit a compose file for Isengard idioms.
//!
//! Two surfaces share one audit pass:
//!
//! - `isd stack deploy` runs the audit pre-flight (see
//!   `compose_cmd::run_deploy`) and prints findings as non-blocking
//!   warnings above the reconcile-plan render. `--no-doctor` skips
//!   the audit for the operator who knows what they want.
//! - `isd stack doctor <path>` runs the same audit and prints the
//!   findings on their own. v0.1 stops there; v0.2 will walk the
//!   findings interactively and offer fixes the operator can opt in
//!   to per-check.
//!
//! Each check is a pure function `(compose: &Value) -> Vec<Finding>`.
//! `audit` is the union of every registered check.
//!
//! See the design spec:
//! `3 Resources/Superpowers/specs/2026-05-23-isd-stack-doctor-design.md`.

use anyhow::{Context as _, Result};
use clap::Args;
use serde_yaml::Value;
use std::path::{Path, PathBuf};

use crate::session::Session;

/// Compose apply targets for doctor fixes.
pub mod apply;
/// Registered read-only doctor checks.
#[allow(clippy::missing_docs_in_private_items)]
pub mod checks;
/// Compose document parsing and round-trip rendering.
#[allow(clippy::missing_docs_in_private_items)]
pub mod document;
/// Registered doctor fixers.
pub mod fixers;

/// Severity of a [`Finding`]. v0.1 only emits `Warning`s; `Error`
/// is reserved for future checks that would make the deploy fail
/// (e.g. a `secrets: file:` that the agent will reject anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Surface to the operator; deploy continues.
    Warning,
}

/// Machine-readable target for a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingTarget {
    /// A compose service by name.
    Service {
        /// Service name from the compose document.
        name: String,
    },
}

/// Machine-readable fix data attached by a check for a registered fixer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixSpec {
    /// Add `isengard.expose` metadata to a service.
    ExposeService {
        /// Service that should receive expose metadata.
        service: String,
        /// Port inferred from compose metadata, when unambiguous.
        inferred_port: Option<u16>,
    },
}

/// A single audit finding. Carries an id for filtering / muting, a
/// severity, the human-readable message, an optional hint line the
/// doctor verb can print as guidance, and optional structured metadata.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Stable id (e.g. `EXPOSE_HOST_MISSING`) for muting and tests.
    pub id: &'static str,
    /// Severity bucket.
    pub severity: Severity,
    /// One-line summary the operator sees first.
    pub message: String,
    /// Optional follow-up line with the suggested fix. The doctor
    /// verb prints this; the deploy pre-flight prints it too so the
    /// operator knows what `isd stack doctor` would offer.
    pub hint: Option<String>,
    /// Optional structured target for fixer dispatch.
    #[allow(dead_code)]
    pub target: Option<FindingTarget>,
    /// Optional structured fix data for fixer dispatch.
    #[allow(dead_code)]
    pub fix: Option<FixSpec>,
}

/// Walk every registered check against the parsed compose YAML and
/// collect their findings. Pure; no IO.
pub fn audit(compose: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(checks::expose_host::check(compose));
    out
}

/// Parse a compose file at `path` into a `serde_yaml::Value` ready for
/// [`audit`]. Surfaces a friendly error when the file is missing or the
/// document doesn't parse.
///
/// # Errors
///
/// Returns `Err` when the file can't be read or the document isn't valid.
pub fn load_compose(path: &Path) -> Result<Value> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading compose file {}", path.display()))?;
    let document = document::ComposeDocument::parse_path(path, &content)?;
    Ok(document.value)
}

/// Print a slice of findings as `warning:` lines on stderr. The deploy
/// pre-flight uses this; the doctor verb uses a slightly richer format
/// (see [`run`]).
pub fn print_warnings(findings: &[Finding]) {
    for f in findings {
        let tag = match f.severity {
            Severity::Warning => "warning",
        };
        eprintln!("{tag}: {} [{}]", f.message, f.id);
        if let Some(hint) = &f.hint {
            eprintln!("  hint: {hint}");
        }
    }
}

/// CLI flags for `isd stack doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Stack name (fetched from the controller) or a local path to a
    /// compose file or directory.
    ///
    /// Resolution order: if the value exists on disk it is treated as
    /// a local path; otherwise it is treated as a stack name and the
    /// controller's stored compose is fetched. Omitted defaults to
    /// `./compose.yaml`.
    pub target: Option<String>,
}

/// Where the audited compose came from. Used to render a meaningful
/// header above the findings.
enum ComposeSource {
    /// Read from a path on disk.
    Local(PathBuf),
    /// Fetched from the controller for a named stack.
    Controller(String),
}

/// Fully loaded compose input for a doctor run.
struct ResolvedCompose {
    /// Original document body, retained for future fixer round trips.
    #[allow(dead_code)]
    body: String,
    /// Parsed compose document normalized for read-only checks.
    document: document::ComposeDocument,
    /// Source label used in CLI output.
    source: ComposeSource,
}

impl ComposeSource {
    /// Operator-facing label for the source: a filesystem path or a
    /// `stack "<name>" (controller)` tag.
    fn label(&self) -> String {
        match self {
            Self::Local(p) => p.display().to_string(),
            Self::Controller(name) => format!("stack {name:?} (controller)"),
        }
    }
}

/// `isd stack doctor`: run the audit and print every finding. v0.1
/// is read-only. The interactive fix loop lands in v0.2.
///
/// Accepts a local path OR a stack name. Local wins when the value
/// exists on disk; otherwise the controller is queried for the named
/// stack's stored compose.
///
/// # Errors
///
/// Returns `Err` when the compose file / stack can't be located or
/// when the document fails to parse.
pub async fn run(args: DoctorArgs, context: Option<&str>) -> Result<()> {
    let resolved = resolve_compose(args.target.as_deref(), context).await?;
    let findings = audit(&resolved.document.value);

    if findings.is_empty() {
        println!("{} looks healthy. No findings.", resolved.source.label());
        return Ok(());
    }

    println!("{}:", resolved.source.label());
    for f in &findings {
        let tag = match f.severity {
            Severity::Warning => "warning",
        };
        println!("  {tag}: {} [{}]", f.message, f.id);
        if let Some(hint) = &f.hint {
            println!("    hint: {hint}");
        }
    }
    println!();
    println!(
        "{} finding(s). Re-run with `isd stack doctor --fix <target>` once v0.2 ships the interactive fixer.",
        findings.len()
    );
    Ok(())
}

/// Pick the compose document for the requested target.
///
/// 1. `None` → the first supported compose document in the cwd.
/// 2. A value that resolves to a file or directory on disk → load locally.
/// 3. Otherwise → treat as a stack name and fetch from the controller.
async fn resolve_compose(target: Option<&str>, context: Option<&str>) -> Result<ResolvedCompose> {
    if let Some(t) = target {
        let path = std::path::Path::new(t);
        if path.exists() {
            let mut resolved = resolve_local_path(path)?;
            if resolved.file_name().and_then(|s| s.to_str()) == Some("stack.toml") {
                resolved = apply::resolve_manifest_compose_path(&resolved)?;
            }
            let body = std::fs::read_to_string(&resolved)
                .with_context(|| format!("reading compose file {}", resolved.display()))?;
            let document = document::ComposeDocument::parse_path(&resolved, &body)?;
            return Ok(ResolvedCompose {
                body,
                document,
                source: ComposeSource::Local(resolved),
            });
        }
        // Not on disk; assume stack name.
        let session = Session::open(context).await?;
        match crate::compose_cmd::fetch_compose_yaml_by_name(&session, t).await? {
            Some(yaml) => {
                let document = document::ComposeDocument::parse_controller_yaml(&yaml)?;
                Ok(ResolvedCompose {
                    body: yaml,
                    document,
                    source: ComposeSource::Controller(t.to_string()),
                })
            }
            None => Err(anyhow::anyhow!(
                "stack {t:?} is tracked by the controller but has no compose stored.\n\
                 \n\
                 This usually means the stack was deployed outside isengard \
                 (e.g. via `docker compose up` directly). The agent discovers it \
                 from the `com.docker.compose.project={t}` label on the containers \
                 but never received the compose YAML to audit.\n\
                 \n\
                 Two ways forward:\n\
                 \n\
                 - Point doctor at the local compose file you used:\n\
                       isd stack doctor /path/to/your/compose.yaml\n\
                 \n\
                 - Or import the stack so future deploys + doctor runs see it:\n\
                       isd stack deploy /path/to/your/compose.yaml\n\
                 \n\
                 Auto-synthesizing compose from running containers is on the roadmap \
                 (needs the agent to ship ports/env/volumes); see\n\
                 `3 Resources/Superpowers/specs/2026-05-23-isd-compose-synthesize-design.md`."
            )),
        }
    } else {
        let mut resolved = resolve_local_path(Path::new("."))?;
        if resolved.file_name().and_then(|s| s.to_str()) == Some("stack.toml") {
            resolved = apply::resolve_manifest_compose_path(&resolved)?;
        }
        let body = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading compose file {}", resolved.display()))?;
        let document = document::ComposeDocument::parse_path(&resolved, &body)?;
        Ok(ResolvedCompose {
            body,
            document,
            source: ComposeSource::Local(resolved),
        })
    }
}

/// Resolve a local path or directory into a concrete compose file.
fn resolve_local_path(target: &Path) -> Result<PathBuf> {
    if target.is_dir() {
        for name in ["stack.toml", "compose.toml", "compose.yaml", "compose.yml"] {
            let candidate = target.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        return Err(anyhow::anyhow!(
            "no stack.toml, compose.toml, compose.yaml, or compose.yml in {}",
            target.display()
        ));
    }
    if target.exists() {
        return Ok(target.to_path_buf());
    }
    Err(anyhow::anyhow!(
        "compose path {} does not exist",
        target.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_on_idiomatic_compose_returns_no_findings() {
        let yaml = r#"
services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
    labels:
      isengard.expose: "demo.example.com"
"#;
        let compose: Value = serde_yaml::from_str(yaml).unwrap();
        assert!(audit(&compose).is_empty());
    }

    #[test]
    fn audit_surfaces_expose_host_finding_for_bare_http_service() {
        let yaml = r#"
services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
"#;
        let compose: Value = serde_yaml::from_str(yaml).unwrap();
        let findings = audit(&compose);
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.id == "EXPOSE_HOST_MISSING"));
    }

    #[test]
    fn local_directory_prefers_stack_then_toml_then_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("compose.yaml"), "services:\n").unwrap();
        std::fs::write(
            tmp.path().join("compose.toml"),
            "[services.web]\nimage = \"nginx\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("stack.toml"),
            "name = \"demo\"\ncompose = [\"compose.toml\"]\n",
        )
        .unwrap();
        let resolved = resolve_local_path(tmp.path()).unwrap();
        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some("stack.toml")
        );
    }

    #[tokio::test]
    async fn stack_toml_target_audits_referenced_compose() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("stack.toml"),
            "name = \"demo\"\ncompose = [\"compose.toml\"]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("compose.toml"),
            "[services.web]\nimage = \"nginx\"\nports = [\"8080:80\"]\n",
        )
        .unwrap();

        let resolved = resolve_compose(Some(tmp.path().to_str().unwrap()), None)
            .await
            .unwrap();
        let findings = audit(&resolved.document.value);
        assert!(findings.iter().any(|f| f.id == "EXPOSE_HOST_MISSING"));
    }

    #[test]
    fn stack_toml_with_zero_compose_files_tells_operator_to_pass_compose_file() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_toml = tmp.path().join("stack.toml");
        std::fs::write(&stack_toml, "name = \"demo\"\ncompose = []\n").unwrap();

        let err = apply::resolve_manifest_compose_path(&stack_toml).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("pass the specific compose file"),
            "{message}"
        );
        assert!(message.contains("doctor --fix"), "{message}");
    }

    #[test]
    fn stack_toml_with_multiple_compose_files_tells_operator_to_pass_compose_file() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_toml = tmp.path().join("stack.toml");
        std::fs::write(
            &stack_toml,
            "name = \"demo\"\ncompose = [\"compose.toml\", \"compose.override.toml\"]\n",
        )
        .unwrap();

        let err = apply::resolve_manifest_compose_path(&stack_toml).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("pass the specific compose file"),
            "{message}"
        );
        assert!(message.contains("doctor --fix"), "{message}");
    }
}
