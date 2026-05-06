//! Update-policy types shared between the storage DAO, the resolver, and the
//! updater plugin.
//!
//! See spec §"Policy struct (in `isengard-core`)" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! All `Policy` fields are `Option`. `None` means "inherit from a less-specific
//! scope". The implicit root resolved value (when no rows exist) is exposed as
//! the [`defaults`] module's constants.

pub mod labels;
pub mod resolve;

pub use labels::{ParseLabelError, has_any_policy_label, parse_policy_labels};
pub use resolve::{
    PolicyContext, PolicyOrigin, ResolvedPolicy, ResolvedProvenance, resolve_policy,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Discriminator for the polymorphic `scope_key` column. Mirrors the SQL
/// CHECK constraint in `0016_policies.sql`.
///
/// Lives in `isengard-core` so the pure resolver can sort and compare scopes
/// without taking a dependency on `isengard-storage`. Storage re-exports it
/// for backwards compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyScopeType {
    Global,
    Fleet,
    Stack,
    Service,
    Container,
}

impl PolicyScopeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Fleet => "fleet",
            Self::Stack => "stack",
            Self::Service => "service",
            Self::Container => "container",
        }
    }

    /// Specificity rank: smaller wins, so `Global` (rank 0) is overridden by
    /// every other scope. Used to order layered resolution.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Global => 0,
            Self::Fleet => 1,
            Self::Stack => 2,
            Self::Service => 3,
            Self::Container => 4,
        }
    }
}

/// Error returned when parsing an unknown scope-type string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPolicyScopeType(pub String);

impl std::fmt::Display for UnknownPolicyScopeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown policy scope_type: {}", self.0)
    }
}

impl std::error::Error for UnknownPolicyScopeType {}

impl FromStr for PolicyScopeType {
    type Err = UnknownPolicyScopeType;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "global" => Ok(Self::Global),
            "fleet" => Ok(Self::Fleet),
            "stack" => Ok(Self::Stack),
            "service" => Ok(Self::Service),
            "container" => Ok(Self::Container),
            other => Err(UnknownPolicyScopeType(other.to_string())),
        }
    }
}

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

    #[test]
    fn scope_type_round_trips_through_str() {
        for s in [
            PolicyScopeType::Global,
            PolicyScopeType::Fleet,
            PolicyScopeType::Stack,
            PolicyScopeType::Service,
            PolicyScopeType::Container,
        ] {
            let parsed: PolicyScopeType = s.as_str().parse().expect("parse roundtrip");
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn scope_type_rank_orders_by_specificity() {
        assert!(PolicyScopeType::Global.rank() < PolicyScopeType::Fleet.rank());
        assert!(PolicyScopeType::Fleet.rank() < PolicyScopeType::Stack.rank());
        assert!(PolicyScopeType::Stack.rank() < PolicyScopeType::Service.rank());
        assert!(PolicyScopeType::Service.rank() < PolicyScopeType::Container.rank());
    }

    #[test]
    fn unknown_scope_type_string_errors() {
        let err = "frobozz".parse::<PolicyScopeType>().unwrap_err();
        assert_eq!(err.0, "frobozz");
        assert!(format!("{err}").contains("unknown policy scope_type"));
    }
}
