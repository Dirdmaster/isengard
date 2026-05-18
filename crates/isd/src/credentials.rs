//! Credentials store at `~/.config/isengard/credentials.toml`.
//!
//! Each saved context selects a docker backend that determines how `isd`
//! reaches the controller. After Track G the only supported shape is
//! `kind = "docker"` with a docker endpoint URL: the controller is
//! discovered automatically by querying for the
//! `io.isengard.role=controller` container on that host.
//!
//! TOML shape:
//!
//! ```toml
//! default_context = "lausanne"
//!
//! [[contexts]]
//! name = "lausanne"
//! kind = "docker"
//! url = "ssh://dirdmaster@10.17.0.125"
//! ```
//!
//! Legacy entries (pre-Track-G `kind = "http"` or `kind = "ssh"`, or the
//! pre-context-redesign `controller_url` + `token` shape) are no longer
//! accepted. Re-create them with `isd context import <docker-context>`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// How `isd` reaches the controller for a given context. After Track G the
/// only supported transport is a docker endpoint: the controller container
/// is discovered by label on that host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Backend {
    /// Docker socket as the sole transport. The controller is discovered
    /// by querying `docker ps --filter label=io.isengard.role=controller`;
    /// no separate dashboard port is configured.
    Docker {
        /// Docker endpoint URL (e.g. `ssh://user@host`, `tcp://host:2375`,
        /// `unix:///var/run/docker.sock`). Same shape `docker context create`
        /// accepts.
        url: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContextEntry {
    pub name: String,
    #[serde(flatten)]
    pub backend: Backend,
}

/// Custom deserialize so legacy entries (with `controller_url` + `token` +
/// `ca_fingerprint_sha256` and no `kind`, or `kind = "http"` / `kind =
/// "ssh"`) surface an actionable error pointing at `isd context import`
/// instead of an opaque serde mismatch.
impl<'de> Deserialize<'de> for ContextEntry {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            name: String,
            #[serde(default)]
            kind: Option<String>,
            // new-shape fields
            #[serde(default)]
            url: Option<String>,
            // legacy-shape fields (read-only, dropped on next save). Kept
            // so we can surface a precise error pointing at the
            // deprecated key rather than a generic "missing field".
            #[serde(default)]
            #[allow(dead_code)]
            target: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            dashboard_port: Option<u16>,
            #[serde(default)]
            #[allow(dead_code)]
            controller_url: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            token: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            ca_fingerprint_sha256: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            docker: Option<String>,
        }
        let raw = Raw::deserialize(de)?;
        let backend = match raw.kind.as_deref() {
            Some("docker") => Backend::Docker {
                url: raw
                    .url
                    .ok_or_else(|| serde::de::Error::custom("kind=docker requires `url`"))?,
            },
            Some(legacy @ ("http" | "ssh")) => {
                return Err(serde::de::Error::custom(format!(
                    "context {:?} uses legacy {legacy} backend; recreate with `isd context import <name>`",
                    raw.name
                )));
            }
            Some(other) => {
                return Err(serde::de::Error::custom(format!(
                    "unknown context kind {other:?} (expected `docker`)"
                )));
            }
            None => {
                return Err(serde::de::Error::custom(format!(
                    "context {:?} predates Track D; recreate with `isd context import <name>`",
                    raw.name
                )));
            }
        };
        Ok(ContextEntry {
            name: raw.name,
            backend,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialsFile {
    /// Name of the context returned by [`CredentialsFile::default_or_named`]
    /// when the caller doesn't pass `--context`. Always points at one of the
    /// `contexts[].name` values when the file has at least one context.
    #[serde(default)]
    pub default_context: Option<String>,
    #[serde(default)]
    pub contexts: Vec<ContextEntry>,
}

impl CredentialsFile {
    /// Resolve the active context. With `name = Some(...)` returns that one;
    /// with `name = None` falls back to `default_context`. Errors if either
    /// look-up fails so subcommands can stop early with a clear message.
    pub fn default_or_named(&self, name: Option<&str>) -> Result<&ContextEntry> {
        match name {
            Some(n) => self.contexts.iter().find(|c| c.name == n).with_context(|| {
                format!("no saved context named {n:?}; run `isd context create` first")
            }),
            None => {
                let want = self.default_context.as_deref().with_context(
                    || "no default context set; run `isd context create <name> --docker <uri>` first",
                )?;
                self.contexts
                    .iter()
                    .find(|c| c.name == want)
                    .with_context(|| {
                        format!("default context {want:?} not found in credentials file")
                    })
            }
        }
    }

    /// Insert (or replace) the named context. Sets it as default if the file
    /// has no default yet; otherwise leaves the default untouched.
    pub fn upsert(&mut self, ctx: ContextEntry) {
        let name = ctx.name.clone();
        if let Some(slot) = self.contexts.iter_mut().find(|c| c.name == name) {
            *slot = ctx;
        } else {
            self.contexts.push(ctx);
        }
        if self.default_context.is_none() {
            self.default_context = Some(name);
        }
    }

    /// Set the default context to `name`. Errors if no such context exists.
    pub fn set_default(&mut self, name: &str) -> Result<()> {
        if !self.contexts.iter().any(|c| c.name == name) {
            anyhow::bail!("no saved context named {name:?}");
        }
        self.default_context = Some(name.to_string());
        Ok(())
    }

    /// Remove a context. If it was the default, clears the default (caller
    /// can pick a new one with `set_default`). Returns whether a context was
    /// removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.contexts.len();
        self.contexts.retain(|c| c.name != name);
        let removed = before != self.contexts.len();
        if removed && self.default_context.as_deref() == Some(name) {
            self.default_context = None;
        }
        removed
    }
}

/// `~/.config/isengard/credentials.toml`. Honours `XDG_CONFIG_HOME` via the
/// `dirs` crate. Returns an error only when the OS has no concept of a home
/// dir (CI without `$HOME`); in that case the operator can pass a path via
/// `ISD_CREDENTIALS_FILE`.
pub fn default_credentials_path() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("ISD_CREDENTIALS_FILE") {
        return Ok(PathBuf::from(env));
    }
    let dir = dirs::config_dir()
        .context("no config directory available (set XDG_CONFIG_HOME or ISD_CREDENTIALS_FILE)")?;
    Ok(dir.join("isengard").join("credentials.toml"))
}

/// Load the credentials file, returning an empty [`CredentialsFile`] when the
/// path doesn't exist yet (first-run case).
pub fn load(path: &Path) -> Result<CredentialsFile> {
    if !path.exists() {
        return Ok(CredentialsFile::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading credentials at {}", path.display()))?;
    let parsed: CredentialsFile = toml::from_str(&text)
        .with_context(|| format!("parsing credentials at {}", path.display()))?;
    Ok(parsed)
}

/// Persist the credentials file at `path` with mode `0600`. Creates any
/// missing parent directories.
pub fn save(path: &Path, file: &CredentialsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let toml = toml::to_string_pretty(file).context("serializing credentials to TOML")?;
    std::fs::write(path, toml.as_bytes())
        .with_context(|| format!("writing credentials to {}", path.display()))?;
    set_owner_only_perms(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod 0600 on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_perms(_path: &Path) -> Result<()> {
    // Windows + WASI: leave to the OS. Operators on those platforms can
    // tighten ACLs manually if needed; the file format itself is portable.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_entry(name: &str, url: &str) -> ContextEntry {
        ContextEntry {
            name: name.into(),
            backend: Backend::Docker { url: url.into() },
        }
    }

    #[test]
    fn round_trip_preserves_docker_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let mut file = CredentialsFile::default();
        file.upsert(docker_entry("local", "unix:///var/run/docker.sock"));
        file.upsert(docker_entry("lausanne", "ssh://dirdmaster@10.17.0.125"));
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, file);
    }

    #[test]
    fn legacy_http_kind_rejected_with_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let legacy = r#"
default_context = "default"

[[contexts]]
name = "default"
kind = "http"
url = "http://127.0.0.1:9418"
"#;
        std::fs::write(&path, legacy).unwrap();
        let err = load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("legacy http backend"), "msg: {msg}");
        assert!(msg.contains("isd context import"), "msg: {msg}");
    }

    #[test]
    fn legacy_ssh_kind_rejected_with_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let legacy = r#"
default_context = "lausanne"

[[contexts]]
name = "lausanne"
kind = "ssh"
target = "dirdmaster@10.17.0.125"
"#;
        std::fs::write(&path, legacy).unwrap();
        let err = load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("legacy ssh backend"), "msg: {msg}");
        assert!(msg.contains("isd context import"), "msg: {msg}");
    }

    #[test]
    fn legacy_controller_url_rejected_with_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let legacy = r#"
