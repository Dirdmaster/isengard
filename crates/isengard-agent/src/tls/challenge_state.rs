//! Shared HashMap<token, key_authorization> between the ACME order task
//! and the :8080 HTTP-01 challenge handler. Tokens are short-lived
//! (LE validates within seconds).

use std::collections::HashMap;
use tokio::sync::RwLock;

/// Shared state for ACME HTTP-01 challenge tokens.
///
/// The ACME order task installs `(token, key_authorization)` entries
/// before triggering validation; the :8080 HTTP server looks up tokens
/// from `/.well-known/acme-challenge/<token>` and returns the key
/// authorization body. Entries are removed once the order completes
/// (success or failure).
#[derive(Default)]
pub struct ChallengeState {
    inner: RwLock<HashMap<String, String>>,
}

impl ChallengeState {
    /// Create an empty challenge state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a token -> key_authorization mapping.
    pub async fn install(&self, token: &str, key_authorization: &str) {
        let mut guard = self.inner.write().await;
        guard.insert(token.to_string(), key_authorization.to_string());
    }

    /// Look up the key authorization for a token, if present.
    pub async fn lookup(&self, token: &str) -> Option<String> {
        let guard = self.inner.read().await;
        guard.get(token).cloned()
    }

    /// Remove a token mapping (called after order completion).
    pub async fn remove(&self, token: &str) {
        let mut guard = self.inner.write().await;
        guard.remove(token);
    }
}
