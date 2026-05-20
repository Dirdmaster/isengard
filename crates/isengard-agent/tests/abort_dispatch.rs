//! Task 9: `DeploymentSupervisor::handle_abort`.
//!
//! These tests don't spin up a real Driver: they pre-register a
//! `CancellationToken` against a synthetic deployment id via the
//! `_test_register_abort_token` hook, then drive `handle_abort` directly.
//! That keeps the test fast (no Docker, no inventory churn) while still
//! exercising the lookup + cancel + return-value contract that the
//! `AbortDeployment` Sync handler depends on.

use std::sync::Arc;

use isengard_agent::deployment::DeploymentSupervisor;
use isengard_agent::proxy::ProxyState;
use isengard_core::NoopEmitter;
use isengard_storage::Inventory;
use tokio_util::sync::CancellationToken;

/// Build a `DeploymentSupervisor` with an in-memory inventory and a lazy
/// bollard handle (no socket touched). Mirrors the helper used in
/// `deployment::mod::supervisor_tests`.
async fn setup() -> DeploymentSupervisor {
    let inv = Inventory::open_in_memory().await.unwrap();
    let docker = Arc::new(bollard::Docker::connect_with_local_defaults().unwrap());
    let proxy_state = ProxyState::new();
    DeploymentSupervisor::new(inv, docker, proxy_state, Arc::new(NoopEmitter))
}

/// `handle_abort` for a registered deployment id cancels the token and
/// returns `true`.
#[tokio::test]
async fn handle_abort_known_id_cancels_and_returns_true() {
    let sup = setup().await;
    let token = CancellationToken::new();
    sup._test_register_abort_token("dep-123", token.clone());

    assert!(!token.is_cancelled(), "token starts uncancelled");
    let cancelled = sup.handle_abort("dep-123").await;
    assert!(cancelled, "handle_abort returns true for known id");
    assert!(
        token.is_cancelled(),
        "token should be cancelled after handle_abort"
    );
}

/// `handle_abort` for an unknown deployment id is a no-op and returns
/// `false`. Surfaces a stale dashboard request without disturbing
/// anything else.
#[tokio::test]
async fn handle_abort_unknown_id_returns_false() {
    let sup = setup().await;
    // Pre-register a token under a different id so the map is non-empty
    // and we're sure the lookup is doing real key matching.
    let other = CancellationToken::new();
    sup._test_register_abort_token("dep-other", other.clone());

    let cancelled = sup.handle_abort("dep-missing").await;
    assert!(!cancelled, "handle_abort returns false for unknown id");
    assert!(
        !other.is_cancelled(),
        "unrelated tokens are not touched on unknown-id abort"
    );
}
