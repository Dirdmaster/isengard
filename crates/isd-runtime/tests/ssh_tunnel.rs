//! Unit + integration tests for [`SshTunnel`].
//!
//! Port acquisition runs without external dependencies. The live ssh
//! test is gated behind `#[ignore]` because it requires SSH access to
//! localhost (passwordless via the operator's agent or default key);
//! run with `cargo test -p isd-runtime --test ssh_tunnel -- --ignored`.

use isd_runtime::SshTunnel;

#[tokio::test]
async fn acquire_local_port_returns_a_bound_port_in_user_range() {
    let port = SshTunnel::acquire_local_port().expect("port acquisition");
    assert!(port >= 1024, "port {port} below user range");
    // u16 caps at 65535 by type; the bind-then-release dance can in
    // theory hand back any of the ephemeral kernel-allocated ports.
}

#[tokio::test]
async fn acquire_local_port_returns_a_fresh_port_each_call() {
    let a = SshTunnel::acquire_local_port().expect("first port");
    let b = SshTunnel::acquire_local_port().expect("second port");
    assert_ne!(a, b, "port acquisition should not collide on the kernel");
}

#[tokio::test]
#[ignore]
async fn open_against_localhost_returns_a_live_tunnel() {
    let tunnel = SshTunnel::open("localhost").await.expect("tunnel open");
    let port = tunnel.local_port();
    assert!(port > 0);
    let host = tunnel.docker_host();
    assert!(host.starts_with("tcp://127.0.0.1:"));
    drop(tunnel);
}
