//! Veth pair lifecycle: create on the host, move the container side
//! into the target netns, and rename it to `eth0` (or `eth1`, `eth2`,
//! ... for multi-network containers) so userspace sees the interface
//! name it expects.
//!
//! The host side stays in the host netns and gets enslaved to the
//! wisp bridge so its frames hit MASQUERADE / DNAT in iptables. The
//! container side is the only thing that ever moves; its lifetime is
//! tied to the netns, so when the container exits and the kernel reaps
//! the namespace, the container-side device disappears and only the
//! host side needs an explicit cleanup.

use crate::cmd;
use crate::error::WispNetError;
use crate::ns;
use std::net::Ipv4Addr;

/// One veth pair.
///
/// `host_name` lives in the host netns and is enslaved to the bridge.
/// `container_name` is the temporary name we hand to `ip link set
/// <name> netns <pid>`; once it's inside the target ns, [`attach_to_ns`]
/// renames it to the operator-supplied `iface_name` (typically `eth0`
/// for single-network containers; `eth<N>` for multi-network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VethPair {
    pub host_name: String,
    pub container_name: String,
}

/// Create a veth pair with random hex suffixes.
///
/// Names are `wveth-h-<hex>` and `wveth-c-<hex>` where `<hex>` is 6
/// hex digits. Both sides must fit inside `IFNAMSIZ - 1 = 15` bytes;
/// `wveth-h-` is 8 bytes, leaving 7 for the suffix. We use 6 to keep a
/// margin and so collisions remain easy to spot in `ip link show`.
///
/// Retries up to 5 times on collision (the kernel returns `EEXIST` if
/// either name is already taken).
pub fn create_pair() -> Result<VethPair, WispNetError> {
    for _ in 0..5 {
        let suffix = random_hex_suffix();
        let pair = VethPair {
            host_name: format!("wveth-h-{suffix}"),
            container_name: format!("wveth-c-{suffix}"),
        };
        match cmd::run_ip(&[
            "link",
            "add",
            &pair.host_name,
            "type",
            "veth",
            "peer",
            "name",
            &pair.container_name,
        ]) {
            Ok(_) => return Ok(pair),
            Err(WispNetError::Cmd { stderr, .. })
                if stderr.to_ascii_lowercase().contains("file exists") =>
            {
                // Collision: pick a fresh suffix.
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(WispNetError::Veth(
        "failed to create veth pair after 5 attempts (name collisions)".into(),
    ))
}

/// Move the container side into the target netns, rename to `iface_name`,
/// and configure addressing.
///
/// `iface_name` is the in-netns name the container sees (typically
/// `eth0` for the primary network, `eth1` / `eth2` / ... for additional
/// networks when a container is attached to more than one). The caller
/// is responsible for handing distinct names per attach so the kernel's
/// link table stays unambiguous.
///
/// The default route is only installed for the FIRST interface (i.e.
/// when `iface_name == "eth0"`). Additional networks add their subnet
/// route only; otherwise each new attach would clobber the gateway and
/// the last one wins. Compose-spec semantics align: the primary network
/// (first declared) gates outbound traffic; secondary networks are for
/// sibling-service reachability.
///
/// Sequence (matches the spec):
///
/// 1. `ip link set <host> master <bridge>`
/// 2. `ip link set <ctr> netns <pid>`
/// 3. `ip link set <host> up`
/// 4. inside ns: `ip link set <ctr> name <iface_name>`
/// 5. inside ns: `ip link set lo up` (no-op after the first attach)
/// 6. inside ns: `ip link set <iface_name> up`
/// 7. inside ns: `ip addr add <ip>/<prefix> dev <iface_name>`
/// 8. inside ns (eth0 only): `ip route add default via <gateway>`
///
/// On any step failure, [`delete`] is called best-effort to remove the
/// host side; the container side either stayed in the host ns (steps
/// 1+2 failed) or rides with the namespace cleanup. The caller still
/// gets the original error.
pub fn attach_to_ns(
    pair: &VethPair,
    ns_pid: u32,
    bridge_name: &str,
    container_ip: Ipv4Addr,
    prefix: u8,
    gateway: Ipv4Addr,
    iface_name: &str,
) -> Result<(), WispNetError> {
    let pid_str = ns_pid.to_string();
    let cidr = format!("{container_ip}/{prefix}");
    let gw_str = gateway.to_string();
    let is_primary = iface_name == "eth0";

    let result: Result<(), WispNetError> = (|| {
        // 1. enslave host side to the bridge.
        cmd::run_ip(&["link", "set", &pair.host_name, "master", bridge_name])?;
        // 2. move container side into the target netns.
        cmd::run_ip(&["link", "set", &pair.container_name, "netns", &pid_str])?;
        // 3. bring host side up.
        cmd::run_ip(&["link", "set", &pair.host_name, "up"])?;

        // 4-8. configure inside the netns.
        ns::run_in_netns(
            ns_pid,
            &[
                "ip",
                "link",
                "set",
                &pair.container_name,
                "name",
                iface_name,
            ],
        )?;
        // Loopback only needs bringing up once; subsequent attaches
        // see `lo` already UP and `ip link set lo up` is a no-op.
        ns::run_in_netns(ns_pid, &["ip", "link", "set", "lo", "up"])?;
        ns::run_in_netns(ns_pid, &["ip", "link", "set", iface_name, "up"])?;
        ns::run_in_netns(ns_pid, &["ip", "addr", "add", &cidr, "dev", iface_name])?;
        if is_primary {
            ns::run_in_netns(ns_pid, &["ip", "route", "add", "default", "via", &gw_str])?;
        }
        Ok(())
    })();

    if result.is_err() {
        // Rollback: best-effort delete of the host side. Ignore the
        // delete error so the caller sees the original failure.
        let _ = delete(pair);
    }
    result
}

/// Best-effort delete of the host side.
///
/// The container side is gone with the namespace if attach succeeded;
/// if attach failed mid-way it might still be in the host ns, but
/// `ip link delete <host>` cascades to the peer because veth pairs are
/// linked devices, so a single delete on the host name suffices.
pub fn delete(pair: &VethPair) -> Result<(), WispNetError> {
    match cmd::run_ip(&["link", "delete", &pair.host_name]) {
        Ok(_) => Ok(()),
        Err(WispNetError::Cmd { stderr, .. }) if is_missing_device(&stderr) => Ok(()),
        Err(e) => Err(e),
    }
}

fn is_missing_device(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("cannot find device") || s.contains("does not exist")
}

/// Build a 6-hex-digit suffix from `/dev/urandom`.
///
/// We avoid `rand` to keep wisp-net's dep tree narrow; reading 3 bytes
/// from urandom and hex-encoding them is the same entropy and one
/// fewer crate.
fn random_hex_suffix() -> String {
    use std::fs::File;
    use std::io::Read;
    let mut buf = [0u8; 3];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        // Fallback: time-based jitter. urandom should always exist on
        // Linux; this branch is only here so unit tests on Mac don't
        // panic if /dev/urandom is for some reason unavailable.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        buf[0] = (nanos & 0xff) as u8;
        buf[1] = ((nanos >> 8) & 0xff) as u8;
        buf[2] = ((nanos >> 16) & 0xff) as u8;
    }
    format!("{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_suffix_is_6_chars() {
        let s = random_hex_suffix();
        assert_eq!(s.len(), 6);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn random_hex_suffix_varies() {
        // Probabilistically: 16^6 = ~16M space, 4 samples should very
        // rarely all match. Failure here means urandom is broken or we
        // hit a 1-in-a-quadrillion event.
        let samples = [
            random_hex_suffix(),
            random_hex_suffix(),
            random_hex_suffix(),
            random_hex_suffix(),
        ];
        let unique: std::collections::HashSet<_> = samples.iter().collect();
        assert!(unique.len() >= 2, "all 4 suffixes equal: {samples:?}");
    }

    #[test]
    fn veth_names_fit_in_ifnamsiz() {
        // wveth-h- = 8 bytes, suffix = 6 bytes -> total 14 (under 15).
        let pair = VethPair {
            host_name: format!("wveth-h-{}", random_hex_suffix()),
            container_name: format!("wveth-c-{}", random_hex_suffix()),
        };
        assert!(pair.host_name.len() <= 15);
        assert!(pair.container_name.len() <= 15);
    }
}
