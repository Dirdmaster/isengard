//! Local store of non-Isengard servers the operator has onboarded via
//! `isd ssh trust`.
//!
//! Lives at `~/.config/isd/trusted_hosts.toml`. Records the hostname,
//! when the operator trusted it, the fingerprint of the CA pubkey that
//! got installed (so a CA swap is visible), and optional SSH user/port
//! overrides for the dial path. Feeds the picker added in PR 4.
//!
//! The on-disk format is a TOML array of tables under a single `[[host]]`
//! key:
//!
//! ```toml
//! [[host]]
//! hostname = "edge-2.hetzner"
//! trusted_at = "2026-05-21T20:00:00Z"
//! ca_fingerprint = "SHA256:abc..."
//! # optional:
//! # ssh_user = "ubuntu"
//! # ssh_port = 2222
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level shape of `~/.config/isd/trusted_hosts.toml`. The TOML
/// array key is `host` (singular) so the file reads naturally; the
/// in-memory field stays `entries` for clarity.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustedHostsFile {
    /// One entry per onboarded server. Order is preserved on round-trip
    /// (the operator's order of bootstrap, not sorted): keeps the file
    /// human-readable as a log.
    #[serde(default, rename = "host")]
    pub entries: Vec<TrustedHost>,
}

/// A single non-Isengard host the operator has trusted via the bastion
/// bootstrap verb. `ssh_user` and `ssh_port` are optional; when unset
/// the dial path falls back to `$USER` and port 22.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedHost {
    /// SSH-reachable hostname (as the operator typed it into `isd ssh
    /// trust`).
    pub hostname: String,
    /// UTC ISO8601 timestamp of when the trust was recorded.
    pub trusted_at: String,
    /// `SHA256:...` fingerprint of the CA pubkey that was installed
    /// remotely. Surfaces in PR 4's picker so a future CA rotation is
    /// visible to the operator.
    pub ca_fingerprint: String,
    /// Override the SSH user used by the picker when dialing this host.
    /// `None` means fall back to `$USER` (the default principal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_user: Option<String>,
    /// Override the SSH port used by the picker. `None` means port 22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
}

impl TrustedHostsFile {
    /// Canonical on-disk location: `~/.config/isd/trusted_hosts.toml`.
    /// Errors only when the home dir cannot be resolved (extremely
    /// minimal containers, missing `$HOME` and `getpwuid` both failing).
    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("home dir not found")?;
        Ok(home.join(".config").join("isd").join("trusted_hosts.toml"))
    }

    /// Load the trust store from `path`. A missing file returns an
    /// empty store so the caller can `upsert` then `save` without
    /// pre-creating the file.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).context("parsing trusted_hosts.toml"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => {
                Err(anyhow::Error::from(e).context(format!("reading {}", path.display())))
            }
        }
    }

    /// Persist the store to `path`, creating parent directories as
    /// needed. The TOML is pretty-printed so the file stays readable.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let s = toml::to_string_pretty(self).context("serializing trusted_hosts")?;
        std::fs::write(path, s)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Insert `h`, replacing any existing entry that shares its
    /// hostname. Idempotent re-trust of the same host is the common
    /// path (CA rotation, port change), so an in-place replace beats
    /// pushing duplicates.
    pub fn upsert(&mut self, h: TrustedHost) {
        if let Some(slot) = self.entries.iter_mut().find(|e| e.hostname == h.hostname) {
            *slot = h;
        } else {
            self.entries.push(h);
        }
    }

    /// Drop every entry whose hostname matches `hostname`. Used by
    /// `isd ssh untrust`. No-op when the hostname is not present (the
    /// caller checks `entries.len()` to detect that case).
    pub fn remove(&mut self, hostname: &str) {
        self.entries.retain(|e| e.hostname != hostname);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_returns_empty_when_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trusted_hosts.toml");
        let f = TrustedHostsFile::load(&path).unwrap();
        assert!(f.entries.is_empty());
    }

    #[test]
    fn add_then_save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trusted_hosts.toml");
        let mut f = TrustedHostsFile::load(&path).unwrap();
        f.upsert(TrustedHost {
            hostname: "edge-2.hetzner".into(),
            trusted_at: "2026-05-21T20:00:00Z".into(),
            ca_fingerprint: "SHA256:abc".into(),
            ssh_user: None,
            ssh_port: None,
        });
        f.save(&path).unwrap();
        let g = TrustedHostsFile::load(&path).unwrap();
        assert_eq!(g.entries.len(), 1);
        assert_eq!(g.entries[0].hostname, "edge-2.hetzner");
        assert_eq!(g.entries[0].trusted_at, "2026-05-21T20:00:00Z");
        assert_eq!(g.entries[0].ca_fingerprint, "SHA256:abc");
        assert!(g.entries[0].ssh_user.is_none());
        assert!(g.entries[0].ssh_port.is_none());
    }

    #[test]
    fn upsert_replaces_existing_by_hostname() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trusted_hosts.toml");
        let mut f = TrustedHostsFile::load(&path).unwrap();
        f.upsert(TrustedHost {
            hostname: "a".into(),
            trusted_at: "t1".into(),
            ca_fingerprint: "fp1".into(),
            ssh_user: None,
            ssh_port: None,
        });
        f.upsert(TrustedHost {
            hostname: "a".into(),
            trusted_at: "t2".into(),
            ca_fingerprint: "fp2".into(),
            ssh_user: Some("ubuntu".into()),
            ssh_port: Some(2222),
        });
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].trusted_at, "t2");
        assert_eq!(f.entries[0].ca_fingerprint, "fp2");
        assert_eq!(f.entries[0].ssh_user.as_deref(), Some("ubuntu"));
        assert_eq!(f.entries[0].ssh_port, Some(2222));
    }

    #[test]
    fn remove_deletes_by_hostname() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trusted_hosts.toml");
        let mut f = TrustedHostsFile::load(&path).unwrap();
        f.upsert(TrustedHost {
            hostname: "a".into(),
            trusted_at: "t".into(),
            ca_fingerprint: "fp".into(),
            ssh_user: None,
            ssh_port: None,
        });
        f.upsert(TrustedHost {
            hostname: "b".into(),
            trusted_at: "t".into(),
            ca_fingerprint: "fp".into(),
            ssh_user: None,
            ssh_port: None,
        });
        f.remove("a");
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].hostname, "b");
    }

    #[test]
    fn remove_missing_hostname_is_noop() {
        let mut f = TrustedHostsFile::default();
        f.upsert(TrustedHost {
            hostname: "a".into(),
            trusted_at: "t".into(),
            ca_fingerprint: "fp".into(),
            ssh_user: None,
            ssh_port: None,
        });
        f.remove("does-not-exist");
        assert_eq!(f.entries.len(), 1);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested").join("isd");
        let path = nested.join("trusted_hosts.toml");
        let f = TrustedHostsFile::default();
        f.save(&path).unwrap();
        assert!(path.exists());
    }
}
