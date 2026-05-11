//! Veth pair create + move into a fresh netns + verify addressing.
//!
//! `#[ignore]` + Linux + root only. Spawns `unshare -n sleep 60` to get
//! a child with a private network namespace, then drives the wisp-net
//! veth/ns/bridge code against it.

#![cfg(target_os = "linux")]

use std::process::{Child, Command};
use wisp_net::Ipam as _;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs root + linux"]
fn veth_pair_moves_into_fresh_ns_with_addr() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping: not running as root");
        return;
    }

    // Bridge in a private subnet that doesn't collide with OrbStack's
    // eth0 (10.x).
    let net = wisp_net::Network::new("wvtest", "10.99.0.0/24".parse().unwrap()).unwrap();
    // Cleanup from a prior failed run.
    let _ = wisp_net::bridge::delete(&net);

    wisp_net::bridge::ensure(&net).expect("bridge::ensure");

    // Spawn a child in a fresh netns. `unshare -n` creates the netns
    // and execs sleep, which holds the ns alive for the duration of
    // the test.
    let child = Command::new("unshare")
        .args(["-n", "sleep", "60"])
        .spawn()
        .expect("spawn unshare");
    let guard = ChildGuard(child);
    let pid = guard.0.id();

    // Allocate an IP via the IPAM.
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let mut ipam = wisp_net::StaticBitmapIpam::new(tmpdir.path());
    let ip = ipam
        .alloc(&net.name, net.subnet, net.gateway, "ctr1")
        .expect("ipam::alloc");

    // Create + attach the veth pair.
    let pair = wisp_net::veth::create_pair().expect("create_pair");
    let result = wisp_net::veth::attach_to_ns(
        &pair,
        pid,
        &net.bridge,
        ip,
        net.subnet.prefix_len(),
        net.gateway,
        "eth0",
    );

    // Always tear down before unwrapping so a failure doesn't leak.
    let attach_ok = result.is_ok();
    let attach_err = format!("{:?}", result);

    let mut assertions: Result<(), String> = Ok(());
    if attach_ok {
        // Host: the host-side veth must exist.
        let host_show = wisp_net::cmd::run_ip(&["link", "show", &pair.host_name]);
        if host_show.is_err() {
            assertions = Err(format!("ip link show {} failed", pair.host_name));
        }
        // Inside the netns: eth0 must show the allocated IP.
        if assertions.is_ok() {
            let in_ns = wisp_net::ns::run_in_netns(pid, &["ip", "addr", "show", "eth0"])
                .unwrap_or_default();
            let want = format!("{}/{}", ip, net.subnet.prefix_len());
            if !in_ns.contains(&want) {
                assertions = Err(format!(
                    "expected '{}' in ns ip addr output: {}",
                    want, in_ns
                ));
            }
        }
    }

    // Teardown: revoke + bridge delete (kill child via ChildGuard drop).
    let _ = wisp_net::veth::delete(&pair);
    let _ = wisp_net::bridge::delete(&net);
    drop(guard);

    if !attach_ok {
        panic!("attach_to_ns failed: {attach_err}");
    }
    if let Err(e) = assertions {
        panic!("assertion failed: {e}");
    }
}
