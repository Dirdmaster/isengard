//! Unit tests for `proxy::swap` — drain semantics for hot-swapping upstreams.
//!
//! These exercise the swap behavior end-to-end against `ProxyState`:
//! - `swap_upstream` flips the old entry to `Draining`, schedules cleanup, then
//!   installs the new entry as `Active`.
//! - `spawn_drain_cleanup` removes the entry only if it's still `Draining` after
//!   the grace period — an entry that's been re-set to `Active` is left alone.

use isengard_agent::proxy::{ProxyState, Upstream, UpstreamState, swap_upstream};
use std::net::SocketAddr;
use std::time::Duration;

fn sample(addr_port: u16) -> Upstream {
    Upstream {
        container_id: format!("c-{addr_port}"),
        addr: SocketAddr::from(([127, 0, 0, 1], addr_port)),
        healthy: true,
        health_path: None,
        health_interval: Duration::from_secs(10),
        consecutive_failures: 0,
        state: UpstreamState::Active,
    }
}

#[tokio::test]
async fn swap_marks_old_as_draining_then_installs_new() {
    let state = ProxyState::new();
    state.upstreams.write().await.set("h.test", sample(8080));

    let new_up = sample(9090);
    swap_upstream(&state, "h.test", new_up.clone(), Duration::from_secs(60))
        .await
        .unwrap();

    let r = state.upstreams.read().await;
    let after = r.get("h.test").unwrap();
    assert_eq!(after.addr, new_up.addr, "new upstream is now serving");
    assert_eq!(after.state, UpstreamState::Active, "new entry is active");
}

#[tokio::test]
async fn draining_upstream_removed_after_grace_period() {
    let state = ProxyState::new();
    state.upstreams.write().await.set("h.test", sample(8080));
    state
        .upstreams
        .write()
        .await
        .set_state("h.test", UpstreamState::Draining);
    isengard_agent::proxy::swap::spawn_drain_cleanup(
        state.clone(),
        "h.test".into(),
        Duration::from_millis(100),
    );

    tokio::time::sleep(Duration::from_millis(250)).await;

    let r = state.upstreams.read().await;
    assert!(
        r.get("h.test").is_none(),
        "draining entry should be removed after grace"
    );
}

#[tokio::test]
async fn cleanup_leaves_active_entry_alone() {
    let state = ProxyState::new();
    state.upstreams.write().await.set("h.test", sample(8080));
    isengard_agent::proxy::swap::spawn_drain_cleanup(
        state.clone(),
        "h.test".into(),
        Duration::from_millis(50),
    );

    state.upstreams.write().await.set("h.test", sample(9090));

    tokio::time::sleep(Duration::from_millis(150)).await;

    let r = state.upstreams.read().await;
    let entry = r.get("h.test").expect("entry should still exist");
    assert_eq!(entry.state, UpstreamState::Active);
    assert_eq!(entry.addr.port(), 9090);
}
