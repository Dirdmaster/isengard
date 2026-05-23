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

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Args;
use serde_yaml::Value;

pub mod checks;

/// Severity of a [`Finding`]. v0.1 only emits `Warning`s; `Error`
/// is reserved for future checks that would make the deploy fail
/// (e.g. a `secrets: file:` that the agent will reject anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Surface to the operator; deploy continues.
    Warning,
}

/// A single audit finding. Carries an id for filtering / muting, a
/// severity, the human-readable message, and an optional hint line
/// the doctor verb can print as guidance.
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
/// YAML doesn't parse.
///
/// # Errors
///
/// Returns `Err` when the file can't be read or the bytes aren't
/// valid YAML.
pub fn load_compose(path: &std::path::Path) -> Result<Value> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading compose file {}", path.display()))?;
    serde_yaml::from_slice(&bytes)
        .with_context(|| format!("parsing compose YAML at {}", path.display()))
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
    /// Path to the compose file (or directory containing one).
    ///
    /// Defaults to `./compose.yaml` when omitted.
    pub path: Option<PathBuf>,
}

/// `isd stack doctor`: run the audit and print every finding. v0.1
/// is read-only. The interactive fix loop lands in v0.2.
///
/// # Errors
///
/// Returns `Err` when the compose file can't be located or parsed.
pub async fn run(args: DoctorArgs) -> Result<()> {
    let path = resolve_compose_path(args.path.as_deref())?;
    let compose = load_compose(&path)?;
    let findings = audit(&compose);

    if findings.is_empty() {
        println!("{} looks healthy. No findings.", path.display());
        return Ok(());
    }

    println!("{}:", path.display());
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
        "{} finding(s). Re-run with `isd stack doctor --fix <path>` once v0.2 ships the interactive fixer.",
        findings.len()
    );
    Ok(())
}

/// Resolve the operator's positional arg into a concrete compose path.
/// Mirrors the resolution `isd stack deploy` does, but stays in-module
/// so we don't pull on compose_cmd's heavier path-classifier.
fn resolve_compose_path(arg: Option<&std::path::Path>) -> Result<PathBuf> {
    let target = arg.unwrap_or_else(|| std::path::Path::new("."));
    let resolved = if target.is_dir() {
        let candidate = target.join("compose.yaml");
        if candidate.exists() {
            candidate
        } else {
            let yml = target.join("compose.yml");
            if yml.exists() {
                yml
            } else {
                return Err(anyhow::anyhow!(
                    "no compose.yaml or compose.yml in {}",
                    target.display()
                ));
            }
        }
    } else if target.exists() {
        target.to_path_buf()
    } else {
        return Err(anyhow::anyhow!(
            "compose path {} does not exist",
            target.display()
        ));
    };
    Ok(resolved)
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
}
