//! Credentials store at `~/.config/isengard/credentials.toml`.
//!
//! Each saved context selects a backend that determines how `isd` reaches
//! the controller:
//!
//! - **`http`**: direct HTTP/HTTPS to a URL the operator can reach. Used
//!   for local dev (`http://127.0.0.1:9418`).
//! - **`ssh`**: tunnel to the controller via SSH. The operator's existing
//!   SSH config (Hosts, IdentityFile, ProxyJump, agent forwarding)
//!   handles authentication; isd shells out to system `ssh` to set up a
//!   port forward for each command's lifetime. Modeled on
//!   `docker context create --docker host=ssh://...`.
//!
//! TOML shape:
//!
//! ```toml
//! default_context = "lausanne"
//!
//! [[contexts]]
//! name = "lausanne"
//! kind = "ssh"
//! target = "dirdmaster@10.17.0.125"
//! dashboard_port = 9418
//!
//! [[contexts]]
//! name = "local"
//! kind = "http"
//! url = "http://127.0.0.1:9418"
//! ```
//!
//! Legacy entries (pre-context-redesign) used `controller_url` + `token`
//!     + `ca_fingerprint_sha256` fields with no `kind`. Those are auto-
//!     migrated to `kind = "http"` on read; the token + fingerprint
//!     fields are dropped.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// How `isd` reaches the controller for a given context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Backend {
    /// Direct HTTP/HTTPS to a URL.
    Http {
        /// Full URL including scheme + port (e.g. `http://127.0.0.1:9418`).
        url: String,
    },
    /// SSH tunnel. `target` is whatever `ssh` understands (`user@host`,
    /// `host`, or a `Host` alias from `~/.ssh/config`).
    Ssh {
        target: String,
        /// Port the controller's dashboard listens on inside the SSH
        /// destination. Tunnel forwards `localhost:<ephemeral>` ->
        /// `127.0.0.1:<dashboard_port>` on the remote.
        #[serde(default = "default_dashboard_port")]
        dashboard_port: u16,
    },
    /// Docker socket as the sole transport. The controller is discovered
    /// by querying `docker ps --filter label=io.isengard.role=controller`;
    /// no separate dashboard port is configured. This is the Track D path.
    Docker {
        /// Docker endpoint URL (e.g. `ssh://user@host`, `tcp://host:2375`,
        /// `unix:///var/run/docker.sock`). Same shape `docker context create`
        /// accepts.
        url: String,
    },
}

fn default_dashboard_port() -> u16 {
    9418
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContextEntry {
    pub name: String,
    #[serde(flatten)]
    pub backend: Backend,
    /// Phase 0.20: optional Docker Engine endpoint for direct-bollard
    /// access. Accepts `ssh://user@host`, `tcp://host:port`, or
    /// `unix:///path`. Coexists with the controller-backed `kind`/`url`
    /// / `target` fields; one or both may be set. When present, `isd ps
    /// --backend docker` uses this instead of the controller round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<String>,
}

/// Custom deserialize so legacy entries (with `controller_url` + `token` +
/// `ca_fingerprint_sha256` and no `kind`) migrate cleanly to the new
/// `Http`-backend shape on first read. Operators don't have to re-run
/// `isd login` (which is gone) just because they upgraded.
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
            #[serde(default)]
            target: Option<String>,
            #[serde(default)]
            dashboard_port: Option<u16>,
            // legacy-shape fields (read-only, dropped on next save)
            #[serde(default)]
            controller_url: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            token: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            ca_fingerprint_sha256: Option<String>,
            // Phase 0.20: direct-bollard endpoint (independent of kind).
            #[serde(default)]
            docker: Option<String>,
        }
        let raw = Raw::deserialize(de)?;
        let backend = match raw.kind.as_deref() {
            Some("http") => Backend::Http {
                url: raw
                    .url
                    .ok_or_else(|| serde::de::Error::custom("kind=http requires `url`"))?,
            },
            Some("ssh") => Backend::Ssh {
                target: raw
                    .target
                    .ok_or_else(|| serde::de::Error::custom("kind=ssh requires `target`"))?,
                dashboard_port: raw.dashboard_port.unwrap_or_else(default_dashboard_port),
            },
            Some("docker") => Backend::Docker {
                url: raw
                    .url
                    .ok_or_else(|| serde::de::Error::custom("kind=docker requires `url`"))?,
            },
            Some(other) => {
                return Err(serde::de::Error::custom(format!(
                    "unknown context kind {other:?} (expected `http`, `ssh`, or `docker`)"
                )));
            }
            None => {
                // Legacy: controller_url -> Http migration.
                let url = raw.controller_url.ok_or_else(|| {
                    serde::de::Error::custom(
                        "legacy context missing both `kind` and `controller_url`",
                    )
                })?;
                Backend::Http { url }
            }
        };
        Ok(ContextEntry {
            name: raw.name,
            backend,
            docker: raw.docker,
        })
    }
}

