//! Production [`wisp::NetworkAttacher`] backed by `wisp-net`.
//!
//! `wisp` deliberately does not depend on `wisp-net` (the runtime owns
//! the data shape; `wisp-net` consumes it via the trait). This file is
//! the wiring layer: a small struct that owns a parsed
//! [`wisp_net::Network`] + an IPAM state directory, and implements
//! `attach` / `detach` by walking bridge -> iptables -> ipam -> veth
//! -> per-attachment iptables -> resolv.conf -> /etc/hosts.
//!
//! The shape mirrors the integration test in
//! `crates/wisp/tests/runtime_with_network.rs`. Keeping the two in
//! sync is intentional: the test exercises this exact sequence, so a
//! green test means the production attacher works end-to-end.

use std::path::Path;

use wisp::{NetworkAttacher, NetworkAttachmentRecord, NetworkSpec, ResolvSource, WispError};

/// `wisp-net`-backed attacher.
///
/// One instance per `wisp run --network <name>` invocation. Holds the
/// parsed [`wisp_net::Network`] (so we already have the bridge name +
/// gateway) and the IPAM state dir (so the lowest-free IP allocation
/// is durable across CLI invocations).
pub struct WispNetAttacher {
    network: wisp_net::Network,
    ipam: wisp_net::StaticBitmapIpam,
}

impl WispNetAttacher {
    /// Build an attacher pinned to `network`, persisting IPAM state
    /// under `ipam_state_dir` (typically
    /// `<state-dir>/networks/<name>/`).
    pub fn new(network: wisp_net::Network, ipam_state_dir: &Path) -> Self {
        Self {
            network,
            ipam: wisp_net::StaticBitmapIpam::new(ipam_state_dir),
        }
    }
}

impl NetworkAttacher for WispNetAttacher {
    fn attach(
        &mut self,
        spec: &NetworkSpec,
        container_id: &str,
        container_pid: u32,
        rootfs: &Path,
        eth_index: u32,
    ) -> Result<NetworkAttachmentRecord, WispError> {
        use wisp_net::Ipam as _;

        let iface_name = format!("eth{eth_index}");
        let is_primary = eth_index == 0;

        // 1. Lazily ensure the bridge exists. Operator may have done it
        //    via `wisp net create`; if not, this creates it on demand
        //    so `wisp run --network app` works without a separate step.
        wisp_net::bridge::ensure(&self.network)
            .map_err(|e| WispError::Lifecycle(format!("bridge::ensure: {e}")))?;

        // 2. Apply network-level iptables rules. Revoke first so a
        //    stale rule from a previous failed run doesn't mask a
        //    real apply error with "rule already present".
        let net_rs = wisp_net::iptables::plan_for_network(&self.network);
        let _ = wisp_net::iptables::revoke(&net_rs);
        wisp_net::iptables::apply(&net_rs)
            .map_err(|e| WispError::Lifecycle(format!("iptables::apply (net): {e}")))?;

        // 3. Allocate IP for this container.
        let ip = self
            .ipam
            .alloc(
                &self.network.name,
                self.network.subnet,
                self.network.gateway,
                container_id,
            )
            .map_err(|e| WispError::Lifecycle(format!("ipam::alloc: {e}")))?;

        // 4. Create + attach veth pair. The container side is renamed
        //    to `eth<eth_index>` so additional network attachments land
        //    on `eth1`, `eth2`, ... without colliding with the primary.
        let pair = wisp_net::veth::create_pair()
            .map_err(|e| WispError::Lifecycle(format!("veth::create_pair: {e}")))?;
        if let Err(e) = wisp_net::veth::attach_to_ns(
            &pair,
            container_pid,
            &self.network.bridge,
            ip,
            self.network.subnet.prefix_len(),
            self.network.gateway,
            &iface_name,
        ) {
            // veth::attach_to_ns rolls back the host side itself on
            // failure; release the IP so the next start can reuse it.
            let _ = self.ipam.release(&self.network.name, container_id);
            return Err(WispError::Lifecycle(format!("veth::attach_to_ns: {e}")));
        }

        // 5. Per-attachment iptables (port DNATs + FORWARD allow).
        let attach_rs =
            wisp_net::iptables::plan_for_attachment(&self.network, ip, container_id, &spec.ports);
        if let Err(e) = wisp_net::iptables::apply(&attach_rs) {
            let _ = wisp_net::veth::delete(&pair);
            let _ = self.ipam.release(&self.network.name, container_id);
            return Err(WispError::Lifecycle(format!(
                "iptables::apply (attachment): {e}"
            )));
        }

        // 6. Render /etc/resolv.conf + /etc/hosts into the rootfs (primary
        //    attach only; subsequent attaches would clobber the first
        //    write and the result would be the second network's view).
        //    The child has not yet pivot_root'd, so writes to
        //    `<rootfs>/etc/...` show up as `/etc/...` once it has.
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
                ResolvSource::None => {
                    // Operator opted out; bundle ships its own resolv.conf.
                }
            }
            if let Err(e) = wisp_net::write_hosts(rootfs, container_id, ip) {
                return Err(WispError::Lifecycle(format!("write_hosts: {e}")));
            }
        }

        Ok(NetworkAttachmentRecord {
            container_id: container_id.to_string(),
            network_name: self.network.name.clone(),
            bridge: self.network.bridge.clone(),
            ipv4: ip,
            veth_host: pair.host_name,
            veth_container: pair.container_name,
            ports: spec.ports.clone(),
        })
    }

    fn detach(&mut self, record: &NetworkAttachmentRecord) -> Result<(), WispError> {
        use wisp_net::Ipam as _;

        // Reverse order: per-attachment rules, host veth, network
        // rules + bridge (only if no other containers remain), IPAM.
        let attach_rs = wisp_net::iptables::plan_for_attachment(
            &self.network,
            record.ipv4,
            &record.container_id,
            &record.ports,
        );
        let _ = wisp_net::iptables::revoke(&attach_rs);

        let pair = wisp_net::veth::VethPair {
            host_name: record.veth_host.clone(),
            container_name: record.veth_container.clone(),
        };
        let _ = wisp_net::veth::delete(&pair);

        // Release IP first so the still-in-use check below reflects
        // the post-release count.
        let _ = self.ipam.release(&self.network.name, &record.container_id);

        // Operator-managed bridges: do NOT auto-delete the bridge or
        // network-level iptables rules just because the last container
        // left. The operator created the network via `wisp net create`
        // and tears it down with `wisp net rm`. Keeping the bridge
        // sticky lets the next `wisp run --network app` start without
        // re-doing the create dance.
        Ok(())
    }
}
