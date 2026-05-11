//! Standalone demo: stand up a wisp bridge, attach a busybox-equivalent
//! sleep process to it, prove the netns sees `eth0` configured + can
//! ping the gateway, then tear everything down.
//!
//! Linux + root only. Run from inside the OrbStack `wisp` VM:
//!
//! ```sh
//! orb -m wisp -u root bash
//! cd /Users/dirdmaster/Projects/isengard/.worktrees/next
//! PATH=/home/dirdmaster/.cargo/bin:$PATH \
//!     cargo run -p wisp-net --example attach-busybox
//! ```
//!
//! The example uses `unshare -n sleep 30` instead of a real `clone3` +
//! `pivot_root` flow because the goal is to demonstrate the wisp-net
//! library APIs end to end without depending on `wisp-runtime`. The
//! production attach flow lives in `crates/wisp-cli/src/net_attacher.rs`
//! and is exercised by `cargo run -p wisp-cli -- run --network ...`.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("attach-busybox is Linux-only; run inside the OrbStack `wisp` VM as root");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use wisp_net::Ipam as _;

/// Reap the unshare child on exit so a panicking demo doesn't leave
/// `sleep 30` running for half a minute.
#[cfg(target_os = "linux")]
struct ChildGuard(Child);
#[cfg(target_os = "linux")]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("attach-busybox requires root: try `orb -m wisp -u root` first");
        std::process::exit(1);
    }

    // Best-effort host-global ip_forward. The demo doesn't need it
    // for the gateway ping (the bridge is on-link), but real attach
    // flows do, and turning it on costs nothing if it's already 1.
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

    // Unique bridge + subnet so a re-run after a previous failure
    // doesn't collide with leftover state. /24 + first-host gateway
    // give us 253 usable IPs.
    let net = wisp_net::Network::new("wisp-demo", "10.111.0.0/24".parse().unwrap())
        .expect("Network::new");
    eprintln!(
        "demo network: name={} bridge={} subnet={} gateway={}",
        net.name, net.bridge, net.subnet, net.gateway
    );

    // Clean up any straggler from a previous run before we start.
    let net_rs = wisp_net::iptables::plan_for_network(&net);
    let _ = wisp_net::iptables::revoke(&net_rs);
    let _ = wisp_net::bridge::delete(&net);

    // 1. Bridge.
    wisp_net::bridge::ensure(&net).expect("bridge::ensure");
    eprintln!("bridge ensured: {}", net.bridge);

    // 2. Network-level iptables (MASQUERADE + FORWARD allows).
    wisp_net::iptables::apply(&net_rs).expect("iptables::apply (network)");
    eprintln!(
        "iptables network rules applied: {} rule(s)",
        net_rs.creates.len()
    );

    // 3. Spawn an unshare-netns sleeper so we have a netns + PID we can
    //    attach to. `sleep 30` gives the demo plenty of room.
    let ns_child = Command::new("unshare")
        .args(["-n", "sleep", "30"])
        .spawn()
        .expect("spawn unshare");
    let pid = ns_child.id();
    let _guard = ChildGuard(ns_child);
    eprintln!("unshare child pid={pid}");

    // 4. Allocate an IP from the demo subnet.
    let ipam_dir = tempfile::tempdir().expect("ipam tempdir");
    let mut ipam = wisp_net::StaticBitmapIpam::new(ipam_dir.path());
    let ctr_ip = ipam
        .alloc(&net.name, net.subnet, net.gateway, "demo-busybox")
        .expect("ipam::alloc");
    eprintln!("allocated ip={ctr_ip} for demo-busybox");

    // 5. Veth pair + attach into the netns.
    let pair = wisp_net::veth::create_pair().expect("veth::create_pair");
    eprintln!(
        "veth pair: host={} ctr={}",
        pair.host_name, pair.container_name
    );
    wisp_net::veth::attach_to_ns(
        &pair,
        pid,
        &net.bridge,
        ctr_ip,
        net.subnet.prefix_len(),
        net.gateway,
        "eth0",
    )
    .expect("veth::attach_to_ns");
    eprintln!("veth attached + addr/route configured inside ns");

    // 6. Confirm eth0 is visible from outside via /proc/<pid>/net/dev.
    //    `unshare` shares /proc with the host, so reading the netns view
    //    of /proc means asking for the *netns's* /proc/net/dev, which
    //    /proc/<pid>/net/dev gives us.
    let dev = std::fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap_or_default();
    if !dev.contains("eth0:") {
        eprintln!("FAIL: eth0 missing from /proc/{pid}/net/dev:\n{dev}");
        cleanup(&net, &pair, &mut ipam, &net_rs);
        std::process::exit(2);
    }
    eprintln!("eth0 visible inside ns");

    // 7. Ping the gateway from inside the netns to prove the wiring works.
    let pid_s = pid.to_string();
    let started = Instant::now();
    let mut last_err = String::new();
    let mut ping_ok = false;
    while started.elapsed() < Duration::from_secs(3) {
        let out = Command::new("nsenter")
            .args([
                "-t",
                &pid_s,
                "-n",
                "ping",
                "-c",
                "1",
                "-W",
                "1",
                &net.gateway.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn nsenter ping");
        if out.status.success() {
            ping_ok = true;
            break;
        }
        last_err = String::from_utf8_lossy(&out.stderr).into_owned();
        std::thread::sleep(Duration::from_millis(200));
    }
    if !ping_ok {
        eprintln!("FAIL: ping {} from inside ns: {}", net.gateway, last_err);
        cleanup(&net, &pair, &mut ipam, &net_rs);
        std::process::exit(3);
    }
    eprintln!("ping {} succeeded from inside ns", net.gateway);

    // 8. Detach + clean up. Order mirrors NetworkAttacher::detach in
    //    crates/wisp-cli/src/net_attacher.rs.
    cleanup(&net, &pair, &mut ipam, &net_rs);

    eprintln!("demo OK: bridge + veth + iptables + IPAM round-trip clean");
}

#[cfg(target_os = "linux")]
fn cleanup(
    net: &wisp_net::Network,
    pair: &wisp_net::veth::VethPair,
    ipam: &mut wisp_net::StaticBitmapIpam,
    net_rs: &wisp_net::iptables::RuleSet,
) {
    let _ = wisp_net::veth::delete(pair);
    let _ = ipam.release(&net.name, "demo-busybox");
    let _ = wisp_net::iptables::revoke(net_rs);
    let _ = wisp_net::bridge::delete(net);
}
