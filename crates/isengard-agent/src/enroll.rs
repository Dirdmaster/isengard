//! Agent → controller enrollment.
//!
//! NOTE(task-10): This module is stubbed out while Task 10 lands the
//! `cert_store` module. The Phase 14 proto refactor (Task 5) renamed
//! `EnrollRequest`/`EnrollResponse` fields, so the old hostname/fingerprint
//! flow no longer compiles. Task 11 will rewrite this module against the new
//! token-redemption flow that returns a cert bundle, and re-wire `run_agent`
//! to persist the bundle via `cert_store::save`.

#![allow(clippy::result_large_err)]
#![allow(dead_code)]

use crate::Result;

/// Resolved host metadata included in the EnrollRequest.
///
/// Kept around as a placeholder so other modules that import the type still
/// compile. Task 11 will rewrite the body to match the new proto.
pub struct HostInfo {
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
}

impl HostInfo {
    /// Best-effort host detection. Stubbed pending Task 11; the values are
    /// still populated so the type behaves sensibly if anything constructs
    /// one in the meantime.
    pub fn detect() -> Self {
        let hostname = hostname::get()
            .ok()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| "unknown-host".into());

        Self {
            fingerprint: hostname.clone(),
            hostname,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version: String::new(),
        }
    }
}

/// Issue an Enroll RPC against the configured controller.
///
/// TODO(task-11): rewrite for the new proto. The Phase 14 flow takes a
/// one-time enrollment token, returns a signed cert bundle, and the agent
/// persists it via `cert_store::save`. Until that lands this just panics if
/// called — `run_agent` itself is also stubbed in Task 11.
pub async fn enroll(_controller_url: &str, _token: &str, _info: HostInfo) -> Result<String> {
    unimplemented!("rewritten in task 11")
}
