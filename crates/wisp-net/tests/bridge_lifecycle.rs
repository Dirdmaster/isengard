//! Bridge create / list / delete round-trip on a real Linux host.
//!
//! `#[ignore]` so `cargo test` on Mac stays green; run with
//! `cargo test -p wisp-net --tests -- --ignored` on the OrbStack
//! `wisp` VM as root.

#![cfg(target_os = "linux")]

#[test]
#[ignore = "needs root + linux"]
fn ensure_then_delete_round_trip() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping: not running as root");
        return;
    }

    let net = wisp_net::Network::new("wisptest", "10.99.0.0/24".parse().unwrap()).unwrap();

    // Best-effort cleanup from a prior failed run.
    let _ = wisp_net::bridge::delete(&net);

    wisp_net::bridge::ensure(&net).expect("ensure failed");
    let bridges = wisp_net::bridge::list_wisp_bridges().expect("list failed");
    assert!(
        bridges.iter().any(|b| b == &net.bridge),
        "bridge {} not in {:?}",
        net.bridge,
        bridges
    );

    // Calling ensure again is a no-op: matching addr already there.
    wisp_net::bridge::ensure(&net).expect("ensure (second call) failed");

    wisp_net::bridge::delete(&net).expect("delete failed");
    let bridges = wisp_net::bridge::list_wisp_bridges().expect("list failed (post-delete)");
    assert!(
        !bridges.iter().any(|b| b == &net.bridge),
        "bridge {} still present after delete: {:?}",
        net.bridge,
        bridges
    );

    // Repeat delete is idempotent (already-gone is OK).
    wisp_net::bridge::delete(&net).expect("delete (second call) failed");
}
