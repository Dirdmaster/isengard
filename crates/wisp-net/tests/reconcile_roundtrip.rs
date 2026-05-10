//! Reconcile-pass end-to-end on a real Linux host.
//!
//! Creates a fake bridge + veth pair + iptables rule using wisp-net's
//! naming conventions but with NO registry entry under
//! `<state_dir>/networks/`, calls `reconcile`, and asserts the kernel
//! state is gone afterwards.
//!
//! `#[ignore]` so `cargo test` on Mac stays green. Run with:
//!
//! ```sh
//! orb -m wisp ... cargo test -p wisp-net --tests -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;

#[test]
#[ignore = "needs root + linux"]
fn reconcile_cleans_orphan_bridge_veth_and_iptables() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping: not running as root");
        return;
    }

    // Pick names that don't collide with anything real.
    let bridge = "wbr-reconcile-tst";
    let veth_host = "wveth-h-rec1";
    let veth_ctr = "wveth-c-rec1";
    let comment = "wisp:reconcile-tst:masq";

    // -- pre-clean (in case a prior failed run left junk) --------------
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", bridge])
        .output();
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", veth_host])
        .output();
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", veth_ctr])
        .output();
    let _ = std::process::Command::new("iptables")
        .args([
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-j",
            "MASQUERADE",
            "-m",
            "comment",
            "--comment",
            comment,
        ])
        .output();

    // -- seed orphan kernel state --------------------------------------
    // 1. orphan bridge: wbr-* with no registry entry.
    let bridge_out = std::process::Command::new("ip")
        .args(["link", "add", "name", bridge, "type", "bridge"])
        .output()
        .expect("ip link add bridge");
    assert!(
        bridge_out.status.success(),
        "create bridge: {}",
        String::from_utf8_lossy(&bridge_out.stderr)
    );

    // 2. orphan veth pair: wveth-h- + wveth-c- with no master.
    let veth_out = std::process::Command::new("ip")
        .args([
            "link", "add", veth_host, "type", "veth", "peer", "name", veth_ctr,
        ])
        .output()
        .expect("ip link add veth");
    assert!(
        veth_out.status.success(),
        "create veth: {}",
        String::from_utf8_lossy(&veth_out.stderr)
    );

    // 3. orphan iptables rule tagged with our reconcile scope.
    let iptables_out = std::process::Command::new("iptables")
        .args([
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-j",
            "MASQUERADE",
            "-m",
            "comment",
            "--comment",
            comment,
        ])
        .output()
        .expect("iptables -A");
    assert!(
        iptables_out.status.success(),
        "add iptables rule: {}",
        String::from_utf8_lossy(&iptables_out.stderr)
    );

    // -- reconcile against an EMPTY registry ---------------------------
    let tmp = tempfile::tempdir().expect("tempdir");
    let networks_dir = tmp.path().join("networks");
    std::fs::create_dir_all(&networks_dir).expect("create networks dir");

    let known: BTreeSet<String> = BTreeSet::new();
    let report = wisp_net::reconcile(wisp_net::ReconcileInputs {
        networks_dir: &networks_dir,
        known_container_ids: Some(&known),
    })
    .expect("reconcile");

    // -- assertions ----------------------------------------------------
    assert!(
        report.orphan_bridges.iter().any(|b| b == bridge),
        "expected {bridge} in {:?}",
        report.orphan_bridges,
    );
    assert!(
        report
            .orphan_veths
            .iter()
            .any(|v| v == veth_host || v == veth_ctr),
        "expected veth halves in {:?}",
        report.orphan_veths,
    );
    assert!(
        report
            .orphan_iptables_scopes
            .iter()
            .any(|s| s == "wisp:reconcile-tst"),
        "expected wisp:reconcile-tst in {:?}",
        report.orphan_iptables_scopes,
    );

    // Kernel side: every artifact should be gone.
    let bridges = wisp_net::bridge::list_wisp_bridges().expect("post-reconcile list_wisp_bridges");
    assert!(
        !bridges.iter().any(|b| b == bridge),
        "bridge {bridge} should be gone, still present in {bridges:?}",
    );

    let still_veth = std::process::Command::new("ip")
        .args(["link", "show", veth_host])
        .output()
        .expect("ip link show");
    assert!(
        !still_veth.status.success(),
        "veth host stub {veth_host} should be gone",
    );

    let iptables_save = std::process::Command::new("iptables-save")
        .args(["-t", "nat"])
        .output()
        .expect("iptables-save");
    let dump = String::from_utf8_lossy(&iptables_save.stdout);
    assert!(
        !dump.contains(comment),
        "iptables rule {comment} should be revoked, dump:\n{dump}"
    );

    // -- idempotent second pass ---------------------------------------
    let report2 = wisp_net::reconcile(wisp_net::ReconcileInputs {
        networks_dir: &networks_dir,
        known_container_ids: Some(&known),
    })
    .expect("reconcile (second pass)");
    assert!(
        report2.is_clean(),
        "second pass should be a noop, got {report2:?}"
    );
}

#[test]
#[ignore = "needs root + linux"]
fn reconcile_preserves_registered_bridge_and_rules() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping: not running as root");
        return;
    }

    let name = "reconcile-keep";
    let bridge = "wbr-reconcile-kee"; // truncated to IFNAMSIZ - 1

    // -- pre-clean ---------------------------------------------------
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", bridge])
        .output();

    // -- seed a registered bridge + registry entry -------------------
    let bridge_out = std::process::Command::new("ip")
        .args(["link", "add", "name", bridge, "type", "bridge"])
        .output()
        .expect("ip link add bridge");
    assert!(bridge_out.status.success());

    let tmp = tempfile::tempdir().expect("tempdir");
    let entry_dir = tmp.path().join("networks").join(name);
    std::fs::create_dir_all(&entry_dir).expect("create entry dir");
    let entry_json = format!(
        r#"{{"name":"{name}","subnet":"10.83.0.0/24","gateway":"10.83.0.1","bridge":"{bridge}"}}"#
    );
    std::fs::write(entry_dir.join("network.json"), entry_json).expect("write entry");

    // Reconcile: should NOT clean this bridge.
    let known: BTreeSet<String> = BTreeSet::new();
    let report = wisp_net::reconcile(wisp_net::ReconcileInputs {
        networks_dir: &tmp.path().join("networks"),
        known_container_ids: Some(&known),
    })
    .expect("reconcile");

    assert!(
        !report.orphan_bridges.iter().any(|b| b == bridge),
        "registered bridge {bridge} should NOT be in {:?}",
        report.orphan_bridges,
    );

    let bridges = wisp_net::bridge::list_wisp_bridges().expect("list");
    assert!(
        bridges.iter().any(|b| b == bridge),
        "registered bridge should still exist"
    );

    // Cleanup.
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", bridge])
        .output();
}
