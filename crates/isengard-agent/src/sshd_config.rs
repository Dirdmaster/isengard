//! Install the controller's SSH CA pubkey as a `TrustedUserCAKeys`
//! drop-in for the host's sshd.
//!
//! Triggered once per enrollment from the agent main flow. The pubkey
//! lives at `/host/etc/isengard/ssh_ca.pub`; sshd picks it up via the
//! drop-in at `/host/etc/ssh/sshd_config.d/40-isengard-ca.conf`. After
//! the write, the agent issues a HUP-equivalent reload so sshd picks
//! up the new directive without restarting open sessions.
//!
//! Best-effort: a failure here must NOT block enrollment. macOS hosts
//! that don't bind-mount the host filesystem (and don't run sshd
//! inside the agent's view) simply skip the install. Operators can
//! opt out completely via the `ISENGARD_SSH_BASTION_DISABLED`
//! environment variable.

use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, warn};

/// Path where the agent writes the CA pubkey on the host.
///
/// `/host/` is the well-known prefix where the agent's compose mounts
/// the host root. Inside-container it's `/host/etc/...`; from the
/// host's perspective it's `/etc/...`. sshd's drop-in references the
/// host-perspective path.
/// Where the agent writes the CA pubkey, as seen from inside the
/// agent's container (the host's `/etc/isengard/ssh_ca.pub` lives at
/// `/host/etc/isengard/ssh_ca.pub` because the agent compose mounts
/// the host root at `/host`).
const CA_PATH_INSIDE_CONTAINER: &str = "/host/etc/isengard/ssh_ca.pub";
/// Same file's path as sshd sees it from the host's perspective.
/// Referenced inside the drop-in body.
const CA_PATH_FROM_SSHD: &str = "/etc/isengard/ssh_ca.pub";

/// Path of the sshd drop-in that wires the CA into `TrustedUserCAKeys`.
const DROPIN_PATH_INSIDE_CONTAINER: &str = "/host/etc/ssh/sshd_config.d/40-isengard-ca.conf";

/// Environment variable that disables the install entirely. Useful for
/// dev hosts where you don't want a controller CA poking at sshd.
pub const DISABLE_ENV_VAR: &str = "ISENGARD_SSH_BASTION_DISABLED";

/// Errors that can fall out of `install_ssh_ca`. All are surfaced as
/// `warn!` by the caller (enrollment continues regardless).
#[derive(Debug, Error)]
pub enum SshdInstallError {
    /// Failed to create the parent directory before writing the CA
    /// pubkey or the sshd drop-in. Wraps the underlying `io::Error`.
    #[error("create directory {path}: {source}")]
    CreateDir {
        /// Directory whose creation failed.
        path: PathBuf,
        /// Source `io::Error` from `tokio::fs::create_dir_all`.
        #[source]
        source: std::io::Error,
    },
    /// Failed to write the CA pubkey or the sshd drop-in to disk.
    /// Wraps the underlying `io::Error`.
    #[error("write {path}: {source}")]
    Write {
        /// File whose write failed.
        path: PathBuf,
        /// Source `io::Error` from `tokio::fs::write`.
        #[source]
        source: std::io::Error,
    },
    /// The agent's expected `/host` bind-mount of the host filesystem
    /// is absent. Surfaces on macOS hosts without Docker Desktop's
    /// host mount and during unit tests.
    #[error("host filesystem not mounted at /host (skipping sshd install)")]
    HostFsMissing,
}

