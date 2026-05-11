//! Multi-network runtime integration test (Phase 0.18).
//!
//! Repros the lausanne bug: a service declaring two networks must
//! reach Running with both veths attached, not error out at the second
//! attach with "wisp does not support live network attach".
//!
//! `#[ignore]`, root-only, Linux-only. Drives:
//!   1. `Runtime::create_with_networks` with two distinct
//!      `NetworkSpec`s pointing at two distinct bridges + subnets.
//!   2. A multi-network `NetworkAttacher` impl that dispatches to the
//!      right `wisp_net::Network` based on `spec.network_name`.
//!   3. `Runtime::start_with_attacher`: the lifecycle invokes
//!      `attach()` twice in declaration order, threading eth_index
//!      0/1 so the veths land on eth0 / eth1.
//!   4. Verifies state.json has 2 attachment records, declaration
//!      order is preserved, eth0 + eth1 are both present in
//!      `/proc/<pid>/net/dev`, and both bridge IPs are reachable.
//!   5. Delete and verify both networks are torn down.

#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use wisp::{ContainerState, NetworkAttacher, NetworkAttachmentRecord, NetworkSpec, WispError};

/// Multi-network attacher used by the integration test. Holds a map
/// of `network_name -> wisp_net::Network` so each `attach()` call can
/// dispatch by name (matches what the agent does in production via
/// `MultiNetAttacher`).
struct MultiNetAttacher {
    networks: BTreeMap<String, wisp_net::Network>,
    /// One IPAM root per network: tests put each network's bitmap in
    /// its own subdir under `<ipam_root>/<network_name>/`.
    ipam_root: std::path::PathBuf,
}

impl MultiNetAttacher {
    fn new(ipam_root: &Path) -> Self {
        Self {
            networks: BTreeMap::new(),
            ipam_root: ipam_root.to_path_buf(),
        }
    }

    fn register(&mut self, network: wisp_net::Network) {
        self.networks.insert(network.name.clone(), network);
    }

