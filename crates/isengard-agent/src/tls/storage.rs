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

    /// Path to the cert file for `hostname`. Wildcard hostnames are encoded
    /// (`*.foo.com` -> `_wildcard_.foo.com`) so the on-disk filename never
    /// contains `*` (which is shell-glob-unsafe even though POSIX allows it).
    fn cert_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{}.crt", encode_for_path(hostname)))
    }

    /// Path to the key file for `hostname`. Same encoding as `cert_path`.
    fn key_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{}.key", encode_for_path(hostname)))
    }

    /// Write the cert and key for `hostname`. Creates the root directory if
    /// it doesn't exist. On Unix, the key file is chmod'd to `0600`
    /// (owner-read/write only) after the bytes are flushed.
    ///
    /// **Atomic via `.new` + rename:** writes to `<host>.crt.new` and
    /// `<host>.key.new` first, chmod's the key tmp, then renames both into
    /// place. A crash between the two renames leaves at most one file
    /// half-installed (cert renamed, key still as `.new`); a subsequent read
    /// fails fast on the missing key rather than serving a mismatched pair.
    /// This is the half-state Pingora's cert callback can't tolerate.
    ///
    /// Validates `hostname` (DNS-label charset, no path-traversal) and the
    /// PEM content (BEGIN markers present) before any disk IO.
    pub async fn write(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        validate_hostname(hostname)?;
        validate_pem(cert_pem, key_pem)?;
        ensure_dir(&self.root).await?;

        let cert_path = self.cert_path(hostname);
        let key_path = self.key_path(hostname);
        let cert_tmp = self.root.join(format!("{hostname}.crt.new"));
        let key_tmp = self.root.join(format!("{hostname}.key.new"));

        fs::write(&cert_tmp, cert_pem)
            .await
            .with_context(|| format!("write cert tmp {} for {hostname}", cert_tmp.display()))?;

        fs::write(&key_tmp, key_pem)
            .await
            .with_context(|| format!("write key tmp {} for {hostname}", key_tmp.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_tmp, std::fs::Permissions::from_mode(0o600))
                .await
                .with_context(|| {
                    format!("chmod 0600 key tmp {} for {hostname}", key_tmp.display())
                })?;
        }

        fs::rename(&cert_tmp, &cert_path).await.with_context(|| {
            format!(
                "rename cert {} -> {} for {hostname}",
                cert_tmp.display(),
                cert_path.display()
            )
        })?;

        fs::rename(&key_tmp, &key_path).await.with_context(|| {
            format!(
                "rename key {} -> {} for {hostname}",
                key_tmp.display(),
                key_path.display()
            )
        })?;

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

/// Reject hostnames that could escape the storage root or break shell tooling.
/// Allows the DNS label charset (ASCII alnum, dot, hyphen). A leading `*.`
/// is permitted for wildcard certs; the remainder is validated as a normal
/// DNS hostname. Refuses empty, path separators, parent-dir traversals, and
/// any other character.
fn validate_hostname(hostname: &str) -> Result<()> {
    if hostname.is_empty() {
        anyhow::bail!("hostname is empty");
    }
    if hostname.contains('/') || hostname.contains('\\') || hostname.contains("..") {
        anyhow::bail!("hostname {hostname:?} contains path-traversal characters");
    }
    let rest = hostname.strip_prefix("*.").unwrap_or(hostname);
    if rest.is_empty() {
        anyhow::bail!("hostname {hostname:?} has nothing after '*.'");
    }
    if rest.contains('*') {
        anyhow::bail!("hostname {hostname:?} contains '*' outside leading wildcard label");
    }
    if let Some(c) = rest
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '.' || *c == '-'))
    {
        anyhow::bail!("hostname {hostname:?} contains invalid character {c:?}");
    }
    Ok(())
}

/// Encode a hostname for use as an on-disk filename. The cache key in
/// `cert_store` keeps the canonical wildcard form (`*.foo.com`) so
/// `lookup_wildcard` can find it; the filename swaps that for a literal
/// `_wildcard_.` prefix to avoid `*` in shell paths. Underscores are
/// rejected by `validate_hostname` so the encoded form can never collide
/// with a real input.
fn encode_for_path(hostname: &str) -> String {
    match hostname.strip_prefix("*.") {
        Some(rest) => format!("_wildcard_.{rest}"),
        None => hostname.to_string(),
    }
}

/// Sanity-check that the PEM blobs at least carry BEGIN markers. Catches
/// caller bugs (passing JSON, or a base64 blob that wasn't PEM-wrapped) at
/// the storage boundary instead of letting Pingora's cert load fail later
/// with an opaque error. Doesn't validate the content cryptographically.
fn validate_pem(cert_pem: &str, key_pem: &str) -> Result<()> {
    if !cert_pem.contains("-----BEGIN CERTIFICATE-----") {
        anyhow::bail!("cert_pem missing '-----BEGIN CERTIFICATE-----' marker");
    }
    // Accept PKCS#8 (`PRIVATE KEY`), traditional RSA (`RSA PRIVATE KEY`),
    // and EC (`EC PRIVATE KEY`) key formats.
    if !key_pem.contains("-----BEGIN ") || !key_pem.contains(" PRIVATE KEY-----") {
        anyhow::bail!("key_pem missing '-----BEGIN ... PRIVATE KEY-----' marker");
    }
    Ok(())
}
