//! Linux-only [`wisp::NetworkAttacher`] used by [`super::wisp_backend::WispBackend`].
//!
//! Mirrors the per-network attach helpers used by
//! `crates/wisp-cli/src/net_attacher.rs` (which the integration test in
//! `crates/wisp/tests/runtime_with_network.rs` exercises). Kept here
//! rather than as a shared dependency because the CLI crate is a
//! binary, not a library: the agent can't depend on it without
//! producing a circular crate graph (wisp-cli depends on wisp-image
//! which depends on wisp; the agent depends on all three).
//!
//! # Multi-network shape
//!
//! Compose services routinely declare `networks: [a, b]`; the agent's
//! `WispBackend` translates each name into a separate
//! [`wisp::NetworkSpec`] and hands the whole Vec to
//! `Runtime::create_with_networks`. At start time the wisp lifecycle
//! invokes `attach()` once per spec, threading `eth_index` so the
//! veths land on distinct in-namespace interfaces.
//!
//! [`MultiNetAttacher`] therefore owns a map of
//! `network_name -> wisp_net::Network` (each with its own bridge /
//! subnet / gateway) plus a single shared
//! [`wisp_net::StaticBitmapIpam`]: IP allocations are namespaced by
//! network name on disk so one IPAM instance handles every network the
//! agent knows about. The dispatch happens per call by looking up
//! `spec.network_name` against the map.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use wisp::{NetworkAttacher, NetworkAttachmentRecord, NetworkSpec, ResolvSource, WispError};

/// Multi-network attacher: dispatches per-attach to the right
/// `wisp_net::Network` based on `spec.network_name`.
///
/// The agent populates this via
/// [`MultiNetAttacher::register_network`] each time a fresh network is
/// ensured (i.e. on first compose reconcile that mentions a new
/// network). The map is the source of truth for "which bridge / subnet
/// does this network resolve to"; subsequent attach calls look up by
/// name. Unknown names error out: the agent's create path
/// pre-registers every network it cares about before triggering start,
/// so a missing key here is a programming error.
pub struct MultiNetAttacher {
    /// network name -> resolved `wisp_net::Network`. Bridge + subnet +
    /// gateway are derived once via `wisp_net::Network::new` and cached
    /// here so per-attach calls don't re-parse.
    networks: BTreeMap<String, wisp_net::Network>,
    /// IPAM state directory root (`<state_dir>/networks/`). Per-network
    /// allocations live under `<root>/<network_name>/`. We construct a
    /// fresh [`wisp_net::StaticBitmapIpam`] per attach against the
    /// per-network subdir so each network's bitmap is independent.
    ipam_root: PathBuf,
}

impl MultiNetAttacher {
    /// Build an empty attacher rooted at `ipam_root`. The agent calls
    /// [`Self::register_network`] for each network it ensures.
    pub fn new(ipam_root: &Path) -> Self {
        Self {
            networks: BTreeMap::new(),
            ipam_root: ipam_root.to_path_buf(),
        }
    }

    /// Register a network so future `attach()` calls naming it can
    /// dispatch. Idempotent: re-registering the same name overwrites
    /// the cached `wisp_net::Network` (the agent's `ensure_bridge`
    /// guarantees the subnet matches, otherwise it would have errored
    /// earlier).
    pub fn register_network(&mut self, network: wisp_net::Network) {
        self.networks.insert(network.name.clone(), network);
    }

    /// Look up a network by name. Returns `None` if not registered.
    fn network_for(&self, name: &str) -> Option<&wisp_net::Network> {
        self.networks.get(name)
    }