impl ContextEntry {
    /// Rewrite a legacy `Backend::Http { url == NO_CONTROLLER_SENTINEL }` +
    /// `docker: Some(...)` pair into a single `Backend::Docker { url }`. No-op
    /// for entries already in the post-Track-D shape.
    pub fn migrate_legacy_sentinel(&self) -> Self {
        use crate::context::NO_CONTROLLER_SENTINEL;
        if let (Backend::Http { url }, Some(docker_url)) = (&self.backend, self.docker.as_deref())
            && url == NO_CONTROLLER_SENTINEL
        {
            return Self {
                name: self.name.clone(),
                backend: Backend::Docker {
                    url: docker_url.to_string(),
                },
                docker: None,
            };
        }
        self.clone()
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
                    || "no default context set; run `isd context create <name> --ssh <target>` first",
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
    let mut parsed: CredentialsFile = toml::from_str(&text)
        .with_context(|| format!("parsing credentials at {}", path.display()))?;
    // Track D: rewrite NO_CONTROLLER_SENTINEL + docker shortcut entries into
    // first-class Backend::Docker so the rest of the code sees the modern
    // shape. On-disk state stays as-is until the next `save` (e.g. via
    // `context import`, `context use`, or `context rm`).
    for ctx in parsed.contexts.iter_mut() {
        *ctx = ctx.migrate_legacy_sentinel();
    }
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

    fn http_entry(name: &str) -> ContextEntry {
        ContextEntry {
            name: name.into(),
            backend: Backend::Http {
                url: "http://127.0.0.1:9418".into(),
            },
            docker: None,
        }
    }

    fn ssh_entry(name: &str, target: &str) -> ContextEntry {
        ContextEntry {
            name: name.into(),
            backend: Backend::Ssh {
                target: target.into(),
                dashboard_port: 9418,
            },
            docker: None,
        }
    }

    #[test]
    fn round_trip_preserves_http_and_ssh_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let mut file = CredentialsFile::default();
        file.upsert(http_entry("local"));
        file.upsert(ssh_entry("lausanne", "dirdmaster@10.17.0.125"));
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, file);
    }

    #[test]
    fn legacy_controller_url_migrates_to_http_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        // Hand-write the old TOML shape to simulate a pre-redesign file.
        let legacy = r#"
default_context = "default"

