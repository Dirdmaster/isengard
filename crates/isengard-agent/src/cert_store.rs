//! Agent-side cert bundle storage. Lives in `state_dir/certs/`.
//! Atomic writes via `.new` + rename; key file gets chmod 600.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct CertBundle {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn cert_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("certs")
}

pub fn exists(state_dir: &Path) -> bool {
    let d = cert_dir(state_dir);
    d.join("ca.pem").exists() && d.join("agent.crt").exists() && d.join("agent.key").exists()
}

pub fn load(state_dir: &Path) -> Result<CertBundle> {
    let d = cert_dir(state_dir);
    Ok(CertBundle {
        ca_pem: std::fs::read_to_string(d.join("ca.pem")).context("read ca.pem")?,
        cert_pem: std::fs::read_to_string(d.join("agent.crt")).context("read agent.crt")?,
        key_pem: std::fs::read_to_string(d.join("agent.key")).context("read agent.key")?,
    })
}

pub fn save(state_dir: &Path, bundle: &CertBundle) -> Result<()> {
    let d = cert_dir(state_dir);
    std::fs::create_dir_all(&d).context("mkdir certs/")?;

    write_atomic(&d.join("ca.pem"), bundle.ca_pem.as_bytes(), 0o644)?;
    write_atomic(&d.join("agent.crt"), bundle.cert_pem.as_bytes(), 0o644)?;
    write_atomic(&d.join("agent.key"), bundle.key_pem.as_bytes(), 0o600)?;
    Ok(())
}

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
