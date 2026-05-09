//! Render `/etc/hosts` inside a container's rootfs.
//!
//! Same shape as `resolv.rs`: the runtime calls this BEFORE the
//! child does `pivot_root`, the path lives on the host filesystem,
//! and the child sees the file at `/etc/hosts` post-pivot.
//!
//! The minimal template is what most images expect when libc looks
//! up `localhost` or the container's own hostname:
//!
//! ```text
//! # wisp: hosts generated at container start
//! 127.0.0.1   localhost
//! ::1         localhost ip6-localhost ip6-loopback
//! <ipv4>      <container_id>
//! ```
//!
//! The container id doubles as the hostname. If the operator already
//! sethostname'd via `Spec.hostname`, this row is still useful: the
//! id is what `wisp ps` shows so a `getent hosts <id>` from inside
//! the container resolves to the bridge IP.

use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::Path;

use crate::error::WispNetError;

/// Write `<rootfs>/etc/hosts` with the minimal localhost rows plus a
/// `<container_ip>  <container_id>` row.
///
/// Overwrites any existing file. Creates `<rootfs>/etc/` if missing.
pub fn write_hosts(
    rootfs: &Path,
    container_id: &str,
    container_ip: Ipv4Addr,
) -> Result<(), WispNetError> {
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc)
        .map_err(|e| WispNetError::Resolv(format!("mkdir -p {}: {e}", etc.display())))?;

    let mut content = String::from("# wisp: hosts generated at container start\n");
    content.push_str("127.0.0.1\tlocalhost\n");
    content.push_str("::1\tlocalhost ip6-localhost ip6-loopback\n");
    content.push_str(&format!("{}\t{}\n", container_ip, container_id));

    write_atomic(&etc.join("hosts"), content.as_bytes())
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<(), WispNetError> {
    let dir = path.parent().ok_or_else(|| {
        WispNetError::Resolv(format!("write_atomic: {} has no parent", path.display()))
    })?;
    fs::create_dir_all(dir)
        .map_err(|e| WispNetError::Resolv(format!("mkdir -p {}: {e}", dir.display())))?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| WispNetError::Resolv(format!("create {}: {e}", tmp.display())))?;
        f.write_all(content)
            .map_err(|e| WispNetError::Resolv(format!("write {}: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| WispNetError::Resolv(format!("fsync {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        WispNetError::Resolv(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_hosts_includes_localhost_and_container_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_hosts(tmp.path(), "web-1", Ipv4Addr::new(10, 83, 0, 2)).unwrap();
        let body = fs::read_to_string(tmp.path().join("etc/hosts")).unwrap();
        assert!(body.contains("127.0.0.1\tlocalhost\n"));
        assert!(body.contains("::1\tlocalhost ip6-localhost ip6-loopback\n"));
        assert!(body.contains("10.83.0.2\tweb-1\n"));
    }

    #[test]
    fn write_hosts_creates_etc_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // No <rootfs>/etc beforehand.
        write_hosts(tmp.path(), "x", Ipv4Addr::new(10, 0, 0, 2)).unwrap();
        assert!(tmp.path().join("etc").is_dir());
        assert!(tmp.path().join("etc/hosts").is_file());
    }

    #[test]
    fn write_hosts_overwrites_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("etc")).unwrap();
        fs::write(tmp.path().join("etc/hosts"), b"# stale\n").unwrap();
        write_hosts(tmp.path(), "ctr", Ipv4Addr::new(10, 0, 0, 5)).unwrap();
        let body = fs::read_to_string(tmp.path().join("etc/hosts")).unwrap();
        assert!(!body.contains("# stale"));
        assert!(body.contains("10.0.0.5\tctr\n"));
    }

    #[test]
    fn write_hosts_starts_with_wisp_header_comment() {
        let tmp = tempfile::tempdir().unwrap();
        write_hosts(tmp.path(), "x", Ipv4Addr::new(10, 0, 0, 2)).unwrap();
        let body = fs::read_to_string(tmp.path().join("etc/hosts")).unwrap();
        assert!(
            body.starts_with("# wisp:"),
            "expected wisp marker comment as first line: {body}"
        );
    }
}
