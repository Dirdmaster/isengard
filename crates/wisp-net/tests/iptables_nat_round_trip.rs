//! Full bridge + veth + iptables NAT round-trip on a real Linux host.
//!
//! `#[ignore]` + Linux + root only. Sets up a bridge, allocates an IP,
//! drops a veth pair into a fresh netns, applies wisp iptables rules
//! including a `--port 22080:12080` DNAT, then spawns a tiny HTTP
//! listener inside the netns and curls it through the host port.

#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
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
fn nat_round_trip_curl_to_in_ns_listener() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping: not running as root");
        return;
    }

    // Use a unique bridge + subnet that doesn't collide with the VM's
    // default routes (10.0.x in OrbStack).
    let net = wisp_net::Network::new("wiptest", "10.99.1.0/24".parse().unwrap()).unwrap();
    let _ = wisp_net::bridge::delete(&net);

    wisp_net::bridge::ensure(&net).expect("bridge::ensure");

    // Enable IP forwarding so the FORWARD-chain rules can route
    // PREROUTING-DNAT'd packets to the bridge. Without this, even a
    // perfect ruleset will silently drop traffic.
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

    // Spawn the netns holder.
    let ns_child = Command::new("unshare")
        .args(["-n", "sleep", "60"])
        .spawn()
        .expect("spawn unshare");
    let ns_guard = ChildGuard(ns_child);
    let pid = ns_guard.0.id();

    // Allocate an IP for the container.
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let mut ipam = wisp_net::StaticBitmapIpam::new(tmpdir.path());
    let ctr_ip = ipam
        .alloc(&net.name, net.subnet, net.gateway, "ctr1")
        .expect("ipam::alloc");

    // Wire the veth pair into the netns.
    let pair = wisp_net::veth::create_pair().expect("create_pair");
    let attach_result = wisp_net::veth::attach_to_ns(
        &pair,
        pid,
        &net.bridge,
        ctr_ip,
        net.subnet.prefix_len(),
        net.gateway,
    );

    // Plan + apply iptables.
    let mut rs = wisp_net::iptables::plan_for_network(&net);
    let attachment = wisp_net::iptables::plan_for_attachment(
        &net,
        ctr_ip,
        "ctr1",
        &[wisp_net::PortPublish {
            host_ip: std::net::Ipv4Addr::UNSPECIFIED,
            host_port: 22080,
            container_port: 12080,
            protocol: wisp_net::PortProtocol::Tcp,
        }],
    );
    rs.extend(attachment);
    let apply_result = if attach_result.is_ok() {
        wisp_net::iptables::apply(&rs)
    } else {
        Err(wisp_net::WispNetError::Iptables(
            "skipped apply because attach failed".into(),
        ))
    };

    // Start a tiny HTTP listener inside the netns. python3 is on the
    // OrbStack image; netcat in Ubuntu's default install (OpenBSD nc)
    // doesn't support `-e`, so a python one-liner is the simplest
    // portable option.
    let listener_script = "import socket;\n\
        s=socket.socket();\n\
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1);\n\
        s.bind(('0.0.0.0',12080));\n\
        s.listen(1);\n\
        c,_=s.accept();\n\
        c.recv(4096);\n\
        c.send(b'HTTP/1.0 200 OK\\r\\nContent-Length: 5\\r\\nConnection: close\\r\\n\\r\\nhello');\n\
        c.close()";
    let listener_child = if apply_result.is_ok() {
        let pid_s = pid.to_string();
        Some(ChildGuard(
            Command::new("nsenter")
                .args(["-t", &pid_s, "-n", "python3", "-c", listener_script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn listener"),
        ))
    } else {
        None
    };

    // Curl from the host. Retry briefly so the listener has time to
    // bind. ~3s total: HTTP listener typically comes up in <100ms but
    // ARP / bridge learning can add latency on the first packet.
    let mut curl_out: Option<String> = None;
    let mut curl_err: Option<String> = None;
    if listener_child.is_some() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let r = Command::new("curl")
                .args(["-s", "--max-time", "1", "http://127.0.0.1:22080/"])
                .output();
            match r {
                Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                    curl_out = Some(String::from_utf8_lossy(&out.stdout).into_owned());
                    break;
                }
                Ok(out) => {
                    curl_err = Some(format!(
                        "exit {:?} stderr={} stdout={}",
                        out.status.code(),
                        String::from_utf8_lossy(&out.stderr),
                        String::from_utf8_lossy(&out.stdout)
                    ));
                }
                Err(e) => curl_err = Some(format!("spawn err: {e}")),
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    // Teardown order: kill the listener, revoke iptables, delete veth,
    // delete bridge, kill ns child. All best-effort.
    drop(listener_child);
    if apply_result.is_ok() {
        let _ = wisp_net::iptables::revoke(&rs);
    }
    let _ = wisp_net::veth::delete(&pair);
    let _ = wisp_net::bridge::delete(&net);
    drop(ns_guard);

    // Now panic with whatever went wrong.
    if let Err(e) = attach_result {
        panic!("attach_to_ns failed: {e:?}");
    }
    if let Err(e) = apply_result {
        panic!("iptables::apply failed: {e:?}");
    }
    let body = curl_out.unwrap_or_else(|| {
        panic!(
            "curl never returned a body. Last err: {}",
            curl_err.as_deref().unwrap_or("(none)")
        )
    });
    assert!(body.contains("hello"), "unexpected body: {body:?}");
}
