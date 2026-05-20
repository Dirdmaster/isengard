//! Agent-side cert bundle storage. Lives in `state_dir/certs/`.
//! Atomic writes via `.new` + rename; key file gets chmod 600.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// On-disk cert bundle the agent uses for mTLS dials. All three PEMs
/// live in `<state_dir>/certs/`. Mode 0600 on `agent.key`; 0644 on the
/// rest.
#[derive(Debug, Clone)]
pub struct CertBundle {
    /// Controller CA root PEM. The trust anchor every agent RPC validates
    /// against.
    pub ca_pem: String,
    /// Agent's signed leaf cert PEM. CN = agent's `HostId`.
    pub cert_pem: String,
    /// Agent's private key PEM, paired with `cert_pem`.
    pub key_pem: String,
}

/// Path to the bundle directory under `state_dir`.
pub fn cert_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("certs")
}

/// True when all three bundle files exist on disk.
pub fn exists(state_dir: &Path) -> bool {
    let d = cert_dir(state_dir);
    d.join("ca.pem").exists() && d.join("agent.crt").exists() && d.join("agent.key").exists()
}

/// Load the bundle from disk.
///
/// # Errors
///
/// Returns an error when any of the three files is missing or unreadable.
pub fn load(state_dir: &Path) -> Result<CertBundle> {
    let d = cert_dir(state_dir);
    Ok(CertBundle {
        ca_pem: std::fs::read_to_string(d.join("ca.pem")).context("read ca.pem")?,
        cert_pem: std::fs::read_to_string(d.join("agent.crt")).context("read agent.crt")?,
        key_pem: std::fs::read_to_string(d.join("agent.key")).context("read agent.key")?,
    })
}

/// Atomically persist the bundle to disk. Creates the directory if
/// missing; chmods `agent.key` to 0600.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or any of the
/// three file writes fails.
pub fn save(state_dir: &Path, bundle: &CertBundle) -> Result<()> {
    let d = cert_dir(state_dir);
    std::fs::create_dir_all(&d).context("mkdir certs/")?;

    write_atomic(&d.join("ca.pem"), bundle.ca_pem.as_bytes(), 0o644)?;
    write_atomic(&d.join("agent.crt"), bundle.cert_pem.as_bytes(), 0o644)?;
    write_atomic(&d.join("agent.key"), bundle.key_pem.as_bytes(), 0o600)?;
    Ok(())
}

/// Internal helper: write atomic.
fn write_atomic(target: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // Generate the .new sibling path correctly (target may have or not have an extension)
    let mut tmp = target.to_path_buf();
    tmp.as_mut_os_string().push(".new");
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {tmp:?}"))?;
        f.write_all(data)?;
        f.sync_all()?;
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(mode);
        f.set_permissions(perms)?;
    }
    std::fs::rename(&tmp, target).with_context(|| format!("rename {tmp:?} -> {target:?}"))?;
    Ok(())
}