    fn ipam_for(&self, name: &str) -> wisp_net::StaticBitmapIpam {
        let dir = self.ipam_root.join(name);
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

        let network = self
            .networks
            .get(&spec.network_name)
            .cloned()
            .ok_or_else(|| {
                WispError::Lifecycle(format!(
                    "test attacher: unknown network {:?}",
                    spec.network_name
                ))
            })?;

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

        // Primary attach writes resolv.conf + /etc/hosts. Secondary
        // attaches deliberately skip these so they don't clobber the
        // first write.
        if is_primary {
            let nameservers = wisp_net::host_nameservers().unwrap_or_default();
            if let Err(e) = wisp_net::write_resolv_conf(rootfs, &nameservers) {
                return Err(WispError::Lifecycle(format!("write_resolv_conf: {e}")));
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

        if let Some(network) = self.networks.get(&record.network_name).cloned() {
            let attach_rs = wisp_net::iptables::plan_for_attachment(
                &network,
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

        let mut ipam = self.ipam_for(&record.network_name);
        let _ = ipam.release(&record.network_name, &record.container_id);

        // Bridge teardown when no allocs remain.
        if let Some(network) = self.networks.get(&record.network_name).cloned() {
            let still_in_use = ipam
                .list(&network.name)
                .map(|m| !m.is_empty())
                .unwrap_or(true);
            if !still_in_use {
                let net_rs = wisp_net::iptables::plan_for_network(&network);
                let _ = wisp_net::iptables::revoke(&net_rs);
                let _ = wisp_net::bridge::delete(&network);
            }
        }
        Ok(())
    }
}

#[test]
#[ignore = "needs root + linux"]
fn multi_network_container_attaches_eth0_and_eth1() {
    if common::requires_root("multi_network_container_attaches_eth0_and_eth1") {
        return;
    }

    // ip_forward so the FORWARD rules don't black-hole.
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let cgroup_root = common::unique_cgroup_root();
    let ipam_root = tmp.path().join("ipam");
    std::fs::create_dir_all(&ipam_root).unwrap();

    // Two distinct networks with non-overlapping subnets so the bridges
    // / iptables rules don't conflict. Use the same `99.x.y.z` family as
    // `runtime_with_network.rs` to avoid collisions with anything a
    // developer might have on the VM.
    let net_a = wisp_net::Network::new("wispmulti-a", "10.99.8.0/24".parse().unwrap()).unwrap();
    let net_b = wisp_net::Network::new("wispmulti-b", "10.99.9.0/24".parse().unwrap()).unwrap();

    // Fresh start: nuke any leftover state.
    for n in [&net_a, &net_b] {
        let _ = wisp_net::bridge::delete(n);
        let rs = wisp_net::iptables::plan_for_network(n);
        let _ = wisp_net::iptables::revoke(&rs);
    }

    let bundle = common::prepare_bundle(tmp.path(), &["/bin/sleep", "30"], &[], None);
    let runtime = common::isolated_runtime(&state_dir, &cgroup_root);
    let id = "multi";

    let specs = vec![
        NetworkSpec::new("wispmulti-a"),
        NetworkSpec::new("wispmulti-b"),
    ];

    let handle = runtime
        .create_with_networks(id, &bundle, specs.clone())
        .expect("create_with_networks");
    assert_eq!(handle.state, ContainerState::Created);
    assert_eq!(
        handle.network_specs.len(),
        2,
        "handle must carry both NetworkSpecs"
    );
    assert_eq!(handle.network_specs[0].network_name, "wispmulti-a");
    assert_eq!(handle.network_specs[1].network_name, "wispmulti-b");

    let mut attacher = MultiNetAttacher::new(&ipam_root);
    attacher.register(net_a.clone());
    attacher.register(net_b.clone());

    let start_result = runtime.start_with_attacher(id, &mut attacher);
    if let Err(e) = &start_result {
        eprintln!("start_with_attacher failed: {e}");
        let _ = runtime.delete_with_attacher(id, true, &mut attacher);
        for n in [&net_a, &net_b] {
            let _ = wisp_net::bridge::delete(n);
            let rs = wisp_net::iptables::plan_for_network(n);
            let _ = wisp_net::iptables::revoke(&rs);
        }
        common::teardown_cgroup_root(&cgroup_root);
    }
    start_result.expect("start_with_attacher");

    let live = runtime.state(id).expect("runtime.state");
    assert_eq!(
        live.network_attachments.len(),
        2,
        "state.json must carry both attachment records"
    );
    // Declaration order persisted: index 0 -> wispmulti-a, index 1 -> wispmulti-b.
    assert_eq!(live.network_attachments[0].network_name, "wispmulti-a");
    assert_eq!(live.network_attachments[1].network_name, "wispmulti-b");
    assert_eq!(live.network_attachments[0].bridge, net_a.bridge);
    assert_eq!(live.network_attachments[1].bridge, net_b.bridge);

    // Both veths visible inside the container's netns: eth0 + eth1.
    let pid = runtime.container_pid(id).expect("container_pid");
    let dev = std::fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap_or_default();
    assert!(dev.contains("eth0:"), "eth0 missing in netns: {dev}");
    assert!(dev.contains("eth1:"), "eth1 missing in netns: {dev}");

    // Primary network's gateway is the default route; we can ping it.
    let pid_s = pid.to_string();
    let ping_a = std::process::Command::new("nsenter")
        .args([
            "-t",
            &pid_s,
            "-n",
            "ping",
            "-c",
            "1",
            "-W",
            "2",
            &net_a.gateway.to_string(),
        ])
        .output()
        .expect("spawn nsenter ping (primary)");
    assert!(
        ping_a.status.success(),
        "ping primary gateway {} failed: {}",
        net_a.gateway,
        String::from_utf8_lossy(&ping_a.stderr),
    );

    // Secondary network: no default route via it, but the
    // on-link gateway is reachable through the interface directly.
    let ping_b = std::process::Command::new("nsenter")
        .args([
            "-t",
            &pid_s,
            "-n",
            "ping",
            "-c",
            "1",
            "-W",
            "2",
            &net_b.gateway.to_string(),
        ])
        .output()
        .expect("spawn nsenter ping (secondary)");
    assert!(
        ping_b.status.success(),
        "ping secondary gateway {} failed: {}",
        net_b.gateway,
        String::from_utf8_lossy(&ping_b.stderr),
    );

    // Kill PID 1, wait for Stopped, then delete + verify cleanup.
    runtime
        .kill(id, nix::sys::signal::Signal::SIGKILL)
        .expect("kill");
    let _ = common::wait_until_state(
        &runtime,
        id,
        ContainerState::Stopped,
        Duration::from_secs(3),
    );

    runtime
        .delete_with_attacher(id, true, &mut attacher)
        .expect("delete_with_attacher");

    let leftover = state_dir.join("containers").join(id);
    assert!(
        !leftover.exists(),
        "state-dir entry still exists: {}",
        leftover.display()
    );

    use wisp_net::Ipam as _;
    let listed_a = attacher
        .ipam_for("wispmulti-a")
        .list("wispmulti-a")
        .unwrap();
    assert!(
        !listed_a.contains_key(id),
        "ipam-a still has {id}: {listed_a:?}"
    );
    let listed_b = attacher
        .ipam_for("wispmulti-b")
        .list("wispmulti-b")
        .unwrap();
    assert!(
        !listed_b.contains_key(id),
        "ipam-b still has {id}: {listed_b:?}"
    );

    let bridges = wisp_net::bridge::list_wisp_bridges().unwrap();
    assert!(
        !bridges.contains(&net_a.bridge),
        "bridge a {} still present after detach: {:?}",
        net_a.bridge,
        bridges,
    );
    assert!(
        !bridges.contains(&net_b.bridge),
        "bridge b {} still present after detach: {:?}",
        net_b.bridge,
        bridges,
    );

    common::teardown_cgroup_root(&cgroup_root);
}
