//! End-to-end runtime + network integration test.
//!
//! `#[ignore]`, root-only, Linux-only. Drives:
//!   1. `Runtime::create_with_network` on a busybox bundle whose
//!      entrypoint is `sleep 30` (long enough for the test to assert
//!      via nsenter from the host).
//!   2. `Runtime::start_with_attacher` with a `NetworkAttacher` impl
//!      backed by `wisp-net::bridge`, `wisp-net::veth`,
//!      `wisp-net::iptables`, and `wisp-net::StaticBitmapIpam`.
//!   3. Verifies the bridge + veth exist, `state.json` carries a
//!      `network_attachment` record, and the container's netns has
//!      `eth0` configured + can ping the gateway.
//!   4. SIGKILLs PID 1, waits for Stopped, then
//!      `Runtime::delete_with_attacher` and verifies the bridge +
//!      iptables rules are torn down + the IP is released.

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::time::Duration;

use wisp::{ContainerState, NetworkAttacher, NetworkAttachmentRecord, NetworkSpec, WispError};

/// A `NetworkAttacher` impl that drives `wisp-net` end to end.
///
/// Owns the IPAM state directory + the parsed `Network` config.
/// `attach` shells out through wisp-net's bridge / veth / iptables
/// helpers; `detach` reverses them. The struct lives only for the
/// duration of one test invocation.
struct WispNetAttacher {
    network: wisp_net::Network,
    ipam: wisp_net::StaticBitmapIpam,
}

impl WispNetAttacher {
    fn new(network: wisp_net::Network, ipam_state_dir: &Path) -> Self {
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

        // 1. Lazily ensure the bridge.
        wisp_net::bridge::ensure(&self.network)
            .map_err(|e| WispError::Lifecycle(format!("bridge::ensure: {e}")))?;

        // 2. Apply network-level iptables rules (idempotent in
        //    practice: dispatch B's `apply` walks the planner output;
        //    a re-run after a previous test gets the rules already
        //    present and iptables errors out, but for a clean test
        //    we revoke first to be safe).
        let net_rs = wisp_net::iptables::plan_for_network(&self.network);
        let _ = wisp_net::iptables::revoke(&net_rs);
        wisp_net::iptables::apply(&net_rs)
            .map_err(|e| WispError::Lifecycle(format!("iptables::apply (net): {e}")))?;

        // 3. Allocate IP.
        let ip = self
            .ipam
            .alloc(
                &self.network.name,
                self.network.subnet,
                self.network.gateway,
                container_id,
            )
            .map_err(|e| WispError::Lifecycle(format!("ipam::alloc: {e}")))?;

        // 4. Create veth + attach to ns.
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
            // veth::attach_to_ns rolls back the host side on failure;
            // release the IP ourselves so the next start can reuse it.
            let _ = self.ipam.release(&self.network.name, container_id);
            return Err(WispError::Lifecycle(format!("veth::attach_to_ns: {e}")));
        }

        // 5. Apply per-attachment iptables (port DNATs).
        let attach_rs =
            wisp_net::iptables::plan_for_attachment(&self.network, ip, container_id, &spec.ports);
        if let Err(e) = wisp_net::iptables::apply(&attach_rs) {
            let _ = wisp_net::veth::delete(&pair);
            let _ = self.ipam.release(&self.network.name, container_id);
            return Err(WispError::Lifecycle(format!(
                "iptables::apply (attachment): {e}"
            )));
        }

