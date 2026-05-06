//! Update-policy types shared between the storage DAO, the resolver, and the
//! updater plugin.
//!
//! See spec §"Policy struct (in `isengard-core`)" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! All `Policy` fields are `Option`. `None` means "inherit from a less-specific
//! scope". The implicit root resolved value (when no rows exist) is exposed as
//! the [`defaults`] module's constants.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How aggressively the updater should bump a service's image.
///
/// `Pinned` is the strongest constraint: never update. `TagOnly` updates only
/// when the resolved tag's digest changes (no semver shift). `Minor` allows
/// patch+minor bumps once Phase 9i lands. `Any` is the loosest: take whatever
/// the registry serves for the configured tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStrategy {
    Pinned,
    TagOnly,
    Minor,
    Any,
}

/// Whether the updater applies an eligible update on its own, asks for human
/// approval, or is blocked entirely.
///
/// `Approval` is data-modeled here but not yet enforced: enforcement lands in
/// Phase 9e. Until then, REST writes that set `gate=Approval` are rejected at
/// the API layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateGate {
    Auto,
    Approval,
    Never,
}

/// What to do when an update fails health checks.
///
/// `Rollback` couples with Phase 10's blue-green collapse recovery and lands
/// in Phase 9j. `Keep` leaves the broken green up for forensic inspection.
/// `Notify` is the safe default: emit an event, leave the previous container
/// in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureHandling {
    Rollback,
    Keep,
    Notify,
}

/// One layer of an update-policy. A `None` field means "inherit from a less
/// specific scope". The actual layered resolution lives in T2's resolver.
///
/// Note: `window` (maintenance window) is deferred to Phase 9h. The field is
/// intentionally absent here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub strategy: Option<UpdateStrategy>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gate: Option<UpdateGate>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub paused_until: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub on_failure: Option<FailureHandling>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub approver_channel: Option<String>,
}

/// Resolver fall-back constants. These are the values the resolver uses for
/// any field that ends up `None` after walking every applicable row.
pub mod defaults {
    use super::{FailureHandling, UpdateGate, UpdateStrategy};

    pub const DEFAULT_STRATEGY: UpdateStrategy = UpdateStrategy::TagOnly;
    pub const DEFAULT_GATE: UpdateGate = UpdateGate::Auto;
    pub const DEFAULT_ON_FAILURE: FailureHandling = FailureHandling::Notify;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_all_none() {
        let p = Policy::default();
        assert!(p.strategy.is_none());
        assert!(p.gate.is_none());
        assert!(p.paused_until.is_none());
        assert!(p.on_failure.is_none());
        assert!(p.approver_channel.is_none());
    }

    #[test]
    fn defaults_module_exposes_constants() {
        assert_eq!(defaults::DEFAULT_STRATEGY, UpdateStrategy::TagOnly);
        assert_eq!(defaults::DEFAULT_GATE, UpdateGate::Auto);
        assert_eq!(defaults::DEFAULT_ON_FAILURE, FailureHandling::Notify);
    }

    #[test]
    fn enums_serialize_kebab_case() {
        assert_eq!(
            serde_json::to_string(&UpdateStrategy::TagOnly).unwrap(),
            "\"tag-only\""
        );
        assert_eq!(
            serde_json::to_string(&UpdateGate::Approval).unwrap(),
            "\"approval\""
        );
        assert_eq!(
            serde_json::to_string(&FailureHandling::Rollback).unwrap(),
            "\"rollback\""
        );
    }

    #[test]
    fn policy_round_trips_through_json() {
        let p = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            gate: Some(UpdateGate::Never),
            paused_until: None,
            on_failure: Some(FailureHandling::Keep),
            approver_channel: Some("ops".to_string()),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: Policy = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }
}
