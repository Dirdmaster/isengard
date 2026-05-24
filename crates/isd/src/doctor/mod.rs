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
    /// Add or replace `isengard.expose.port` for a service that already
    /// has a hostname label.
    SetExposePort {
        /// Service that should receive the port override.
        service: String,
        /// Candidate upstream container ports the operator may choose from.
        candidate_ports: Vec<u16>,
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
    /// Interactively apply available fixes.
    #[arg(long)]
    pub fix: bool,
    /// Skip final confirmation after printing the proposed diff.
    #[arg(long)]
    pub yes: bool,
    /// Force local file writes where supported by the apply target.
    #[arg(long)]
    pub force: bool,
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
    Controller {
        /// Operator-facing stack name.
        name: String,
        /// Controller stack id.
        id: String,
        /// SHA-256 fetched with the stored compose body.
        sha256: String,
    },
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
    /// Controller session used to fetch this compose document.
    session: Option<Session>,
}

impl ComposeSource {
    /// Operator-facing label for the source: a filesystem path or a
    /// `stack "<name>" (controller)` tag.
    fn label(&self) -> String {
        match self {
            Self::Local(p) => p.display().to_string(),
            Self::Controller { name, .. } => format!("stack {name:?} (controller)"),
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
    if args.fix {
        return run_fix(args, context).await;
    }

    let resolved = resolve_compose(args.target.as_deref(), context, false).await?;
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
        "{} finding(s). Re-run with `isd stack doctor --fix <target>` to repair fixable findings.",
        findings.len()
    );
    Ok(())
}

/// Run the interactive doctor fixer loop for supported findings.
async fn run_fix(args: DoctorArgs, context: Option<&str>) -> Result<()> {
    let mut resolved = resolve_compose(args.target.as_deref(), context, true).await?;
    let original = resolved.body.clone();
    let findings = audit(&resolved.document.value);
    if !findings.is_empty()
        && findings
            .iter()
            .all(|finding| fixers::fixer_id_for(finding).is_none())
    {
        println!(
            "{} finding(s), but none have registered fixers yet.",
            findings.len()
        );
        return Ok(());
    }

    let changed = apply_fix_inputs(
        &mut resolved.document,
        findings,
        prompt_should_fix,
        prompt_hostname,
        prompt_expose_port,
    )?;

    if !changed {
        println!("No fixes applied.");
        return Ok(());
    }

    let remaining = audit(&resolved.document.value);
    if remaining.iter().any(|f| f.id == "EXPOSE_HOST_MISSING") {
        eprintln!("warning: some expose findings remain after selected fixes");
    }

    let proposed = resolved.document.to_string()?;
    print_compact_diff(&original, &proposed);
    if !args.yes && !confirm_apply()? {
        println!("Aborted.");
        return Ok(());
    }

    let ResolvedCompose {
        source, session, ..
    } = resolved;
    match source {
        ComposeSource::Local(path) => {
            apply::ApplyTarget::LocalFile { path }
                .apply(&proposed, args.force)
                .await?;
            println!("Applied fixes locally.");
        }
        ComposeSource::Controller { name, id, sha256 } => {
            apply::ApplyTarget::ControllerStack {
                stack_id: id,
                stack_name: name.clone(),
                sha256,
            }
            .apply_with_session(session.as_ref(), &proposed, args.force)
            .await?;
            println!(
                "Applied fixes to stack {:?}. Run `isd stack doctor {}` to verify.",
                name, name
            );
        }
    }

    Ok(())
}

/// Apply supported fix inputs, collecting interactive values through a callback.
fn apply_fix_inputs(
    document: &mut document::ComposeDocument,
    findings: Vec<Finding>,
    mut should_fix: impl FnMut(&str) -> Result<bool>,
    mut hostname_for_service: impl FnMut(&str) -> Result<String>,
    mut expose_port_for_service: impl FnMut(&str, &[u16]) -> Result<u16>,
) -> Result<bool> {
    let mut changed = false;
    for finding in findings {
        match finding.fix.clone() {
            Some(FixSpec::ExposeService { .. }) => {
                if fixers::fixer_id_for(&finding) != Some("EXPOSE_HOST_MISSING") {
                    continue;
                }
                let Some(mut input) = fixers::expose_host::input_from_finding(&finding) else {
                    continue;
                };
                if !should_fix(&input.service)? {
                    continue;
                }
                input.hostname = hostname_for_service(&input.service)?;
                changed |= fixers::expose_host::apply_expose_host(&mut document.value, &input)?;
            }
            Some(FixSpec::SetExposePort {
                service,
                candidate_ports,
            }) => {
                if !should_fix(&service)? {
                    continue;
                }
                let port = match candidate_ports.as_slice() {
                    [] => anyhow::bail!(
                        "services.{service} has no candidate ports for isengard.expose.port"
                    ),
                    [port] => *port,
                    ports => expose_port_for_service(&service, ports)?,
                };
                changed |=
                    fixers::expose_host::apply_expose_port(&mut document.value, &service, port)?;
            }
            None => continue,
        }
    }
    Ok(changed)
}

/// Prompt whether to apply a fix for one service.
fn prompt_should_fix(service: &str) -> Result<bool> {
    inquire::Confirm::new(&format!("Fix services.{service}?"))
        .with_default(true)
        .prompt()
        .map_err(|e| anyhow::anyhow!("fix prompt cancelled: {e}"))
}

/// Prompt for the hostname used by the expose-host fixer.
fn prompt_hostname(service: &str) -> Result<String> {
    let hostname = inquire::Text::new(&format!(
        "Expose services.{service} through which hostname?"
    ))
    .with_validator(|input: &str| {
        if input.trim().is_empty() {
            Ok(inquire::validator::Validation::Invalid(
                "hostname cannot be empty".into(),
            ))
        } else {
            Ok(inquire::validator::Validation::Valid)
        }
    })
    .prompt()
    .map_err(|e| anyhow::anyhow!("hostname prompt cancelled: {e}"))?;

    Ok(hostname.trim().to_string())
}

/// Prompt for the upstream port used by the expose-port fixer.
fn prompt_expose_port(service: &str, candidate_ports: &[u16]) -> Result<u16> {
    inquire::Select::new(
        &format!("Use which upstream container port for services.{service}?"),
        candidate_ports.to_vec(),
    )
    .prompt()
    .map_err(|e| anyhow::anyhow!("port prompt cancelled: {e}"))
}

/// Print a compact diff of proposed fixes.
fn print_compact_diff(current: &str, proposed: &str) {
    let diff = similar::TextDiff::from_lines(current, proposed);
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

/// Confirm applying proposed fixes, refusing non-TTY prompts.
fn confirm_apply() -> Result<bool> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("no TTY for confirmation; pass --yes to apply fixes");
    }
    print!("Apply fixes? [y/N] ");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(
        buf.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Pick the compose document for the requested target.
///
/// 1. `None` → the first supported compose document in the cwd.
/// 2. A value that resolves to a file or directory on disk → load locally.
/// 3. Otherwise → treat as a stack name and fetch from the controller.
async fn resolve_compose(
    target: Option<&str>,
    context: Option<&str>,
    fix_mode: bool,
) -> Result<ResolvedCompose> {
    if let Some(t) = target {
        let path = std::path::Path::new(t);
        if path.exists() {
            let mut resolved = resolve_local_path(path)?;
            if resolved.file_name().and_then(|s| s.to_str()) == Some("stack.toml") {
                resolved = resolve_manifest_compose_path_for_audit(&resolved)?;
            }
            let body = std::fs::read_to_string(&resolved)
                .with_context(|| format!("reading compose file {}", resolved.display()))?;
            let document = document::ComposeDocument::parse_path(&resolved, &body)?;
            return Ok(ResolvedCompose {
                body,
                document,
                source: ComposeSource::Local(resolved),
                session: None,
            });
        }
        // Not on disk; assume stack name.
        let session = Session::open(context).await?;
        let id = crate::compose_cmd::resolve_stack_id(&session, t).await?;
        match crate::compose_cmd::fetch_compose(&session, &id).await? {
            Some(row) => {
                let document = document::ComposeDocument::parse_controller_yaml(&row.compose_yaml)?;
                Ok(ResolvedCompose {
                    body: row.compose_yaml,
                    document,
                    source: ComposeSource::Controller {
                        name: t.to_string(),
                        id,
                        sha256: row.sha256,
                    },
                    session: Some(session),
                })
            }
            None => Err(missing_controller_compose_message(t, fix_mode)),
        }
    } else {
        let mut resolved = resolve_local_path(Path::new("."))?;
        if resolved.file_name().and_then(|s| s.to_str()) == Some("stack.toml") {
            resolved = resolve_manifest_compose_path_for_audit(&resolved)?;
        }
        let body = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading compose file {}", resolved.display()))?;
        let document = document::ComposeDocument::parse_path(&resolved, &body)?;
        Ok(ResolvedCompose {
            body,
            document,
            source: ComposeSource::Local(resolved),
            session: None,
        })
    }
}

/// Resolve a manifest compose target conservatively for doctor.
fn resolve_manifest_compose_path_for_audit(stack_toml: &Path) -> Result<PathBuf> {
    let paths = apply::resolve_manifest_compose_paths(stack_toml)?;
    if paths.len() == 1 {
        return Ok(paths[0].clone());
    }

    let mut paths_with_findings = Vec::new();
    for path in &paths {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading compose file {}", path.display()))?;
        let document = document::ComposeDocument::parse_path(path, &body)?;
        if !audit(&document.value).is_empty() {
            paths_with_findings.push(path.clone());
        }
    }

    if paths_with_findings.len() == 1 {
        return Ok(paths_with_findings.remove(0));
    }

    Err(manifest_doctor_compose_error(paths_with_findings.len()))
}

/// Build the operator guidance used when doctor cannot pick one manifest compose.
fn manifest_doctor_compose_error(finding_compose_count: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "stack.toml has {} compose files with doctor findings; pass the specific compose file to isd stack doctor",
        finding_compose_count
    )
}

/// Build the missing-compose guidance for read-only and fix doctor modes.
fn missing_controller_compose_message(stack_name: &str, fix_mode: bool) -> anyhow::Error {
    let fix_line = if fix_mode {
        "\n\ndoctor --fix cannot mutate missing compose."
    } else {
        ""
    };
    anyhow::anyhow!(
        "stack {stack_name:?} is tracked by the controller but has no compose stored.{fix_line}\n\
         \n\
         This usually means the stack was deployed outside isengard \
         (e.g. via `docker compose up` directly). The agent discovers it \
         from the `com.docker.compose.project={stack_name}` label on the containers \
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
    )
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
    fn doctor_args_parse_fix_yes_force() {
        use clap::Parser;

        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(flatten)]
            args: DoctorArgs,
        }