    /// Build a fresh per-network IPAM handle. Each call returns its own
    /// `StaticBitmapIpam` instance; the on-disk bitmap is the shared
    /// state across instances, so concurrent attaches against the same
    /// network race against the same file (the IPAM's lock policy
    /// handles that).
    fn ipam_for(&self, network_name: &str) -> wisp_net::StaticBitmapIpam {
        let dir = self.ipam_root.join(network_name);
        // Best-effort mkdir: the agent's `ensure_bridge` also creates
        // it. We mkdir here defensively so tests that exercise the
        // attacher in isolation don't have to pre-populate.
        let _ = std::fs::create_dir_all(&dir);
        wisp_net::StaticBitmapIpam::new(&dir)
    }
}

impl NetworkAttacher for MultiNetAttacher {
    fn attach(
        &mut self,
        spec: &NetworkSpec,
        container_id: &str,
        container_pid: u32,
        rootfs: &Path,
        eth_index: u32,
    ) -> Result<NetworkAttachmentRecord, WispError> {
        use wisp_net::Ipam as _;

        let network = self.network_for(&spec.network_name).ok_or_else(|| {
            WispError::Lifecycle(format!(
                "MultiNetAttacher: network {:?} is not registered \
                 (agent must call register_network before start)",
                spec.network_name
            ))
        })?;
        // Clone the network out of the borrow so we can release the
        // immutable borrow on `self.networks` before we touch
        // `self.ipam_for`.
        let network = network.clone();

        let iface_name = format!("eth{eth_index}");
        let is_primary = eth_index == 0;

        wisp_net::bridge::ensure(&network)
            .map_err(|e| WispError::Lifecycle(format!("bridge::ensure: {e}")))?;

        let net_rs = wisp_net::iptables::plan_for_network(&network);
        let _ = wisp_net::iptables::revoke(&net_rs);
        wisp_net::iptables::apply(&net_rs)
            .map_err(|e| WispError::Lifecycle(format!("iptables::apply (net): {e}")))?;

        let mut ipam = self.ipam_for(&network.name);
        let ip = ipam
            .alloc(&network.name, network.subnet, network.gateway, container_id)
            .map_err(|e| WispError::Lifecycle(format!("ipam::alloc: {e}")))?;

        let pair = wisp_net::veth::create_pair()
            .map_err(|e| WispError::Lifecycle(format!("veth::create_pair: {e}")))?;
        if let Err(e) = wisp_net::veth::attach_to_ns(
            &pair,
            container_pid,
            &network.bridge,
            ip,
            network.subnet.prefix_len(),
            network.gateway,
            &iface_name,
        ) {
            let _ = ipam.release(&network.name, container_id);
            return Err(WispError::Lifecycle(format!("veth::attach_to_ns: {e}")));
        }

        let attach_rs =
            wisp_net::iptables::plan_for_attachment(&network, ip, container_id, &spec.ports);
        if let Err(e) = wisp_net::iptables::apply(&attach_rs) {
            let _ = wisp_net::veth::delete(&pair);
            let _ = ipam.release(&network.name, container_id);
            return Err(WispError::Lifecycle(format!(
                "iptables::apply (attachment): {e}"
            )));
        }

        // /etc/resolv.conf + /etc/hosts are written on the PRIMARY
        // attach only. Subsequent network attaches would clobber the
        // first write and leave the container with the wrong DNS view.
        if is_primary {
            match &spec.resolv_source {
                ResolvSource::HostCopy => {
                    let nameservers = wisp_net::host_nameservers().unwrap_or_default();
                    if let Err(e) = wisp_net::write_resolv_conf(rootfs, &nameservers) {
                        return Err(WispError::Lifecycle(format!(
                            "write_resolv_conf (host copy): {e}"
                        )));
                    }
                }
                ResolvSource::Static(addrs) => {
                    if let Err(e) = wisp_net::write_resolv_conf(rootfs, addrs) {
                        return Err(WispError::Lifecycle(format!(
                            "write_resolv_conf (static): {e}"
                        )));
                    }
                }
                ResolvSource::None => {}
            }
            if let Err(e) = wisp_net::write_hosts(rootfs, container_id, ip) {
                return Err(WispError::Lifecycle(format!("write_hosts: {e}")));
            }
        }

        Ok(NetworkAttachmentRecord {
            container_id: container_id.to_string(),
            network_name: network.name.clone(),
            bridge: network.bridge.clone(),
            ipv4: ip,
            veth_host: pair.host_name,
            veth_container: pair.container_name,
            ports: spec.ports.clone(),
        })
    }