/// Persist the controller's SSH user-cert authority public key and
/// drop a `sshd_config.d/` entry that tells sshd to trust user certs
/// signed by it.
///
/// Returns immediately with `Ok(false)` when `ISENGARD_SSH_BASTION_DISABLED`
/// is set in the environment or when the host filesystem is not
/// mounted at `/host`. Returns `Ok(true)` after a successful install +
/// reload attempt.
///
/// # Errors
///
/// Surfaces `SshdInstallError` for I/O failures during the directory
/// create or the file writes. The sshd reload is best-effort; its
/// failures are logged via `tracing` but do not propagate.
pub async fn install_ssh_ca(pubkey: &[u8]) -> Result<bool, SshdInstallError> {
    if std::env::var_os(DISABLE_ENV_VAR).is_some() {
        debug!("ssh bastion disabled via {} env var", DISABLE_ENV_VAR);
        return Ok(false);
    }
    if !host_fs_mounted().await {
        return Err(SshdInstallError::HostFsMissing);
    }

    let ca_path = Path::new(CA_PATH_INSIDE_CONTAINER);
    let dropin_path = Path::new(DROPIN_PATH_INSIDE_CONTAINER);

    write_pubkey(ca_path, pubkey).await?;
    write_dropin(dropin_path).await?;

    if let Err(e) = reload_sshd().await {
        warn!(error = %e, "ssh bastion installed but sshd reload failed");
    }

    Ok(true)
}

/// Render the drop-in body. Static apart from the comment lead-in;
/// extracted so tests can assert the exact bytes that hit disk.
pub fn dropin_body() -> String {
    format!(
        "# Managed by isengard-agent. Do not edit; changes are\n\
         # overwritten on every successful enrollment.\n\
         TrustedUserCAKeys {CA_PATH_FROM_SSHD}\n"
    )
}

/// Check whether `/host` is a real bind mount of the host filesystem.
/// On a properly configured agent install it always is; on macOS
/// hosts (no Docker Desktop host mount) or in unit tests it is not.
async fn host_fs_mounted() -> bool {
    fs::metadata("/host/etc").await.is_ok()
}

/// Create `path`'s parent directory if needed and write the CA pubkey
/// bytes verbatim. Verbatim so the on-disk file is the exact
/// `authorized_keys`-format blob the controller returned.
async fn write_pubkey(path: &Path, pubkey: &[u8]) -> Result<(), SshdInstallError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|source| SshdInstallError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    fs::write(path, pubkey)
        .await
        .map_err(|source| SshdInstallError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// Create `path`'s parent directory if needed and write the rendered
/// sshd drop-in body. Body comes from `dropin_body()` so unit tests
/// can pin its exact contents.
async fn write_dropin(path: &Path) -> Result<(), SshdInstallError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|source| SshdInstallError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    fs::write(path, dropin_body().as_bytes())
        .await
        .map_err(|source| SshdInstallError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// Reload sshd on the host. Tries `nsenter -t 1 -m -p systemctl reload
/// ssh` first (systemd hosts); falls back to a SIGHUP to whatever
/// `pgrep -o sshd` returns on the host PID namespace. Both invocations
/// use `nsenter` to reach the host's PID namespace from inside the
/// agent container.
///
/// Returns Ok when EITHER path succeeds; Err only when both fail.
async fn reload_sshd() -> Result<(), anyhow::Error> {
    let systemd = Command::new("nsenter")
        .args(["-t", "1", "-m", "-p", "systemctl", "reload", "ssh"])
        .status()
        .await;
    if let Ok(status) = systemd {
        if status.success() {
            return Ok(());
        }
    }
    let sighup = Command::new("nsenter")
        .args(["-t", "1", "-m", "-p", "pkill", "-HUP", "-o", "sshd"])
        .status()
        .await?;
    if sighup.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "sshd reload via nsenter failed (systemctl + pkill both unsuccessful)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropin_body_pins_managed_comment_and_ca_path() {
        let body = dropin_body();
        assert!(body.starts_with("# Managed by isengard-agent."));
        assert!(body.contains("TrustedUserCAKeys /etc/isengard/ssh_ca.pub"));
        assert!(body.ends_with('\n'));
    }

    #[tokio::test]
    async fn disabled_env_var_short_circuits() {
        unsafe {
            std::env::set_var(DISABLE_ENV_VAR, "1");
        }
        let result = install_ssh_ca(b"dummy").await;
        unsafe {
            std::env::remove_var(DISABLE_ENV_VAR);
        }
        assert!(matches!(result, Ok(false)));
    }
}
