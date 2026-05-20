//! Backup-specific credentials store at `~/.config/isd/backup.toml`.
//!
//! Operator-side, mode 0600 on unix. Per-context passphrase + S3 endpoint /
//! creds. Distinct file from `credentials.toml` (controller-context store) so
//! the backup passphrase + S3 creds can be backed up, escrowed, or restored
//! independently of the controller-context file.
//!
//! Resolution chain for the encryption passphrase (in order):
//!   1. `--passphrase-file <path>` CLI flag
//!   2. `ISENGARD_BACKUP_PASSPHRASE` env var
//!   3. `contexts.<ctx>.passphrase` in this file
//!   4. Interactive prompt with confirm + store-after
//!
//! The `default_path` + `resolve_passphrase` helpers are wired by
//! `backup_cmd` / `restore_cmd` in the next tasks of this phase. They are
//! exposed here so the unit-test boundary stays tight to the credentials
//! file format, separate from the command surface.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// On-disk shape of `~/.config/isd/backup.toml`. Top-level table maps
/// context name to its per-context creds entry.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BackupCredentialsFile {
    /// One entry per context. Empty by default; populated lazily on
    /// first interactive passphrase prompt.
    #[serde(default)]
    pub contexts: std::collections::BTreeMap<String, ContextBackupCreds>,
}

/// Per-context backup credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextBackupCreds {
    /// age passphrase. Stored as-is; file mode 0600 on unix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// S3 endpoint + credentials for the `--to s3://...` destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Creds>,
}

/// S3-API credentials. Works against AWS, R2, MinIO, B2, Wasabi via
/// `endpoint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Creds {
    /// API endpoint (`https://s3.amazonaws.com`, `https://<acct>.r2.cloudflarestorage.com`, ...).
    pub endpoint: String,
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Region; some implementations (R2) want `auto`.
    pub region: String,
}

/// Canonical path: `~/.config/isd/backup.toml`.
///
/// # Errors
///
/// Returns `Err` when the home directory cannot be resolved.
pub fn default_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("home dir not found")?;
    Ok(home.join(".config").join("isd").join("backup.toml"))
}

/// Read the credentials file at `path`. Returns the default (empty)
/// value when the file does not exist.
///
/// # Errors
///
/// Returns `Err` on read or TOML parse errors. A missing file is not
/// an error.
pub fn load(path: &Path) -> Result<BackupCredentialsFile> {
    if !path.exists() {
        return Ok(BackupCredentialsFile::default());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write the credentials file, creating the parent directory and
/// enforcing mode 0600 on unix.
///
/// # Errors
///
/// Returns `Err` on serialization, directory creation, write, or
/// permissions-set failure.
pub fn save(path: &Path, file: &BackupCredentialsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(file).context("serializing backup creds")?;
    std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("setting backup.toml mode 0600")?;
    }
    Ok(())
}

/// Resolve a backup passphrase for `context_name`, walking the 4-tier
/// chain (flag, env, stored, interactive). On tier 4 the prompted
/// value is stored back to [`default_path`] so subsequent invocations
/// skip the prompt.
///
/// # Errors
///
/// Returns `Err` when the flag's file is unreadable, the interactive
/// prompt fails, the operator submits an empty passphrase, or the
/// confirmation doesn't match. Empty env vars fall through to the
/// next tier instead of erroring.
pub fn resolve_passphrase(
    context_name: &str,
    file: &BackupCredentialsFile,
    flag_passphrase_file: Option<&Path>,
) -> Result<String> {
    // 1. --passphrase-file <path>
    if let Some(path) = flag_passphrase_file {
        return std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .with_context(|| format!("reading {}", path.display()));
    }
    // 2. ISENGARD_BACKUP_PASSPHRASE env var
    if let Ok(v) = std::env::var("ISENGARD_BACKUP_PASSPHRASE") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    // 3. Stored in backup.toml
    if let Some(creds) = file.contexts.get(context_name) {
        if let Some(p) = &creds.passphrase {
            return Ok(p.clone());
        }
    }
    // 4. Interactive prompt + store-after
    eprintln!("isd backup: No backup passphrase set for context {context_name:?}.");
    eprintln!("isd backup: Pick a passphrase. It will be stored in `~/.config/isd/backup.toml`");
    eprintln!("isd backup: (mode 0600). LOSE IT = LOSE YOUR BACKUPS. Escrow it.");
    let p = rpassword::prompt_password("isd backup: passphrase: ")
        .context("reading passphrase from terminal")?;
    if p.is_empty() {
        anyhow::bail!("empty passphrase");
    }
    let p2 = rpassword::prompt_password("isd backup: passphrase (confirm): ")
        .context("reading passphrase confirmation from terminal")?;
    if p != p2 {
        anyhow::bail!("passphrases do not match");
    }
    // Store the freshly-prompted passphrase under this context.
    let path = default_path()?;
    let mut f = load(&path)?;
    f.contexts
        .entry(context_name.into())
        .or_default()
        .passphrase = Some(p.clone());
    save(&path, &f)?;
    eprintln!("isd backup: passphrase stored in {}", path.display());
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_credentials_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("backup.toml");
        let mut file = BackupCredentialsFile::default();
        file.contexts.insert(
            "lausanne".into(),
            ContextBackupCreds {
                passphrase: Some("hunter2".into()),
                s3: Some(S3Creds {
                    endpoint: "https://r2.cf.com".into(),
                    access_key_id: "AKID".into(),
                    secret_access_key: "SECRET".into(),
                    region: "auto".into(),
                }),
            },
        );
        save(&path, &file).unwrap();

        // Mode check on unix: 0600 means u+rw, nothing else.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "backup.toml must be mode 0600, got {mode:o}");
        }

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.contexts.len(), 1);
        let c = &loaded.contexts["lausanne"];
        assert_eq!(c.passphrase.as_deref(), Some("hunter2"));
        assert_eq!(c.s3.as_ref().unwrap().endpoint, "https://r2.cf.com");
        assert_eq!(c.s3.as_ref().unwrap().access_key_id, "AKID");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.toml");
        let file = load(&path).unwrap();
        assert!(file.contexts.is_empty());
    }
}
