//! Filesystem-backed TLS cert/key storage.
//!
//! Layout: `<root>/<hostname>.crt` and `<root>/<hostname>.key`. The `.key`
//! file is `chmod 0600` on Unix so only the agent process (and root) can
//! read it. The `.crt` keeps the umask default — it's a public cert.
//!
//! Async throughout: callers (ACME issuance, cert_store warm-load) live in
//! the tokio runtime, and we want to avoid blocking the executor on disk IO.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;

/// A cert/key pair, both PEM-encoded.
#[derive(Debug, Clone)]
pub struct CertFiles {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Filesystem store rooted at a single directory. Cheap to clone (just a
/// `PathBuf`); safe to share across tasks.
#[derive(Debug, Clone)]
pub struct TlsStorage {
    root: PathBuf,
}

impl TlsStorage {
    /// Create a new store rooted at `root`. The directory itself is created
    /// lazily on first `write`; `read` of a missing host returns an error.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Path to the cert file for `hostname`.
    fn cert_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{hostname}.crt"))
    }

    /// Path to the key file for `hostname`.
    fn key_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{hostname}.key"))
    }

    /// Write the cert and key for `hostname`. Creates the root directory if
    /// it doesn't exist. On Unix, the key file is chmod'd to `0600`
    /// (owner-read/write only) after the bytes are flushed.
    pub async fn write(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        ensure_dir(&self.root).await?;

        let cert_path = self.cert_path(hostname);
        let key_path = self.key_path(hostname);

        fs::write(&cert_path, cert_pem)
            .await
            .with_context(|| format!("write cert {} for {hostname}", cert_path.display()))?;

        fs::write(&key_path, key_pem)
            .await
            .with_context(|| format!("write key {} for {hostname}", key_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .await
                .with_context(|| format!("chmod 0600 key {} for {hostname}", key_path.display()))?;
        }

        Ok(())
    }

    /// Read the cert/key pair for `hostname`. Returns an error (with the
    /// hostname in the message) if either file is missing or unreadable.
    pub async fn read(&self, hostname: &str) -> Result<CertFiles> {
        let cert_path = self.cert_path(hostname);
        let key_path = self.key_path(hostname);

        let cert_pem = fs::read_to_string(&cert_path)
            .await
            .with_context(|| format!("read cert for {hostname} at {}", cert_path.display()))?;

        let key_pem = fs::read_to_string(&key_path)
            .await
            .with_context(|| format!("read key for {hostname} at {}", key_path.display()))?;

        Ok(CertFiles { cert_pem, key_pem })
    }

    /// Best-effort delete of both cert and key for `hostname`. Missing files
    /// are not errors; other IO failures are.
    pub async fn delete(&self, hostname: &str) -> Result<()> {
        for path in [self.cert_path(hostname), self.key_path(hostname)] {
            match fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("delete {} for {hostname}", path.display()));
                }
            }
        }
        Ok(())
    }
}

async fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .await
        .with_context(|| format!("create tls storage dir {}", path.display()))
}
