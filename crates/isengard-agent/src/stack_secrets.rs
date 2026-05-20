//! Follow-up: read the stack-level `secrets = [.]` list out
//! of an on-disk `stack.toml`.
//!
//! Background: the controller persists every `stack.toml` verbatim to
//! `<stack_dir>/stack.toml` via `compose_writer::apply_controller_write`.
//! The reconcile path (watcher + apply) needs to know which fleet
//! secrets to mount into every container of the stack, but the watcher
//! only sees the compose YAML. Pulling the list back out of the
//! persisted manifest keeps the watcher self-contained: any reconcile
//! (controller WriteCompose, hand edit, `isd deploy`) sees the same
//! stack-level secrets.
//!
//! The agent does NOT validate the full manifest here. The dashboard +
//! controller already rejected malformed manifests at submit time. The
//! parser is intentionally lenient: it pulls a top-level `secrets`
//! array of strings, returns an empty list when the field is absent or
//! the file doesn't exist, and refuses (with a clear error) entries
//! that aren't strings or that fail the same character validation as
//! per-service secret references.

use std::path::Path;

use thiserror::Error;

/// File name the agent writes a verbatim manifest to inside each stack dir.
pub const STACK_MANIFEST_FILE: &str = "stack.toml";

/// Errors [`read_stack_secrets`] can return.
#[derive(Debug, Error)]
pub enum StackSecretsError {
    /// Filesystem read failed.
    #[error("read {path}: {source}")]
    Io {
        /// File path that failed to read.
        path: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Manifest is not valid TOML.
    #[error("parse {path}: {source}")]
    Toml {
        /// Manifest path.
        path: String,
        /// Underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },

    /// Top-level `secrets` exists but isn't an array of strings.
    #[error("{path}: top-level `secrets` must be an array of strings")]
    Shape {
        /// Manifest path.
        path: String,
    },

    /// A secret name failed the character-set check.
    #[error("{path}: secret name {name:?} contains invalid characters")]
    InvalidName {
        /// Manifest path.
        path: String,
        /// Offending secret name.
        name: String,
    },
}

/// Read the stack-level `secrets = [...]` list out of `<dir>/stack.toml`.
///
/// Returns `Ok(vec![])` when:
///  - the manifest file does not exist (legacy compose-only stack), or
///  - the manifest has no top-level `secrets` key.
///
/// Returns `Err` only on hard I/O or parse failure, or when the
/// `secrets` field is present but malformed (not an array of strings,
/// or a name fails validation). Callers can treat `Err` as a deploy
/// failure: the watcher logs + skips the reconcile rather than mounting
/// an unknown set of secrets.
pub fn read_stack_secrets(dir: &Path) -> Result<Vec<String>, StackSecretsError> {
    let path = dir.join(STACK_MANIFEST_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(&path).map_err(|source| StackSecretsError::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse_stack_secrets(&body, &path.display().to_string())
}

/// Parse a `stack.toml` body and pull out the top-level `secrets`
/// array. Split out so tests can exercise the parser without touching
/// the disk.
pub fn parse_stack_secrets(
    body: &str,
    path_for_errors: &str,
) -> Result<Vec<String>, StackSecretsError> {
    let doc: toml::Value = toml::from_str(body).map_err(|source| StackSecretsError::Toml {
        path: path_for_errors.to_string(),
        source,
    })?;
    let Some(raw) = doc.get("secrets") else {
        return Ok(Vec::new());
    };
    let Some(arr) = raw.as_array() else {
        return Err(StackSecretsError::Shape {
            path: path_for_errors.to_string(),
        });
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let s = entry.as_str().ok_or_else(|| StackSecretsError::Shape {
            path: path_for_errors.to_string(),
        })?;
        if !is_valid_secret_name(s) {
            return Err(StackSecretsError::InvalidName {
                path: path_for_errors.to_string(),
                name: s.to_string(),
            });
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// Same character rules as `secret_fetch::validate_secret_name`: ASCII
/// alphanumeric plus `.`, `_`, `-`; non-empty; max 64 chars. Kept in
/// sync deliberately: if a name passes here it also passes the fetch
/// validator.
fn is_valid_secret_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_returns_empty_when_no_secrets_key() {
        let body = "name = \"hello\"\ncompose = [\"compose.yml\"]\n";
        let v = parse_stack_secrets(body, "test").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn parse_extracts_string_array() {
        let body = "name = \"hello\"\nsecrets = [\"cf_dns_token\", \"github_token\"]\n";
        let v = parse_stack_secrets(body, "test").unwrap();
        assert_eq!(v, vec!["cf_dns_token", "github_token"]);
    }

    #[test]
    fn parse_rejects_non_array_secrets() {
        let body = "secrets = \"oops\"\n";
        let err = parse_stack_secrets(body, "test").unwrap_err();
        assert!(matches!(err, StackSecretsError::Shape { .. }));
    }

    #[test]
    fn parse_rejects_array_of_non_strings() {
        let body = "secrets = [1, 2]\n";
        let err = parse_stack_secrets(body, "test").unwrap_err();
        assert!(matches!(err, StackSecretsError::Shape { .. }));
    }

    #[test]
    fn parse_rejects_invalid_name() {
        let body = "secrets = [\"bad/name\"]\n";
        let err = parse_stack_secrets(body, "test").unwrap_err();
        assert!(matches!(err, StackSecretsError::InvalidName { .. }));
    }

    #[test]
    fn parse_rejects_empty_name() {
        let body = "secrets = [\"\"]\n";
        let err = parse_stack_secrets(body, "test").unwrap_err();
        assert!(matches!(err, StackSecretsError::InvalidName { .. }));
    }

    #[test]
    fn read_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let v = read_stack_secrets(tmp.path()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn read_extracts_from_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(STACK_MANIFEST_FILE);
        std::fs::write(
            &path,
            "name = \"hello\"\nsecrets = [\"a\", \"b_one\", \"c.dot\"]\n",
        )
        .unwrap();
        let v = read_stack_secrets(tmp.path()).unwrap();
        assert_eq!(v, vec!["a", "b_one", "c.dot"]);
    }

    #[test]
    fn read_tolerates_full_manifest_without_parsing_fleet_or_hooks() {
        // Real stack.toml has many fields; the lenient parser ignores
        // everything except the top-level `secrets` array.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(STACK_MANIFEST_FILE);
        std::fs::write(
            &path,
            r#"
name = "hello"
fleet = "edge"
compose = ["compose.yml"]
secrets = ["cf_dns_token"]

[[hooks]]
on = "pre-deploy"
cmd = ["/usr/local/bin/notify.sh"]

[overlays.prod]
compose = ["compose.prod.yml"]
"#,
        )
        .unwrap();
        let v = read_stack_secrets(tmp.path()).unwrap();
        assert_eq!(v, vec!["cf_dns_token"]);
    }
}