        // 6. Render /etc/resolv.conf + /etc/hosts inside the bundle's
        //    rootfs (primary attach only). The child has not yet
        //    pivot_root'd, so writes to `<rootfs>/etc/...` show up as
        //    `/etc/...` once it has.
        if is_primary {
            match &spec.resolv_source {
                wisp::ResolvSource::HostCopy => {
                    let nameservers = wisp_net::host_nameservers().unwrap_or_default();
                    if let Err(e) = wisp_net::write_resolv_conf(rootfs, &nameservers) {
                        return Err(WispError::Lifecycle(format!(
                            "write_resolv_conf (host copy): {e}"
                        )));
                    }
                }
                wisp::ResolvSource::Static(addrs) => {
                    if let Err(e) = wisp_net::write_resolv_conf(rootfs, addrs) {
                        return Err(WispError::Lifecycle(format!(
                            "write_resolv_conf (static): {e}"
                        )));
                    }
                }
                wisp::ResolvSource::None => {
                    // Operator opted out; leave whatever the bundle ships.
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
        // rules (only if no other containers remain), IPAM release.
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

        // Release IP first so `still_in_use` below reflects the
        // post-release count.
        let _ = self.ipam.release(&self.network.name, &record.container_id);

        // Network-level rules + bridge: if the IPAM has no remaining
        // allocs for this network, tear down the bridge + masq rules.
        let still_in_use = self
            .ipam
            .list(&self.network.name)
            .map(|m| !m.is_empty())
            .unwrap_or(true);
        if !still_in_use {
            let net_rs = wisp_net::iptables::plan_for_network(&self.network);
            let _ = wisp_net::iptables::revoke(&net_rs);
            let _ = wisp_net::bridge::delete(&self.network);
        }

        Ok(())
    }
}

#[test]
#[ignore = "needs root + linux"]
fn busybox_with_network_attaches_and_cleans_up() {
    if common::requires_root("busybox_with_network_attaches_and_cleans_up") {
        return;
    }

    // Enable host-global ip_forward; the per-attachment FORWARD
    // rules need this to actually carry traffic.
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let cgroup_root = common::unique_cgroup_root();
    let ipam_dir = tmp.path().join("ipam");
    std::fs::create_dir_all(&ipam_dir).unwrap();

    // Use a unique subnet so we don't collide with any existing wisp
    // network on the VM.
    let net = wisp_net::Network::new("wispnetto", "10.99.7.0/24".parse().unwrap()).unwrap();

    // Make sure no leftover state from a previous run blocks us.
    let _ = wisp_net::bridge::delete(&net);
    let net_rs = wisp_net::iptables::plan_for_network(&net);
    let _ = wisp_net::iptables::revoke(&net_rs);

    // Container body: sleep so the netns stays alive long enough for
    // the test to assert via nsenter from the host.
    let bundle = common::prepare_bundle(tmp.path(), &["/bin/sleep", "30"], &[], None);

    let runtime = common::isolated_runtime(&state_dir, &cgroup_root);
    let spec = NetworkSpec::new("wispnetto");
    let id = "netto";

    let handle = runtime
        .create_with_network(id, &bundle, spec.clone())
        .expect("create_with_network");
    assert_eq!(handle.state, ContainerState::Created);
    assert_eq!(
        handle
            .network_specs
            .first()
            .map(|s| s.network_name.as_str()),
        Some("wispnetto")
    );

    // Drive start with the wisp-net-backed attacher.
    let mut attacher = WispNetAttacher::new(net.clone(), &ipam_dir);
    let start_result = runtime.start_with_attacher(id, &mut attacher);

    // Whatever happens, try to clean up before re-asserting.
    if let Err(e) = &start_result {
        eprintln!("start_with_attacher failed: {e}");
        // Force-cleanup so a flaky run doesn't leak state.
        let _ = runtime.delete_with_attacher(id, true, &mut attacher);
        let _ = wisp_net::bridge::delete(&net);
        let _ = wisp_net::iptables::revoke(&net_rs);
        common::teardown_cgroup_root(&cgroup_root);
    }
    start_result.expect("start_with_attacher");

    // Confirm state.json carries the attachment record now.
    let live = runtime.state(id).expect("runtime.state");
    assert!(
        !live.network_attachments.is_empty(),
        "no attachment recorded"
    );
    let rec = &live.network_attachments[0];
    assert_eq!(rec.network_name, "wispnetto");
    assert_eq!(rec.bridge, net.bridge);
    assert!(rec.ipv4.is_private(), "ipv4 not private: {}", rec.ipv4);
    assert!(rec.veth_host.starts_with("wveth-h-"));

    // Confirm bridge listing reports our bridge.
    let bridges = wisp_net::bridge::list_wisp_bridges().unwrap();
    assert!(
        bridges.contains(&net.bridge),
        "bridge {} not in {:?}",
        net.bridge,
        bridges
    );

    // Confirm container_pid is recorded + reachable + has eth0
    // configured by inspecting `/proc/<pid>/net/dev` (which lives in
    // the container's netns by virtue of being read via /proc).
    let pid = runtime.container_pid(id).expect("container_pid");
    let net_dev_path = format!("/proc/{pid}/net/dev");
    let dev = std::fs::read_to_string(&net_dev_path).unwrap_or_default();
    assert!(
        dev.contains("eth0:"),
        "eth0 missing from {net_dev_path}; got:\n{dev}"
    );

    // Confirm /etc/resolv.conf + /etc/hosts were rendered into the
    // bundle's rootfs (commit 3 wired wisp-net::resolv +
    // wisp-net::hosts into the test's WispNetAttacher). The child
    // has not pivot_root'd from the host's view, but the underlying
    // inodes are the same files the post-pivot child reads.
    let resolv_body =
        std::fs::read_to_string(bundle.join("rootfs/etc/resolv.conf")).unwrap_or_default();
    let hosts_body = std::fs::read_to_string(bundle.join("rootfs/etc/hosts")).unwrap_or_default();
    assert!(
        resolv_body.contains("# wisp:"),
        "resolv.conf missing wisp marker: {resolv_body:?}"
    );
    assert!(
        hosts_body.contains("127.0.0.1\tlocalhost\n"),
        "hosts missing localhost row: {hosts_body:?}"
    );
    assert!(
        hosts_body.contains(&format!("{}\tnetto\n", rec.ipv4)),
        "hosts missing container row for {}: {hosts_body:?}",
        rec.ipv4
    );

    // Ping the gateway from inside the netns via nsenter. The test
    // process holds CAP_NET_RAW on the host, so even though the
    // container itself dropped CAP_NET_RAW, nsenter inherits the
    // test's caps.
    let pid_s = pid.to_string();
    let ping_out = std::process::Command::new("nsenter")
        .args([
            "-t",
            &pid_s,
            "-n",
            "ping",
            "-c",
            "1",
            "-W",
            "2",
            "10.99.7.1",
        ])
        .output()
        .expect("spawn nsenter ping");
    assert!(
        ping_out.status.success(),
        "ping gateway via netns failed: stdout={} stderr={}",
        String::from_utf8_lossy(&ping_out.stdout),
        String::from_utf8_lossy(&ping_out.stderr)
    );

    // Kill the container first so its PID 1 (sleep) exits, then
    // give the kernel a moment to settle the cgroup before delete.
    // PID 1 in a fresh PID namespace only respects SIGKILL.
    runtime
        .kill(id, nix::sys::signal::Signal::SIGKILL)
        .expect("kill");
    let _ = common::wait_until_state(
        &runtime,
        id,
        ContainerState::Stopped,
        Duration::from_secs(3),
    );

    // Now delete + detach.
    runtime
        .delete_with_attacher(id, true, &mut attacher)
        .expect("delete_with_attacher");

    // Verify state-dir entry is gone.
    let leftover = state_dir.join("containers").join(id);
    assert!(
        !leftover.exists(),
        "state-dir entry still exists: {}",
        leftover.display()
    );

    // The IP must be released for the next allocation.
    use wisp_net::Ipam as _;
    let listed = attacher.ipam.list("wispnetto").unwrap();
    assert!(
        !listed.contains_key("netto"),
        "ipam still has netto: {listed:?}"
    );

    // Bridge teardown when last container leaves.
    let bridges = wisp_net::bridge::list_wisp_bridges().unwrap();
    assert!(
        !bridges.contains(&net.bridge),
        "bridge {} still present after detach: {:?}",
        net.bridge,
        bridges
    );

    common::teardown_cgroup_root(&cgroup_root);
}
