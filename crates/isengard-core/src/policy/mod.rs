//! Update-policy types shared between the storage DAO, the resolver, and
//! the updater plugin.
//!
//! See spec §"Policy struct (in `isengard-core`)" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! All [`Policy`] fields are `Option`. `None` means "inherit from a
//! less-specific scope". The implicit root resolved value (when no rows
//! exist) is exposed as the [`defaults`] module's constants.

pub mod gate;
pub mod labels;
pub mod resolve;
pub mod window;

pub use gate::{ExternalGate, GateDecision, GatePayload, GateResponse};
pub use labels::{ParseLabelError, has_any_policy_label, parse_policy_labels};
pub use resolve::{
    PolicyContext, PolicyOrigin, ResolvedPolicy, ResolvedProvenance, resolve_policy,
};
pub use window::{WINDOW_DURATION, is_in_window, next_window_after, parse_cron};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Discriminator for the polymorphic `scope_key` column.
///
/// Mirrors the SQL CHECK constraint in `0016_policies.sql`. Lives in
/// `isengard-core` so the pure resolver can sort and compare scopes
/// without taking a dependency on `isengard-storage`. Storage re-exports it
/// for backwards compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyScopeType {
    /// Applies to every host (rank 0).
    Global,
    /// Applies to all hosts in a fleet (rank 1).
    Fleet,
    /// Applies to every container in a stack (rank 2).
    Stack,
    /// Applies to every container of one service (rank 3).
    Service,
    /// Applies to one specific container (rank 4).
    Container,
}

impl PolicyScopeType {
    /// Stable kebab-case string used by storage and the resolver.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Fleet => "fleet",
            Self::Stack => "stack",
            Self::Service => "service",
            Self::Container => "container",
        }
    }

    /// Specificity rank: smaller wins, so [`Global`](Self::Global)
    /// (rank 0) is overridden by every other scope.
    ///
    /// Used to order layered resolution.
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
pub struct UnknownPolicyScopeType(
    /// The offending input string.
    pub String,
);

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
/// [`Pinned`](Self::Pinned) is the strongest constraint: never update.
/// [`TagOnly`](Self::TagOnly) updates only when the resolved tag's digest
/// changes (no semver shift). [`Minor`](Self::Minor) allows patch + minor
/// bumps. [`Any`](Self::Any) is the loosest: take whatever the registry
/// serves for the configured tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStrategy {
    /// Never update.
    Pinned,
    /// Update when the digest behind the same tag changes.
    TagOnly,
    /// Allow patch + minor semver bumps.
    Minor,
    /// Take whatever the registry serves for the configured tag.
    Any,
}

/// Whether the updater applies an eligible update on its own, asks for
/// human approval, or is blocked entirely.
///
/// [`Approval`](Self::Approval) is data-modeled here but not yet enforced:
/// enforcement lands in a later phase. Until then, REST writes that set
/// `gate=Approval` are rejected at the API layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateGate {
    /// Apply the update without asking.
    Auto,
    /// Persist a pending-approval row and wait for a human decision.
    Approval,
    /// Block the update.
    Never,
}

/// What to do when an update fails health checks.
///
/// [`Rollback`](Self::Rollback) couples with blue-green collapse recovery.
/// [`Keep`](Self::Keep) leaves the broken green up for forensic
/// inspection. [`Notify`](Self::Notify) is the safe default: emit an
/// event, leave the previous container in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureHandling {
    /// Roll back to the previous container.
    Rollback,
    /// Leave the broken new container up for forensic inspection.
    Keep,
    /// Emit an event, leave the previous container in place.
    Notify,
}

/// Maintenance window: when may updates apply?
///
/// A cron expression plus an optional IANA timezone name. `None` timezone
/// resolves as UTC.
///
/// The cron expression is standard 5-field syntax:
/// `minute hour day-of-month month day-of-week`. Uses the `croner` crate,
/// which accepts both 5- and 6-field forms; the UI and validation helper
/// text only document 5-field for clarity.
///
/// The window's effective duration (how long after a firing the cycle
/// considers itself "in window") is fixed at [`window::WINDOW_DURATION`]
/// (1h) for v1. Operators wanting longer windows use multiple cron lines
/// or step expressions (e.g. `0 2-4 * * 0`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceWindow {
    /// 5-field cron expression. See module docs for syntax.
    pub cron_expr: String,
    /// IANA timezone name (e.g. `"Europe/Zurich"`). `None` resolves to UTC.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timezone: Option<String>,
}

/// One layer of an update-policy.
///
/// A `None` field means "inherit from a less specific scope". The actual
/// layered resolution lives in the [`resolve`] module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Update aggressiveness for this layer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub strategy: Option<UpdateStrategy>,
    /// Approval gating for this layer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gate: Option<UpdateGate>,
    /// Pause updates until this UTC instant.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub paused_until: Option<DateTime<Utc>>,
    /// Failure handling override.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub on_failure: Option<FailureHandling>,
    /// Notifier channel id to route approval prompts to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub approver_channel: Option<String>,
    /// Maintenance window.
    ///
    /// `None` means "no window constraint"; updates may apply at any time.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window: Option<MaintenanceWindow>,
    /// External-action gate.
    ///
    /// `None` means "no gate"; the updater applies the existing
    /// post-policy logic. `Some(...)` makes the updater consult the gate
    /// URL before any update.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub external_gate: Option<ExternalGate>,
}

/// Resolver fall-back constants.
///
/// These are the values the resolver uses for any field that ends up
/// `None` after walking every applicable row.
pub mod defaults {
    use super::{FailureHandling, UpdateGate, UpdateStrategy};

    /// Default strategy when no policy row sets one: [`UpdateStrategy::TagOnly`].
    pub const DEFAULT_STRATEGY: UpdateStrategy = UpdateStrategy::TagOnly;
    /// Default gate when no policy row sets one: [`UpdateGate::Auto`].
    pub const DEFAULT_GATE: UpdateGate = UpdateGate::Auto;
    /// Default failure handling when no policy row sets one:
    /// [`FailureHandling::Notify`].
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
            window: None,
            external_gate: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: Policy = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    /// External_gate round-trips through JSON.
    #[test]
    fn policy_with_external_gate_round_trips() {
        let p = Policy {
            external_gate: Some(ExternalGate {
                url: "https://gate.example.com/decide".to_string(),
                secret: Some("shh".to_string()),
                timeout_secs: 30,
            }),
            ..Default::default()
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"external_gate\""));
        assert!(s.contains("\"timeout_secs\":30"));
        let back: Policy = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    /// Backwards-compat: rows persisted (no `external_gate`
    /// key) decode cleanly with `external_gate = None`.
    #[test]
    fn policy_without_external_gate_field_deserializes() {
        let json = r#"{"strategy":"pinned","gate":"auto"}"#;
        let p: Policy = serde_json::from_str(json).unwrap();
        assert!(p.external_gate.is_none());
    }

    #[test]
    fn policy_with_window_round_trips() {
        let p = Policy {
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: Some("Europe/Zurich".to_string()),
            }),
            ..Default::default()
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"window\""));
        assert!(s.contains("\"cron_expr\":\"0 2 * * 0\""));
        let back: Policy = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    /// Backwards-compat: rows persisted (no `window` key)
    /// must deserialize cleanly with `window = None`.
    #[test]
    fn policy_without_window_field_deserializes() {
        let json = r#"{"strategy":"pinned","gate":"auto"}"#;
        let p: Policy = serde_json::from_str(json).unwrap();
        assert!(p.window.is_none());
        assert_eq!(p.strategy, Some(UpdateStrategy::Pinned));
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
