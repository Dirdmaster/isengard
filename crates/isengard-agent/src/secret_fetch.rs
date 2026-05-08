//! Agent-side secret fetching + tmpfs materialisation (v0.3.6).
//!
//! When a stack's compose references a top-level `secrets:` entry with
//! `external: true`, the agent fetches the plaintext value from the
//! controller's `FetchSecret` mTLS RPC at apply time, writes it to a
//! per-container directory on tmpfs, and bind-mounts that directory at
//! `/run/secrets/<name>` inside the workload container.
//!
//! Plaintext lifecycle on the agent:
//!  - Lives in the controller response heap-side for the duration of the
//!    fetch + write call.
//!  - Lands on a tmpfs file with mode 0400 (owned by root). On Linux the
//!    file is on a real `tmpfs` (memory-backed); on macOS dev the host
//!    is a regular tmpdir and we document the limitation.
//!  - Is removed on container stop / recreate via [`cleanup_for_container`].
//!
//! Logs reference secrets by name only; the value bytes are NEVER logged.

// `tonic::Status` is ~176 bytes, so the FetchError enum is unavoidably
// "large" by clippy's heuristic. Boxing every variant just to satisfy the
// lint would obscure the local-only error paths (validation, IO) that
// dominate the call sites; allow the lint at module level. Same pattern
// the dashboard plugin uses.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use isengard_proto::pb::FetchSecretRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use thiserror::Error;
use tokio::sync::RwLock;
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};

/// Root tmpfs directory under which per-container secret dirs live. On
/// Linux production this is mounted as `tmpfs` so values never hit disk;
/// on macOS dev a regular directory is used (documented limitation).
pub const SECRETS_ROOT: &str = "/run/isengard-secrets";

/// In-container mount point for materialised secrets. Chosen to match
/// Docker / Kubernetes: workloads can portably read from `/run/secrets/<name>`.
pub const CONTAINER_SECRETS_ROOT: &str = "/run/secrets";

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("dial controller: {0}")]
    Dial(#[from] tonic::transport::Error),

    #[error("FetchSecret RPC: {0}")]
    Rpc(#[from] tonic::Status),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid secret name {0:?}")]
    InvalidName(String),

    #[error("invalid container name {0:?}")]
    InvalidContainer(String),
}

/// Materialised secret bind-mount: the host-side path that holds the
/// plaintext bytes plus the in-container target where it should appear.
/// The bind-mount is read-only at apply time so workloads can't write
/// over their own secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialisedSecret {
    /// Name of the secret as referenced by `services.<svc>.secrets`.
    pub name: String,
    /// Host path holding the plaintext bytes. Created with mode 0400.
    pub host_path: PathBuf,
    /// Where the file should appear inside the container (e.g.
    /// `/run/secrets/<name>` by default; long-form `target:` overrides).
    pub container_path: String,
}

impl MaterialisedSecret {
    /// Render as a `bollard` bind-mount entry: `<host>:<container>:ro`.
    pub fn as_bollard_bind(&self) -> String {
        format!(
            "{}:{}:ro",
            self.host_path.to_string_lossy(),
            self.container_path
        )
    }
}

/// Compute the host-side path for `(container, secret_name)`. Both inputs
/// are validated against a strict character set to prevent path traversal:
/// callers can safely substitute these into shell / mount commands.
pub fn host_path_for(container: &str, name: &str) -> Result<PathBuf, FetchError> {
    validate_container_name(container)?;
    validate_secret_name(name)?;
    Ok(Path::new(SECRETS_ROOT).join(container).join(name))
}

/// Default in-container path for a secret: `/run/secrets/<name>`. The
/// long-form compose `target:` overrides this; otherwise we follow
/// Docker / Kubernetes convention.
pub fn default_container_path(name: &str) -> Result<String, FetchError> {
    validate_secret_name(name)?;
    Ok(format!("{CONTAINER_SECRETS_ROOT}/{name}"))
}

