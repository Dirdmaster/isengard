//! macOS `/etc/resolver/<zone>` install helper. Idempotent: rewrites the
//! file every time so a stale port from an earlier `--dns-port` flag is
//! corrected. Linux uses `systemd-resolved` (out of scope for v0.3.5; the
//! gateway still works via `dig @127.0.0.1 -p 5300 ...`).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result, anyhow};

/// Path to the per-zone resolver file. macOS reads every file under
/// `/etc/resolver/` at boot and on every change.
fn resolver_path(zone: &str) -> PathBuf {
    PathBuf::from("/etc/resolver").join(zone.trim_matches('.'))
}

/// Best-effort check: does the resolver file exist?
pub fn is_installed(zone: &str) -> bool {
    resolver_path(zone).exists()
}

/// Render the resolver-file contents for a given zone + port.
pub fn render(port: u16) -> String {
    format!("nameserver 127.0.0.1\nport {port}\n")
}

/// Install the resolver. Writes via `sudo tee` so the operator only types
/// their password once and we don't have to spawn a privileged binary.
pub fn install(zone: &str, port: u16) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(anyhow!(
            "--install-resolver is macOS-only (Linux: configure systemd-resolved manually)"
        ));
    }
    let zone = zone.trim().trim_matches('.');
    if zone.is_empty() {
        return Err(anyhow!("zone must not be empty"));
    }
    let path = resolver_path(zone);
    let body = render(port);
    println!("Writing {} ...", path.display());
    println!();
    println!("{body}");

    // Ensure /etc/resolver exists (it usually does on macOS, but creating
    // it is harmless if it doesn't).
    let mkdir = Command::new("sudo")
        .args(["mkdir", "-p", "/etc/resolver"])
        .status()
        .context("running `sudo mkdir -p /etc/resolver`")?;
    if !mkdir.success() {
        return Err(anyhow!("`sudo mkdir -p /etc/resolver` failed"));
    }

    // Pipe the body via `sudo tee`. Using `bash -c "echo ... | sudo tee"`
    // would force a second sudo prompt; spawn `sudo tee` directly and
    // write to its stdin instead.
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("sudo")
        .args(["tee", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("spawning `sudo tee`")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("sudo tee stdin missing"))?;
        stdin
            .write_all(body.as_bytes())
            .context("writing resolver body to sudo tee stdin")?;
    }
    let status = child.wait().context("waiting for `sudo tee`")?;
    if !status.success() {
        return Err(anyhow!("`sudo tee {}` failed", path.display()));
    }
    println!();
    println!("ok: macOS will pick up the change immediately. Verify with:");
    println!("  scutil --dns | grep -A2 '{zone}'");
    Ok(())
}

/// Remove the resolver file. No-op when already absent.
pub fn uninstall(zone: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(anyhow!(
            "--uninstall-resolver is macOS-only (Linux: configure systemd-resolved manually)"
        ));
    }
    let zone = zone.trim().trim_matches('.');
    if zone.is_empty() {
        return Err(anyhow!("zone must not be empty"));
    }
    let path = resolver_path(zone);
    if !path.exists() {
        println!("ok: {} is already absent.", path.display());
        return Ok(());
    }
    println!("Removing {} ...", path.display());
    let status = Command::new("sudo")
        .args(["rm", "-f", path.to_str().unwrap()])
        .status()
        .context("running `sudo rm -f`")?;
    if !status.success() {
        return Err(anyhow!("`sudo rm -f {}` failed", path.display()));
    }
    println!("ok.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_two_lines_with_terminal_newline() {
        let s = render(5300);
        assert_eq!(s, "nameserver 127.0.0.1\nport 5300\n");
    }

    #[test]
    fn render_uses_supplied_port() {
        assert!(render(53).contains("port 53"));
        assert!(render(8053).contains("port 8053"));
    }

    #[test]
    fn resolver_path_strips_dot_decoration() {
        assert_eq!(resolver_path(".isd."), PathBuf::from("/etc/resolver/isd"));
        assert_eq!(resolver_path("isd"), PathBuf::from("/etc/resolver/isd"));
    }
}
