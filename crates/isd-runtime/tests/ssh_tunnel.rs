//! Integration tests for [`isd_runtime::SshTunnel`].
//!
//! Port acquisition runs without external dependencies. The live ssh
//! test is gated `#[ignore]` because it requires SSH access to
//! localhost (passwordless via the operator's agent or default key);
//! run with `cargo test -p isd-runtime --test ssh_tunnel -- --ignored`.

use isd_runtime::SshTunnel;

/// Acquired ports land in the user range (>= 1024). The kernel can
/// hand back any ephemeral port; the lower bound is the assertion that
/// matters for an unprivileged binder.
#[tokio::test]
async fn acquire_local_port_returns_a_bound_port_in_user_range() {
    let port = SshTunnel::acquire_local_port().expect("port acquisition");
    assert!(port >= 1024, "port {port} below user range");
}

/// Two consecutive calls return distinct ports. Confirms the
/// bind-then-release dance does not collide with itself.
#[tokio::test]
async fn acquire_local_port_returns_a_fresh_port_each_call() {
    let a = SshTunnel::acquire_local_port().expect("first port");
    let b = SshTunnel::acquire_local_port().expect("second port");
    assert_ne!(a, b, "port acquisition should not collide on the kernel");
}

/// `open("localhost")` returns a live tunnel with a loopback
/// `docker_host` URL. Ignored: requires passwordless SSH to localhost.
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
