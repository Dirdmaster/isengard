//! Bridge-scoped IPv4 allocator.
//!
//! `StaticBitmapIpam` keeps an on-disk JSON record per network at
//! `<state_dir>/<network_name>/allocs.json`:
//!
//! ```json
//! { "version": 1, "allocs": { "<bundle-id>": "<ipv4>" } }
//! ```
//!
//! Allocation policy: lowest-numbered free IPv4 inside the subnet, skipping
//! the gateway (typically `.1` for /24) and the broadcast (`.255` for /24).
//!
//! Persistence: each alloc / release atomically rewrites the file via
//! `tempfile::NamedTempFile::persist`. There is no cross-process file lock;
//! a single agent process is the only writer in 0.3, and an internal
//! `Mutex` serializes calls within that process.

use crate::error::WispNetError;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Bridge-scoped IPv4 allocator.
///
/// Each `network_name` has its own pool scoped to a `subnet`; allocations
/// are keyed by `bundle_id` so callers can release by id and so `list`
/// returns id -> ip pairs.
///
/// `alloc` is idempotent: allocating the same `bundle_id` twice into the
/// same network returns the same IP (no error, no second slot consumed).
/// This matches the expected wisp-runtime retry semantics during
/// container start.
pub trait Ipam: Send {
    fn alloc(
        &mut self,
        network_name: &str,
        subnet: ipnet::Ipv4Net,
        gateway: Ipv4Addr,
        bundle_id: &str,
    ) -> Result<Ipv4Addr, WispNetError>;

    fn release(&mut self, network_name: &str, bundle_id: &str) -> Result<(), WispNetError>;

    fn list(&self, network_name: &str) -> Result<BTreeMap<String, Ipv4Addr>, WispNetError>;
}

/// On-disk static-bitmap IPAM. State directory layout:
///
/// ```text
/// <state_dir>/
///   <network_name>/
///     allocs.json
/// ```
///
/// Single-process writer assumption. Multiple `StaticBitmapIpam`
/// instances pointing at the same `state_dir` (or the same network) from
/// different processes will race; do not do that. For 0.3 the agent is
/// the sole writer.
pub struct StaticBitmapIpam {
    state_dir: PathBuf,
    inner: Mutex<()>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct AllocsFile {
    version: u32,
    /// `bundle_id -> ipv4`. `BTreeMap` for deterministic on-disk order
    /// (so `git diff` of state files stays readable in case anyone
    /// snapshots them).
    allocs: BTreeMap<String, Ipv4Addr>,
}

impl AllocsFile {
    fn empty() -> Self {
        Self {
            version: 1,
            allocs: BTreeMap::new(),
        }
    }
}

impl StaticBitmapIpam {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            inner: Mutex::new(()),
        }
    }

    fn network_dir(&self, network_name: &str) -> PathBuf {
        self.state_dir.join(network_name)
    }

    fn allocs_path(&self, network_name: &str) -> PathBuf {
        self.network_dir(network_name).join("allocs.json")
    }

    fn read_allocs(&self, network_name: &str) -> Result<AllocsFile, WispNetError> {
        let path = self.allocs_path(network_name);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let parsed: AllocsFile = serde_json::from_slice(&bytes)?;
                if parsed.version != 1 {
                    return Err(WispNetError::Parse(format!(
                        "unsupported allocs.json version {} at {}",
                        parsed.version,
                        path.display()
                    )));
                }
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AllocsFile::empty()),
            Err(e) => Err(WispNetError::Io(e)),
        }
    }

    fn write_allocs(&self, network_name: &str, file: &AllocsFile) -> Result<(), WispNetError> {
        let dir = self.network_dir(network_name);
        std::fs::create_dir_all(&dir)?;
        let path = self.allocs_path(network_name);
        let json = serde_json::to_vec_pretty(file)?;
        // Atomic write: NamedTempFile in the same dir + persist.
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        use std::io::Write;
        tmp.write_all(&json)?;
        tmp.flush()?;
        tmp.persist(&path).map_err(|e| WispNetError::Io(e.error))?;
        Ok(())
    }
}

/// Iterate the usable host addresses inside `subnet`, ascending, skipping
/// the network address, the broadcast address, and the gateway.
fn usable_hosts(subnet: ipnet::Ipv4Net, gateway: Ipv4Addr) -> impl Iterator<Item = Ipv4Addr> {
    subnet
        .hosts()
        .filter(move |addr| *addr != gateway && *addr != subnet.broadcast())
}

impl Ipam for StaticBitmapIpam {
    fn alloc(
        &mut self,
        network_name: &str,
        subnet: ipnet::Ipv4Net,
        gateway: Ipv4Addr,
        bundle_id: &str,
    ) -> Result<Ipv4Addr, WispNetError> {
        let _guard = self.inner.lock().expect("ipam mutex poisoned");
        let mut state = self.read_allocs(network_name)?;

        // Idempotency: same id -> same IP, no new slot consumed.
        if let Some(existing) = state.allocs.get(bundle_id) {
            return Ok(*existing);
        }

        let taken: std::collections::BTreeSet<Ipv4Addr> = state.allocs.values().copied().collect();
        let chosen = usable_hosts(subnet, gateway)
            .find(|addr| !taken.contains(addr))
            .ok_or_else(|| {
                WispNetError::Ipam(format!(
                    "subnet {} is full ({} usable host(s))",
                    subnet,
                    usable_hosts(subnet, gateway).count()
                ))
            })?;

        state.allocs.insert(bundle_id.to_string(), chosen);
        self.write_allocs(network_name, &state)?;
        Ok(chosen)
    }