default_context = "default"

[[contexts]]
name = "default"
controller_url = "http://127.0.0.1:9418"
token = "abc"
ca_fingerprint_sha256 = ""
"#;
        std::fs::write(&path, legacy).unwrap();
        let err = load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("predates Track D"), "msg: {msg}");
        assert!(msg.contains("isd context import"), "msg: {msg}");
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, CredentialsFile::default());
    }

    #[test]
    fn default_or_named_resolves_in_both_modes() {
        let mut file = CredentialsFile::default();
        file.upsert(docker_entry("home", "unix:///var/run/docker.sock"));
        let resolved = file.default_or_named(None).unwrap();
        assert_eq!(resolved.name, "home");
        let by_name = file.default_or_named(Some("home")).unwrap();
        assert_eq!(by_name.name, "home");
        assert!(file.default_or_named(Some("missing")).is_err());
    }

    #[test]
    fn upsert_only_promotes_default_when_unset() {
        let mut file = CredentialsFile::default();
        file.upsert(docker_entry("home", "unix:///var/run/docker.sock"));
        assert_eq!(file.default_context.as_deref(), Some("home"));
        file.upsert(docker_entry("work", "ssh://user@work"));
        // Default stays "home" because it was already set.
        assert_eq!(file.default_context.as_deref(), Some("home"));
        file.set_default("work").unwrap();
        assert_eq!(file.default_context.as_deref(), Some("work"));
    }

    #[test]
    fn remove_clears_default_when_removing_the_default() {
        let mut file = CredentialsFile::default();
        file.upsert(docker_entry("home", "unix:///var/run/docker.sock"));
        file.upsert(docker_entry("work", "ssh://user@work"));
        file.set_default("work").unwrap();
        assert!(file.remove("work"));
        assert_eq!(file.default_context, None);
        assert!(!file.remove("nonexistent"));
    }

    #[test]
    #[cfg(unix)]
    fn save_writes_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let mut file = CredentialsFile::default();
        file.upsert(docker_entry("home", "unix:///var/run/docker.sock"));
        save(&path, &file).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn docker_backend_serializes_and_deserializes() {
        let toml_in = r#"
default_context = "lausanne"

[[contexts]]
name = "lausanne"
kind = "docker"
url = "ssh://dirdmaster@10.17.0.125"
"#;
        let file: CredentialsFile = toml::from_str(toml_in).unwrap();
        assert_eq!(file.default_context.as_deref(), Some("lausanne"));
        assert_eq!(file.contexts.len(), 1);
        match &file.contexts[0].backend {
            Backend::Docker { url } => assert_eq!(url, "ssh://dirdmaster@10.17.0.125"),
        }
        // Round-trip.
        let toml_out = toml::to_string(&file).unwrap();
        assert!(toml_out.contains("kind = \"docker\""));
        assert!(toml_out.contains("url = \"ssh://dirdmaster@10.17.0.125\""));
    }
}