        let w = Wrap::try_parse_from(["x", "--fix", "--yes", "--force", "servarr"]).unwrap();
        assert!(w.args.fix);
        assert!(w.args.yes);
        assert!(w.args.force);
        assert_eq!(w.args.target.as_deref(), Some("servarr"));
    }

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
    fn apply_fix_inputs_uses_hostname_callback() {
        let mut document = document::ComposeDocument::parse_path(
            std::path::Path::new("compose.yaml"),
            "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n",
        )
        .unwrap();
        let findings = audit(&document.value);

        let changed = apply_fix_inputs(
            &mut document,
            findings,
            |service| {
                assert_eq!(service, "web");
                Ok(true)
            },
            |service| {
                assert_eq!(service, "web");
                Ok("web.test".to_string())
            },
            |_, _| panic!("port callback should not be called for hostname fixes"),
        )
        .unwrap();

        assert!(changed);
        assert_eq!(
            document.value["services"]["web"]["labels"]["isengard.expose"].as_str(),
            Some("web.test")
        );
    }

    #[test]
    fn apply_fix_inputs_skips_service_without_prompting_for_hostname() {
        let mut document = document::ComposeDocument::parse_path(
            std::path::Path::new("compose.yaml"),
            "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n",
        )
        .unwrap();
        let findings = audit(&document.value);

        let changed = apply_fix_inputs(
            &mut document,
            findings,
            |_| Ok(false),
            |_| panic!("hostname callback should not be called for skipped services"),
            |_, _| panic!("port callback should not be called for skipped services"),
        )
        .unwrap();

        assert!(!changed);
        assert!(document.value["services"]["web"]["labels"].is_null());
    }

    #[test]
    fn apply_fix_inputs_fixes_service_when_confirmed() {
        let mut document = document::ComposeDocument::parse_path(
            std::path::Path::new("compose.yaml"),
            "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n",
        )
        .unwrap();
        let findings = audit(&document.value);

        let changed = apply_fix_inputs(
            &mut document,
            findings,
            |service| {
                assert_eq!(service, "web");
                Ok(true)
            },
            |service| {
                assert_eq!(service, "web");
                Ok("web.test".to_string())
            },
            |_, _| panic!("port callback should not be called for hostname fixes"),
        )
        .unwrap();

        assert!(changed);
        assert_eq!(
            document.value["services"]["web"]["labels"]["isengard.expose"].as_str(),
            Some("web.test")
        );
    }

    #[test]
    fn apply_fix_inputs_sets_single_candidate_expose_port() {
        let mut document = document::ComposeDocument::parse_path(
            std::path::Path::new("compose.yaml"),
            "services:\n  qbittorrent:\n    labels:\n      isengard.expose: qb.vallee.casa\n",
        )
        .unwrap();
        let findings = vec![Finding {
            id: "EXPOSE_PORT_AMBIGUOUS",
            severity: Severity::Warning,
            message: "ambiguous".into(),
            hint: None,
            target: Some(FindingTarget::Service {
                name: "qbittorrent".into(),
            }),
            fix: Some(FixSpec::SetExposePort {
                service: "qbittorrent".into(),
                candidate_ports: vec![8080],
            }),
        }];

        let changed = apply_fix_inputs(
            &mut document,
            findings,
            |service| {
                assert_eq!(service, "qbittorrent");
                Ok(true)
            },
            |_| panic!("hostname callback should not be called for port fixes"),
            |_, _| panic!("port callback should not be called with one candidate"),
        )
        .unwrap();

        assert!(changed);
        assert_eq!(
            document.value["services"]["qbittorrent"]["labels"]["isengard.expose"].as_str(),
            Some("qb.vallee.casa")
        );
        assert_eq!(
            document.value["services"]["qbittorrent"]["labels"]["isengard.expose.port"].as_str(),
            Some("8080")
        );
    }

    #[test]
    fn missing_controller_compose_message_keeps_read_only_guidance() {
        let message = missing_controller_compose_message("demo", false).to_string();
        assert!(message.contains("stack \"demo\" is tracked by the controller"));
        assert!(message.contains("isd stack doctor /path/to/your/compose.yaml"));
        assert!(!message.contains("doctor --fix cannot mutate missing compose"));
    }

    #[test]
    fn missing_controller_compose_message_explains_fix_limitation() {
        let message = missing_controller_compose_message("demo", true).to_string();
        assert!(message.contains("stack \"demo\" is tracked by the controller"));
        assert!(message.contains("doctor --fix cannot mutate missing compose"));
        assert!(message.contains("isd stack deploy /path/to/your/compose.yaml"));
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

        let resolved = resolve_compose(Some(tmp.path().to_str().unwrap()), None, false)
            .await
            .unwrap();
        let findings = audit(&resolved.document.value);
        assert!(findings.iter().any(|f| f.id == "EXPOSE_HOST_MISSING"));
    }

    #[tokio::test]
    async fn stack_toml_with_one_finding_compose_resolves_that_compose() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("stack.toml"),
            "name = \"demo\"\ncompose = [\"base.toml\", \"override.toml\"]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("base.toml"),
            "[services.web]\nimage = \"nginx\"\nports = [\"8080:80\"]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("override.toml"),
            "[services.worker]\nimage = \"alpine\"\n",
        )
        .unwrap();

        let resolved = resolve_compose(Some(tmp.path().to_str().unwrap()), None, false)
            .await
            .unwrap();
        let findings = audit(&resolved.document.value);

        match resolved.source {
            ComposeSource::Local(path) => assert_eq!(path, tmp.path().join("base.toml")),
            ComposeSource::Controller { .. } => panic!("expected local compose source"),
        }
        assert!(findings.iter().any(|f| f.id == "EXPOSE_HOST_MISSING"));
    }

    #[tokio::test]
    async fn stack_toml_with_multiple_finding_composes_errors_with_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("stack.toml"),
            "name = \"demo\"\ncompose = [\"base.toml\", \"override.toml\"]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("base.toml"),
            "[services.web]\nimage = \"nginx\"\nports = [\"8080:80\"]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("override.toml"),
            "[services.admin]\nimage = \"nginx\"\nports = [\"8081:80\"]\n",
        )
        .unwrap();

        let err = match resolve_compose(Some(tmp.path().to_str().unwrap()), None, false).await {
            Ok(_) => panic!("expected ambiguous manifest compose error"),
            Err(err) => err,
        };
        let message = err.to_string();

        assert!(
            message.contains("pass the specific compose file"),
            "{message}"
        );
        assert!(message.contains("isd stack doctor"), "{message}");
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
        assert!(message.contains("isd stack doctor"), "{message}");
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
        assert!(message.contains("isd stack doctor"), "{message}");
    }
}
