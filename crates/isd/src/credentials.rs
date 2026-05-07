//! Credentials store at `~/.config/isengard/credentials.toml`.
//!
//! TOML shape (multi-context, additive):
//!
//! ```toml
//! default_context = "default"
//!
//! [[contexts]]
//! name = "default"
//! controller_url = "https://controller.local:9417"
//! token = "..."
//! ca_fingerprint_sha256 = "ab:cd:..."
//! ```
//!
//! v0.3a writes a single context (`default`) on `isd login`. Future
//! subcommands `isd context add / use` round out multi-controller workflows
//! without changing the file shape.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextEntry {
    pub name: String,
    pub controller_url: String,
    pub token: String,
    /// Colon-separated lowercase hex SHA-256 fingerprint (e.g.
    /// `ab:cd:ef:...`) of the controller's TLS leaf cert. Captured at
    /// `isd login` and re-verified on every subsequent connect.
    pub ca_fingerprint_sha256: String,
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
            Some(n) => self
                .contexts
                .iter()
                .find(|c| c.name == n)
                .with_context(|| format!("no saved context named {n:?}; run `isd login` first")),
            None => {
                let want = self.default_context.as_deref().with_context(
                    || "no default context set; run `isd login <controller_url>` first",
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

    /// Insert (or replace) the named context and set it as the default.
    pub fn upsert_default(&mut self, ctx: ContextEntry) {
        let name = ctx.name.clone();
        if let Some(slot) = self.contexts.iter_mut().find(|c| c.name == name) {
            *slot = ctx;
        } else {
            self.contexts.push(ctx);
        }
        self.default_context = Some(name);
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

    fn entry(name: &str) -> ContextEntry {
        ContextEntry {
            name: name.into(),
            controller_url: "https://controller.local:9417".into(),
            token: "tok-1234".into(),
            ca_fingerprint_sha256: "ab:cd:ef".into(),
        }
    }

    #[test]
    fn round_trip_preserves_defaults_and_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let mut file = CredentialsFile::default();
        file.upsert_default(entry("home"));
        file.upsert_default(entry("work"));
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.default_context.as_deref(), Some("work"));
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
        file.upsert_default(entry("home"));
        let resolved = file.default_or_named(None).unwrap();
        assert_eq!(resolved.name, "home");
        let by_name = file.default_or_named(Some("home")).unwrap();
        assert_eq!(by_name.name, "home");
        assert!(file.default_or_named(Some("missing")).is_err());
    }

    #[test]
    fn upsert_replaces_existing_and_updates_default() {
        let mut file = CredentialsFile::default();
        file.upsert_default(entry("home"));
        let mut second = entry("home");
        second.token = "rotated".into();
        file.upsert_default(second.clone());
        assert_eq!(file.contexts.len(), 1);
        assert_eq!(file.contexts[0].token, "rotated");
        assert_eq!(file.default_context.as_deref(), Some("home"));
    }

    #[test]
    #[cfg(unix)]
    fn save_writes_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let mut file = CredentialsFile::default();
        file.upsert_default(entry("home"));
        save(&path, &file).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