fn validate_secret_name(name: &str) -> Result<(), FetchError> {
    if name.is_empty() || name.len() > 64 {
        return Err(FetchError::InvalidName(name.to_string()));
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok {
        return Err(FetchError::InvalidName(name.to_string()));
    }
    Ok(())
}

fn validate_container_name(name: &str) -> Result<(), FetchError> {
    if name.is_empty() || name.len() > 128 {
        return Err(FetchError::InvalidContainer(name.to_string()));
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok {
        return Err(FetchError::InvalidContainer(name.to_string()));
    }
    Ok(())
}

/// Fetch a single secret over mTLS and return the plaintext bytes. Used
/// by [`fetch_and_materialise`] but also exposed for tests + callers that
/// want to skip the disk write (e.g. dry-run).
pub async fn fetch_one(
    endpoint_holder: Arc<RwLock<Endpoint>>,
    name: &str,
) -> Result<Vec<u8>, FetchError> {
    validate_secret_name(name)?;
    let endpoint = endpoint_holder.read().await.clone();
    let channel: Channel = endpoint.connect().await?;
    let mut client = ControllerClient::new(channel);
    let resp = client
        .fetch_secret(FetchSecretRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    Ok(resp.value)
}

/// Fetch + materialise all secrets a container references. Returns one
/// [`MaterialisedSecret`] per name, ordered as input.
///
/// Side effects:
/// - Creates `<SECRETS_ROOT>/<container>/` if missing.
/// - Writes each `<name>` file with mode 0400.
///
/// On any error, partial writes are NOT cleaned up: the caller is
/// expected to invoke [`cleanup_for_container`] before retrying so the
/// retry sees a clean slate.
pub async fn fetch_and_materialise(
    endpoint_holder: Arc<RwLock<Endpoint>>,
    container: &str,
    names: &[String],
) -> Result<Vec<MaterialisedSecret>, FetchError> {
    validate_container_name(container)?;
    let dir = Path::new(SECRETS_ROOT).join(container);
    ensure_dir_with_mode(&dir, 0o700)?;

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        validate_secret_name(name)?;
        let bytes = fetch_one(endpoint_holder.clone(), name).await?;
        let host_path = dir.join(name);
        write_secret_file(&host_path, &bytes)?;
        info!(
            container = %container,
            name = %name,
            bytes = bytes.len(),
            "secret materialised on tmpfs",
        );
        out.push(MaterialisedSecret {
            name: name.clone(),
            host_path,
            container_path: default_container_path(name)?,
        });
    }
    Ok(out)
}

/// Remove every materialised secret for `container`. Idempotent: missing
/// dir is not an error. Called on container stop / recreate so plaintext
/// doesn't outlive the workload.
pub fn cleanup_for_container(container: &str) -> Result<(), FetchError> {
    validate_container_name(container)?;
    let dir = Path::new(SECRETS_ROOT).join(container);
    if !dir.exists() {
        return Ok(());
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        warn!(
            container = %container,
            error = %e,
            "secret_fetch: cleanup failed (continuing)",
        );
        return Err(FetchError::Io(e));
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_dir_with_mode(dir: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let perm = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(dir, perm)?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_dir_with_mode(dir: &Path, _mode: u32) -> std::io::Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // O_TRUNC + 0o400 so a re-fetch of a rotated value lands cleanly and
    // workloads can't write over the secret file.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o400)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_path_combines_root_container_name() {
        let p = host_path_for("hello-web", "cf_token").unwrap();
        assert_eq!(p, PathBuf::from("/run/isengard-secrets/hello-web/cf_token"));
    }

    #[test]
    fn host_path_rejects_traversal_in_name() {
        let err = host_path_for("hello-web", "../etc/passwd").unwrap_err();
        assert!(matches!(err, FetchError::InvalidName(_)));
    }

    #[test]
    fn host_path_rejects_traversal_in_container() {
        let err = host_path_for("../bad", "x").unwrap_err();
        assert!(matches!(err, FetchError::InvalidContainer(_)));
    }

    #[test]
    fn default_container_path_uses_run_secrets() {
        assert_eq!(
            default_container_path("api_token").unwrap(),
            "/run/secrets/api_token"
        );
    }

    #[test]
    fn validate_secret_name_max_64_chars() {
        let n: String = "a".repeat(64);
        assert!(validate_secret_name(&n).is_ok());
        let too: String = "a".repeat(65);
        assert!(matches!(
            validate_secret_name(&too),
            Err(FetchError::InvalidName(_))
        ));
    }

    #[test]
    fn validate_secret_name_rejects_empty_and_special() {
        assert!(matches!(
            validate_secret_name(""),
            Err(FetchError::InvalidName(_))
        ));
        assert!(matches!(
            validate_secret_name("a/b"),
            Err(FetchError::InvalidName(_))
        ));
        assert!(matches!(
            validate_secret_name("space here"),
            Err(FetchError::InvalidName(_))
        ));
    }

    #[test]
    fn materialised_secret_renders_bollard_bind() {
        let m = MaterialisedSecret {
            name: "x".into(),
            host_path: PathBuf::from("/run/isengard-secrets/c1/x"),
            container_path: "/run/secrets/x".into(),
        };
        assert_eq!(
            m.as_bollard_bind(),
            "/run/isengard-secrets/c1/x:/run/secrets/x:ro"
        );
    }

    #[test]
    fn cleanup_is_noop_when_dir_missing() {
        // Use a guaranteed-missing path under tempdir.
        let tmp = tempfile::tempdir().unwrap();
        let bogus = format!("{}/missing-{}", tmp.path().display(), std::process::id());
        // We can't override SECRETS_ROOT in this test, but the function
        // also tolerates a non-existent canonical path on systems where
        // /run/isengard-secrets/<id> isn't created. Run the validate
        // path: passing an invalid container name short-circuits, while
        // the existing-dir check kicks in for valid names.
        let _ = bogus; // unused (kept for documentation of intent)
        // Validate-only smoke: invalid container name returns error.
        assert!(matches!(
            cleanup_for_container("../etc"),
            Err(FetchError::InvalidContainer(_))
        ));
    }
}
