//! Linux-only [`wisp::NetworkAttacher`] used by [`super::wisp_backend::WispBackend`].
//!
//! Mirrors `crates/wisp-cli/src/net_attacher.rs` (which the integration
//! test in `crates/wisp/tests/runtime_with_network.rs` exercises). Kept
//! here as a duplicate rather than a shared dependency because the CLI
//! crate is a binary, not a library: the agent can't depend on it
//! without producing a circular crate graph (wisp-cli depends on
//! wisp-image which depends on wisp; the agent depends on all three).
//!
//! Dispatch B2 only uses this for the agent's default network. Multi-
//! network containers are deferred to 0.5; in 0.4 the agent's
//! `connect_network` / `disconnect_network` errors out with a clear
//! "recreate the container" message.

#![cfg(target_os = "linux")]

use std::path::Path;

use wisp::{NetworkAttacher, NetworkAttachmentRecord, NetworkSpec, ResolvSource, WispError};

pub struct WispNetAttacher {
    network: wisp_net::Network,
    ipam: wisp_net::StaticBitmapIpam,
}

impl WispNetAttacher {
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
    ) -> Result<NetworkAttachmentRecord, WispError> {
        use wisp_net::Ipam as _;

        wisp_net::bridge::ensure(&self.network)
            .map_err(|e| WispError::Lifecycle(format!("bridge::ensure: {e}")))?;

        let net_rs = wisp_net::iptables::plan_for_network(&self.network);
        let _ = wisp_net::iptables::revoke(&net_rs);
        wisp_net::iptables::apply(&net_rs)
            .map_err(|e| WispError::Lifecycle(format!("iptables::apply (net): {e}")))?;

        let ip = self
            .ipam
            .alloc(
                &self.network.name,
                self.network.subnet,
                self.network.gateway,
                container_id,
            )
            .map_err(|e| WispError::Lifecycle(format!("ipam::alloc: {e}")))?;

        let pair = wisp_net::veth::create_pair()
            .map_err(|e| WispError::Lifecycle(format!("veth::create_pair: {e}")))?;
        if let Err(e) = wisp_net::veth::attach_to_ns(
            &pair,
            container_pid,
            &self.network.bridge,
            ip,
            self.network.subnet.prefix_len(),
            self.network.gateway,
        ) {
            let _ = self.ipam.release(&self.network.name, container_id);
            return Err(WispError::Lifecycle(format!("veth::attach_to_ns: {e}")));
        }

        let attach_rs =
            wisp_net::iptables::plan_for_attachment(&self.network, ip, container_id, &spec.ports);
        if let Err(e) = wisp_net::iptables::apply(&attach_rs) {
            let _ = wisp_net::veth::delete(&pair);
            let _ = self.ipam.release(&self.network.name, container_id);
            return Err(WispError::Lifecycle(format!(
                "iptables::apply (attachment): {e}"
            )));
        }

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

        let _ = self.ipam.release(&self.network.name, &record.container_id);
        Ok(())
    }
}
