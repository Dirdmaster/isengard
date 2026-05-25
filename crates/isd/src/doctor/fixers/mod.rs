//! Registry for doctor fixers.

use crate::doctor::{Finding, FixSpec};

/// Expose-host fixer implementation.
pub mod expose_host;

/// Structured input accepted by registered doctor fixers.
#[allow(dead_code)]
// Wired into command flow by a later doctor fixer task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixInput {
    /// Add `isengard.expose` labels to a compose service.
    ExposeHost(expose_host::ExposeHostInput),
}

/// Return the fixer id that can handle a finding.
#[allow(dead_code)]
// Wired into command flow by a later doctor fixer task.
pub fn fixer_id_for(finding: &Finding) -> Option<&'static str> {
    match &finding.fix {
        Some(FixSpec::ExposeService { .. }) => Some("EXPOSE_HOST_MISSING"),
        Some(FixSpec::SetExposePort { .. }) => Some(finding.id),
        None => None,
    }
}
