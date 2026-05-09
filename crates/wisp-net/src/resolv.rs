//! Render `/etc/resolv.conf` inside a container's rootfs.
//!
//! Called by the runtime BEFORE the child does `pivot_root`: at that
//! point `<bundle>/rootfs/etc/resolv.conf` is just a regular file on
//! the host filesystem and we own the writes. Once the child is
//! exec'd, those bytes are visible at `/etc/resolv.conf` from inside
//! the container.
//!
//! Two concrete inputs are supported by the runtime; both reduce to a
//! list of nameserver IPs by the time they reach this module:
//!
//! - `ResolvSource::HostCopy`: read `/etc/resolv.conf` on the host,
//!   filter the `nameserver` lines that point at loopback addresses
//!   (a 127.x stub resolver on the host is useless to the container,
//!   which lives on a different bridge subnet), and pass the rest in.
//! - `ResolvSource::Static(...)`: pass the operator's literal list.
//!
//! [`ResolvSource::None`] is handled by the caller (no
//! `write_resolv_conf` invocation).
//!
//! Writes are atomic-ish: a tempfile in the same directory + rename,
//! same shape as `state::write`. The parent `<rootfs>/etc/` directory
//! is created with `mkdir_p` if missing so a freshly-prepared bundle
//! that omits `/etc` does not blow up.

use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;

use crate::error::WispNetError;

/// Write `<rootfs>/etc/resolv.conf` with one `nameserver <addr>` line
/// per entry in `nameservers`.
///
/// The file is overwritten if it already exists. Empty
/// `nameservers` writes a header comment + no nameserver lines, so
/// the container ends up with a deterministic empty resolver
/// (matching docker's behaviour on `--dns=` with no value).
pub fn write_resolv_conf(rootfs: &Path, nameservers: &[IpAddr]) -> Result<(), WispNetError> {
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc)
        .map_err(|e| WispNetError::Resolv(format!("mkdir -p {}: {e}", etc.display())))?;

    let mut content = String::from("# wisp: resolv.conf generated at container start\n");
    for ns in nameservers {
        content.push_str(&format!("nameserver {}\n", ns));
    }

    write_atomic(&etc.join("resolv.conf"), content.as_bytes())
}

/// Read the host's `/etc/resolv.conf` and return the non-loopback
/// nameserver addresses in declaration order.
///
/// Used by the runtime when [`crate::ResolvSource::HostCopy`] is
/// requested. Lines that don't begin with `nameserver ` are ignored;
/// nameservers in `127.0.0.0/8` (and `::1`) are dropped because the
/// host's stub resolver is unreachable from the container's bridge
/// subnet.
///
/// Errors only on a hard IO failure reading the host file. An empty
/// resolv.conf is fine and produces an empty Vec; the caller decides
/// whether to fall back.
pub fn host_nameservers() -> Result<Vec<IpAddr>, WispNetError> {
    let path = Path::new("/etc/resolv.conf");
    let body = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(WispNetError::Resolv(format!(
                "read {}: {e}",
                path.display()
            )));
        }
    };
    Ok(parse_host_nameservers(&body))
}

fn parse_host_nameservers(body: &str) -> Vec<IpAddr> {
    body.lines()
        .filter_map(|raw| {
            let line = raw.trim();
            if line.starts_with('#') || line.is_empty() {
                return None;
            }
            let rest = line.strip_prefix("nameserver")?.trim();
            // Take the first whitespace-delimited token; ignore any
            // trailing comment.
            let token = rest.split_whitespace().next()?;
            let addr: IpAddr = token.parse().ok()?;
            if is_loopback(&addr) { None } else { Some(addr) }
        })
        .collect()
}

fn is_loopback(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.octets()[0] == 127,
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Atomic file write: create `<path>.tmp` next to the target, write
/// + fsync, rename onto the target. Same shape as `wisp::state::write`.
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
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn write_resolv_conf_emits_nameserver_per_addr() {
        let tmp = tempfile::tempdir().unwrap();
        let nameservers = vec![
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6("2606:4700:4700::1111".parse().unwrap()),
        ];
        write_resolv_conf(tmp.path(), &nameservers).unwrap();
        let body = fs::read_to_string(tmp.path().join("etc/resolv.conf")).unwrap();
        assert!(body.contains("nameserver 1.1.1.1\n"));
        assert!(body.contains("nameserver 8.8.8.8\n"));
        assert!(body.contains("nameserver 2606:4700:4700::1111\n"));
    }

    #[test]
    fn write_resolv_conf_creates_etc_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Note: we deliberately do NOT pre-create <rootfs>/etc.
        let result = write_resolv_conf(tmp.path(), &[IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))]);
        assert!(result.is_ok(), "expected mkdir success: {result:?}");
        assert!(tmp.path().join("etc").is_dir());
        assert!(tmp.path().join("etc/resolv.conf").is_file());
    }

    #[test]
    fn write_resolv_conf_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("etc")).unwrap();
        fs::write(tmp.path().join("etc/resolv.conf"), b"stale data\n").unwrap();
        write_resolv_conf(tmp.path(), &[IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))]).unwrap();
        let body = fs::read_to_string(tmp.path().join("etc/resolv.conf")).unwrap();
        assert!(!body.contains("stale data"));
        assert!(body.contains("nameserver 1.1.1.1"));
    }

    #[test]
    fn write_resolv_conf_empty_list_writes_header_only() {
        let tmp = tempfile::tempdir().unwrap();
        write_resolv_conf(tmp.path(), &[]).unwrap();
        let body = fs::read_to_string(tmp.path().join("etc/resolv.conf")).unwrap();
        assert!(body.starts_with("# wisp:"));
        assert!(!body.contains("nameserver"));
    }

    #[test]
    fn parse_host_nameservers_filters_loopback() {
        let raw = "\
# auto-generated\n\
nameserver 127.0.0.53\n\
nameserver 1.1.1.1\n\
nameserver ::1\n\
nameserver 2606:4700:4700::1111\n\
search example.com\n";
        let got = parse_host_nameservers(raw);
        assert_eq!(
            got,
            vec![
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
                IpAddr::V6("2606:4700:4700::1111".parse().unwrap()),
            ]
        );
    }

    #[test]
    fn parse_host_nameservers_handles_blank_lines_and_comments() {
        let raw = "\n\n# comment\n\n  nameserver 8.8.8.8\n\n";
        let got = parse_host_nameservers(raw);
        assert_eq!(got, vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]);
    }

    #[test]
    fn parse_host_nameservers_skips_garbage_addresses() {
        let raw = "nameserver not-an-ip\nnameserver 8.8.8.8\n";
        let got = parse_host_nameservers(raw);
        assert_eq!(got, vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]);
    }

    #[test]
    fn is_loopback_matches_v4_127_and_v6_loopback() {
        assert!(is_loopback(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 53))));
        assert!(is_loopback(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_loopback(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }
}