    fn release(&mut self, network_name: &str, bundle_id: &str) -> Result<(), WispNetError> {
        let _guard = self.inner.lock().expect("ipam mutex poisoned");
        let mut state = self.read_allocs(network_name)?;
        if state.allocs.remove(bundle_id).is_none() {
            // Releasing an unknown id is a no-op; idempotent on repeated
            // detach.
            return Ok(());
        }
        self.write_allocs(network_name, &state)?;
        Ok(())
    }

    fn list(&self, network_name: &str) -> Result<BTreeMap<String, Ipv4Addr>, WispNetError> {
        let _guard = self.inner.lock().expect("ipam mutex poisoned");
        Ok(self.read_allocs(network_name)?.allocs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::Ipv4Net;

    fn subnet_24() -> Ipv4Net {
        "10.83.0.0/24".parse().unwrap()
    }

    fn gw_24() -> Ipv4Addr {
        Ipv4Addr::new(10, 83, 0, 1)
    }

    #[test]
    fn alloc_returns_lowest_free() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        let a = ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
        let b = ipam.alloc("net", subnet_24(), gw_24(), "ctr-b").unwrap();
        assert_eq!(a, Ipv4Addr::new(10, 83, 0, 2));
        assert_eq!(b, Ipv4Addr::new(10, 83, 0, 3));
    }

    #[test]
    fn alloc_skips_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        let first = ipam.alloc("net", subnet_24(), gw_24(), "ctr").unwrap();
        assert_eq!(first, Ipv4Addr::new(10, 83, 0, 2));
        // Gateway never appears in the alloc map.
        assert!(!ipam.list("net").unwrap().values().any(|v| *v == gw_24()));
    }

    #[test]
    fn alloc_skips_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        // /24 has 254 hosts (Ipv4Net::hosts excludes net + broadcast for
        // prefix < 31). Subtract one for the gateway: 253 allocations
        // possible. The last one must be .254, not .255.
        let mut last = None;
        for i in 0..253 {
            last = Some(
                ipam.alloc("net", subnet_24(), gw_24(), &format!("ctr-{i}"))
                    .unwrap(),
            );
        }
        assert_eq!(last, Some(Ipv4Addr::new(10, 83, 0, 254)));
        assert!(
            !ipam
                .list("net")
                .unwrap()
                .values()
                .any(|v| *v == Ipv4Addr::new(10, 83, 0, 255))
        );
    }

    #[test]
    fn alloc_persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut ipam = StaticBitmapIpam::new(dir.path());
            ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
            ipam.alloc("net", subnet_24(), gw_24(), "ctr-b").unwrap();
        }
        let ipam = StaticBitmapIpam::new(dir.path());
        let listed = ipam.list("net").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed.get("ctr-a"), Some(&Ipv4Addr::new(10, 83, 0, 2)));
        assert_eq!(listed.get("ctr-b"), Some(&Ipv4Addr::new(10, 83, 0, 3)));
    }

    #[test]
    fn release_returns_addr_to_pool() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        let a1 = ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
        ipam.release("net", "ctr-a").unwrap();
        let a2 = ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
        assert_eq!(a1, a2);
        assert_eq!(a1, Ipv4Addr::new(10, 83, 0, 2));
    }

    #[test]
    fn alloc_errors_when_subnet_full() {
        // /29: 8 addrs. Hosts() returns 6 (skips network + broadcast).
        // Skipping the gateway (.1) leaves 5 usable.
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        let net: Ipv4Net = "192.168.7.0/29".parse().unwrap();
        let gw = Ipv4Addr::new(192, 168, 7, 1);
        for i in 0..5 {
            ipam.alloc("tiny", net, gw, &format!("ctr-{i}")).unwrap();
        }
        let err = ipam.alloc("tiny", net, gw, "ctr-overflow").unwrap_err();
        match err {
            WispNetError::Ipam(msg) => {
                assert!(msg.contains("full"), "expected full error, got {msg}")
            }
            other => panic!("expected Ipam(full) error, got {other:?}"),
        }
    }

    #[test]
    fn list_returns_currently_allocated() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        assert!(ipam.list("net").unwrap().is_empty());
        ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
        ipam.alloc("net", subnet_24(), gw_24(), "ctr-b").unwrap();
        let listed = ipam.list("net").unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.contains_key("ctr-a"));
        assert!(listed.contains_key("ctr-b"));
    }

    #[test]
    fn alloc_idempotent_for_same_bundle_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        let first = ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
        let second = ipam.alloc("net", subnet_24(), gw_24(), "ctr-a").unwrap();
        assert_eq!(first, second);
        // No second slot consumed.
        assert_eq!(ipam.list("net").unwrap().len(), 1);
    }

    #[test]
    fn networks_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let mut ipam = StaticBitmapIpam::new(dir.path());
        let a = ipam.alloc("net-a", subnet_24(), gw_24(), "ctr").unwrap();
        let b = ipam.alloc("net-b", subnet_24(), gw_24(), "ctr").unwrap();
        // Both networks start fresh: each allocates the lowest free addr,
        // which is .2 on a /24 with .1 reserved as the gateway.
        assert_eq!(a, Ipv4Addr::new(10, 83, 0, 2));
        assert_eq!(b, Ipv4Addr::new(10, 83, 0, 2));
        assert_eq!(ipam.list("net-a").unwrap().len(), 1);
        assert_eq!(ipam.list("net-b").unwrap().len(), 1);
    }
}
