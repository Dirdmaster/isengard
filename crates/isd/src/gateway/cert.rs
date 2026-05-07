//! Self-signed cert generation for the gateway's HTTPS listener.
//!
//! Caches `cert.pem` + `key.pem` under
//! `~/Library/Application Support/isengard/gateway/` (macOS) /
//! `$XDG_DATA_HOME/isengard/gateway/` (Linux). Reused across runs so the
//! browser's `thisisunsafe` warning only has to be dismissed once.
//!
//! The cert covers `*.<zone>` and `localhost`; that's enough for `dig` /
//! `curl` smoke tests + most browsers. We don't try to install it into the
//! system keychain: that's the operator's call (`mkcert`-style trust install
//! is documented in the PR body but out of scope for v0.3.5).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, SanType,
};

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";

/// Default cert directory. macOS picks `~/Library/Application Support`; Linux
/// picks `$XDG_DATA_HOME` (or `~/.local/share`). Errors only when the OS
/// has no concept of a per-user data dir (e.g. CI without `$HOME`).
pub fn default_cert_dir() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("ISD_GATEWAY_CERT_DIR") {
        return Ok(PathBuf::from(env));
    }
    let base = dirs::data_dir()
        .context("no data directory available (set $XDG_DATA_HOME or ISD_GATEWAY_CERT_DIR)")?;
    Ok(base.join("isengard").join("gateway"))
}

/// Load a cached cert pair from `dir`, or generate a fresh one (and persist
/// it) if any side is missing. Returns the PEM blobs the rustls config wants.
pub fn load_or_generate(dir: &Path, zone: &str) -> Result<(String, String)> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key_pem = std::fs::read_to_string(&key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        if !cert_pem.trim().is_empty() && !key_pem.trim().is_empty() {
            return Ok((cert_pem, key_pem));
        }
    }
    generate_and_persist(dir, zone)
}

fn generate_and_persist(dir: &Path, zone: &str) -> Result<(String, String)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating cert directory {}", dir.display()))?;

    let zone = zone.trim().trim_matches('.').to_ascii_lowercase();
    let key_pair =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating ECDSA P-256 key")?;
    let mut params = CertificateParams::new(vec![]).context("rcgen params")?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("isd gateway *.{zone}"));
    dn.push(DnType::OrganizationName, "Isengard Gateway (dev)");
    params.distinguished_name = dn;
    // SANs: `*.<zone>`, the zone apex itself, and `localhost`. Browsers
    // ignore CN and use the SAN for hostname matching.
    params.subject_alt_names = vec![
        SanType::DnsName(format!("*.{zone}").try_into().context("wildcard SAN")?),
        SanType::DnsName(zone.clone().try_into().context("apex SAN")?),
        SanType::DnsName(
            "localhost"
                .to_string()
                .try_into()
                .context("localhost SAN")?,
        ),
    ];
    // Validity: ten years. Browsers don't enforce a max for self-signed
    // dev certs the way they do for public-trust ones, but a long horizon
    // means the operator doesn't have to regenerate every year.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(365 * 10);

    let cert = params
        .self_signed(&key_pair)
        .context("self-signing gateway cert")?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    std::fs::write(&cert_path, cert_pem.as_bytes())
        .with_context(|| format!("writing {}", cert_path.display()))?;
    std::fs::write(&key_path, key_pem.as_bytes())
        .with_context(|| format!("writing {}", key_path.display()))?;
    set_owner_only_perms(&key_path)?;
    Ok((cert_pem, key_pem))
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_persist_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let (c1, k1) = generate_and_persist(dir.path(), "isd").unwrap();
        assert!(c1.contains("BEGIN CERTIFICATE"));
        assert!(k1.contains("BEGIN PRIVATE KEY") || k1.contains("BEGIN EC PRIVATE KEY"));

        // Second call with the same dir should reuse the cached pair.
        let (c2, k2) = load_or_generate(dir.path(), "isd").unwrap();
        assert_eq!(c1, c2);
        assert_eq!(k1, k2);
    }

    #[test]
    fn missing_cert_triggers_generation() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing in the dir yet -> first call generates.
        let (c1, _) = load_or_generate(dir.path(), "isd").unwrap();
        // Delete just the cert side; load_or_generate should regenerate
        // the whole pair rather than reuse a half-broken cache.
        std::fs::remove_file(dir.path().join(CERT_FILE)).unwrap();
        let (c2, _) = load_or_generate(dir.path(), "isd").unwrap();
        assert_ne!(c1, c2);
    }

    #[test]
    fn key_perms_are_owner_only_on_unix() {
        if !cfg!(unix) {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let _ = generate_and_persist(dir.path(), "isd").unwrap();
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