[[contexts]]
name = "default"
controller_url = "http://127.0.0.1:9418"
token = "abc"
ca_fingerprint_sha256 = ""
"#;
        std::fs::write(&path, legacy).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.contexts[0].name, "default");
        match &loaded.contexts[0].backend {
            Backend::Http { url } => assert_eq!(url, "http://127.0.0.1:9418"),
            other => panic!("expected Http backend, got {other:?}"),
        }
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
        file.upsert(http_entry("home"));
        let resolved = file.default_or_named(None).unwrap();
        assert_eq!(resolved.name, "home");
        let by_name = file.default_or_named(Some("home")).unwrap();
        assert_eq!(by_name.name, "home");
        assert!(file.default_or_named(Some("missing")).is_err());
    }

    #[test]
    fn upsert_only_promotes_default_when_unset() {
        let mut file = CredentialsFile::default();
        file.upsert(http_entry("home"));
        assert_eq!(file.default_context.as_deref(), Some("home"));
        file.upsert(ssh_entry("work", "user@work"));
        // Default stays "home" because it was already set.
        assert_eq!(file.default_context.as_deref(), Some("home"));
        file.set_default("work").unwrap();
        assert_eq!(file.default_context.as_deref(), Some("work"));
    }

    #[test]
    fn remove_clears_default_when_removing_the_default() {
        let mut file = CredentialsFile::default();
        file.upsert(http_entry("home"));
        file.upsert(ssh_entry("work", "user@work"));
        file.set_default("work").unwrap();
        assert!(file.remove("work"));
        assert_eq!(file.default_context, None);
        assert!(!file.remove("nonexistent"));
    }

    #[test]
    fn ssh_dashboard_port_defaults_to_9418_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let raw = r#"
default_context = "lausanne"

[[contexts]]
name = "lausanne"
kind = "ssh"
target = "dirdmaster@10.17.0.125"
"#;
        std::fs::write(&path, raw).unwrap();
        let loaded = load(&path).unwrap();
        match &loaded.contexts[0].backend {
            Backend::Ssh { dashboard_port, .. } => assert_eq!(*dashboard_port, 9418),
            other => panic!("expected Ssh backend, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn save_writes_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        let mut file = CredentialsFile::default();
        file.upsert(http_entry("home"));
        save(&path, &file).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn context_entry_with_docker_field_roundtrips() {
        let entry = ContextEntry {
            name: "lausanne".into(),
            backend: Backend::Ssh {
                target: "dirdmaster@10.17.0.125".into(),
                dashboard_port: 9418,
            },
            docker: Some("ssh://dirdmaster@10.17.0.125".into()),
        };
        let mut file = CredentialsFile::default();
        file.upsert(entry.clone());
        let toml = toml::to_string(&file).expect("encode");
        assert!(
            toml.contains("docker = \"ssh://dirdmaster@10.17.0.125\""),
            "docker field should serialize to TOML: {toml}"
        );
        let back: CredentialsFile = toml::from_str(&toml).expect("decode");
        assert_eq!(back.contexts[0].docker, entry.docker);
    }

    #[test]
    fn context_entry_without_docker_field_does_not_serialize_it() {
        let entry = http_entry("plain");
        let mut file = CredentialsFile::default();
        file.upsert(entry);
        let toml = toml::to_string(&file).expect("encode");
        assert!(
            !toml.contains("docker ="),
            "docker key should be omitted when None: {toml}"
        );
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
            other => panic!("expected Docker, got {other:?}"),
        }
        // Round-trip.
        let toml_out = toml::to_string(&file).unwrap();
        assert!(toml_out.contains("kind = \"docker\""));
        assert!(toml_out.contains("url = \"ssh://dirdmaster@10.17.0.125\""));
    }

    #[test]
    fn legacy_no_controller_sentinel_migrates_to_docker_on_load() {
        use crate::context::NO_CONTROLLER_SENTINEL;
        let toml_in = format!(
            r#"
default_context = "lausanne-direct"

[[contexts]]
name = "lausanne-direct"
kind = "http"
url = "{}"
docker = "ssh://dirdmaster@10.17.0.125"
"#,
            NO_CONTROLLER_SENTINEL
        );
        let file: CredentialsFile = toml::from_str(&toml_in).unwrap();
        // The HTTP-sentinel + docker shortcut is rewritten to Docker on load.
        let migrated = file.contexts[0].migrate_legacy_sentinel();
        match &migrated.backend {
            Backend::Docker { url } => assert_eq!(url, "ssh://dirdmaster@10.17.0.125"),
            other => panic!("expected migrated Docker, got {other:?}"),
        }
        assert!(
            migrated.docker.is_none(),
            "docker field cleared post-migration"
        );
    }
}