    fn detach(&mut self, record: &NetworkAttachmentRecord) -> Result<(), WispError> {
        use wisp_net::Ipam as _;

        // Detach by network name: the persisted record carries it, so
        // we look up the matching `wisp_net::Network` from our map. If
        // the agent restarted between attach and detach the registry
        // may be empty; fall back to a best-effort detach using just
        // the recorded bridge + ports (we don't need the subnet for
        // detach because iptables rules + veth delete + ipam release
        // are all keyed by container id / record contents).
        let plan_network = self.network_for(&record.network_name).cloned();
        if let Some(network) = plan_network.as_ref() {
            let attach_rs = wisp_net::iptables::plan_for_attachment(
                network,
                record.ipv4,
                &record.container_id,
                &record.ports,
            );
            let _ = wisp_net::iptables::revoke(&attach_rs);
        }

        let pair = wisp_net::veth::VethPair {
            host_name: record.veth_host.clone(),
            container_name: record.veth_container.clone(),
        };
        let _ = wisp_net::veth::delete(&pair);

        // IPAM release: needs the network name only, which the record
        // carries. The bitmap on disk reflects the alloc regardless of
        // whether the map has the network registered, so this is safe
        // even on a fresh attacher.
        let mut ipam = self.ipam_for(&record.network_name);
        let _ = ipam.release(&record.network_name, &record.container_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::Ipv4Net;
    use std::net::Ipv4Addr;
    use tempfile::TempDir;

    fn fake_network(name: &str, subnet: &str) -> wisp_net::Network {
        let net: Ipv4Net = subnet.parse().unwrap();
        wisp_net::Network::new(name, net).unwrap()
    }

    #[test]
    fn registering_a_network_makes_it_resolvable() {
        let tmp = TempDir::new().unwrap();
        let mut a = MultiNetAttacher::new(tmp.path());
        let net = fake_network("isengard-proxy", "10.83.0.0/24");
        a.register_network(net.clone());
        let got = a.network_for("isengard-proxy").expect("registered");
        assert_eq!(got.name, net.name);
        assert_eq!(got.bridge, net.bridge);
    }

    #[test]
    fn unknown_network_resolves_to_none() {
        let tmp = TempDir::new().unwrap();
        let a = MultiNetAttacher::new(tmp.path());
        assert!(a.network_for("ghost").is_none());
    }

    #[test]
    fn ipam_for_creates_a_per_network_subdir() {
        let tmp = TempDir::new().unwrap();
        let a = MultiNetAttacher::new(tmp.path());
        // Build the IPAM; the side-effect is the subdir being created.
        let _ipam = a.ipam_for("servarr");
        assert!(tmp.path().join("servarr").is_dir());
    }

    #[test]
    fn detach_record_for_unregistered_network_does_not_panic() {
        // Detach should be best-effort even when the attacher's network
        // map is empty (e.g. agent restarted after attach). The veth +
        // ipam path runs purely off the record fields.
        let tmp = TempDir::new().unwrap();
        let mut a = MultiNetAttacher::new(tmp.path());
        let record = NetworkAttachmentRecord {
            container_id: "test-ctr".into(),
            network_name: "ghost-net".into(),
            bridge: "wbr-ghost".into(),
            ipv4: Ipv4Addr::new(10, 83, 0, 9),
            veth_host: "wveth-h-deadbe".into(),
            veth_container: "wveth-c-deadbe".into(),
            ports: vec![],
        };
        // The detach issues `ip link delete` against a veth that
        // doesn't exist (-> error from the `ip` binary), but the
        // function tolerates that path and returns Ok regardless.
        let _ = a.detach(&record);
    }
}
