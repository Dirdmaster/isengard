//! Policy-aware skip helpers for the updater plugin (Phase 9b, T3).
//!
//! See spec §"Updater integration (9b)" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! The updater pulls a `LoadedPolicy` snapshot once per cycle and, for each
//! candidate container, builds a `PolicyContext` (fleet from the host row,
//! stack + service from compose labels, host_id_hex + container_name) and
//! calls [`isengard_core::resolve_policy`].
//!
//! Two skip rules are honoured at this slice:
//!
//! 1. `strategy == Pinned`: never update.
//! 2. `paused_until.is_some_and(|t| t > now())`: temporarily paused.
//!
//! Other resolved-policy fields (`gate`, `on_failure`, `approver_channel`)
//! are surfaced by the resolver but NOT enforced here. Phase 9e+ adds them.

use chrono::{DateTime, Utc};
use isengard_core::LoadedPolicy;
use isengard_core::policy::{
    PolicyContext, PolicyScopeType, ResolvedPolicy, UpdateStrategy, resolve_policy,
};
use std::collections::HashMap;

/// Compose project label. Used to derive the `stack` field of the
/// `PolicyContext`. Mirrors the standard label compose v2 emits.
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";

/// Compose service label. Already used by `dispatch_helpers::service_name`;
/// we read it again here so a candidate's PolicyContext can be built from
/// labels alone (no Docker re-inspect required).
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

/// Decision returned by [`policy_decision`]. Mirrors the cycle's branching:
/// `Skip` short-circuits with a reason that is then translated into a
/// `update.policy_skipped` event, while `Proceed` falls through to the
/// existing recreate path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Skip(SkipReason),
    Proceed,
}

/// Reason a candidate was skipped. Translated to the `reason` field of the
/// `update.policy_skipped` event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Pinned,
    Paused { until: DateTime<Utc> },
}

impl SkipReason {
    /// Stable string for the event payload. Stays stable across phases:
    /// downstream notifier rules key off these values.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Paused { .. } => "paused",
        }
    }
}

/// Owned variant of `PolicyContext` so the updater can build it from short-lived
/// strings (compose labels, host fleet name) without manual lifetime gymnastics.
/// Convert to a borrowed `PolicyContext<'_>` via [`OwnedPolicyContext::as_ref`].
#[derive(Debug, Clone, Default)]
pub struct OwnedPolicyContext {
    pub fleet: Option<String>,
    pub stack: Option<String>,
    pub service: Option<String>,
    pub host_id_hex: Option<String>,
    pub container_name: Option<String>,
}

impl OwnedPolicyContext {
    /// Borrow as a `PolicyContext<'_>`. The borrow lives as long as `self`.
    pub fn as_ref(&self) -> PolicyContext<'_> {
        PolicyContext {
            fleet: self.fleet.as_deref(),
            stack: self.stack.as_deref(),
            service: self.service.as_deref(),
            host_id_hex: self.host_id_hex.as_deref(),
            container_name: self.container_name.as_deref(),
        }
    }
}

/// Build an `OwnedPolicyContext` from the inputs the updater already has on
/// hand: the container's labels (compose project / service), the host's
/// fleet name (cached at plugin init), the host_id_hex, and the container
/// name.
///
/// `service` is derived from the compose-service label. The plan calls for
/// the compose project label to feed `stack`. If a candidate isn't a
/// compose-managed container both fields are `None`, which the resolver
/// treats as "no stack/service rows apply".
///
/// The plan keeps this strictly label-based: the dispatch_helpers helper
/// `service_name` falls back to the container name when the compose
/// service label is missing. We do NOT replicate that fallback here:
/// policy resolution should only match service-scoped rows when the
/// container actually identifies as a compose service. A standalone
/// container without compose labels resolves against fleet/global rows
/// only.
pub fn policy_context_from_container(
    labels: Option<&HashMap<String, String>>,
    fleet: Option<&str>,
    host_id_hex: Option<&str>,
    container_name: &str,
) -> OwnedPolicyContext {
    let stack = labels
        .and_then(|m| m.get(COMPOSE_PROJECT_LABEL))
        .map(String::from);
    let service = labels
        .and_then(|m| m.get(COMPOSE_SERVICE_LABEL))
        .map(String::from);

    OwnedPolicyContext {
        fleet: fleet.map(String::from),
        stack,
        service,
        host_id_hex: host_id_hex.map(String::from),
        container_name: Some(container_name.to_string()),
    }
}

/// Project an owned snapshot down to the `&[(scope, key, &Policy)]` shape
/// the resolver expects. Pure projection; no allocation beyond the outer
/// `Vec`.
pub fn project_for_resolver(
    snapshot: &[LoadedPolicy],
) -> Vec<(PolicyScopeType, &str, &isengard_core::policy::Policy)> {
    snapshot
        .iter()
        .map(|r| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect()
}

/// Apply the two Phase 9b skip rules to a resolved policy.
///
/// `now` is supplied by the caller so tests can drive the paused_until edge
/// without sleeping or freezing the clock.
pub fn decision_from_resolved(resolved: &ResolvedPolicy, now: DateTime<Utc>) -> PolicyDecision {
    if resolved.strategy == UpdateStrategy::Pinned {
        return PolicyDecision::Skip(SkipReason::Pinned);
    }
    if let Some(until) = resolved.paused_until {
        if until > now {
            return PolicyDecision::Skip(SkipReason::Paused { until });
        }
    }
    PolicyDecision::Proceed
}

/// One-shot helper composing the resolver + the skip rules. Used by the
/// integration tests so they don't have to assemble the resolver call by
/// hand.
pub fn policy_decision(
    snapshot: &[LoadedPolicy],
    ctx: &PolicyContext<'_>,
    now: DateTime<Utc>,
) -> (ResolvedPolicy, PolicyDecision) {
    let projected = project_for_resolver(snapshot);
    let resolved = resolve_policy(&projected, ctx);
    let decision = decision_from_resolved(&resolved, now);
    (resolved, decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use isengard_core::policy::{Policy, UpdateStrategy};

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn skip_reason_strings_are_stable() {
        assert_eq!(SkipReason::Pinned.as_str(), "pinned");
        assert_eq!(SkipReason::Paused { until: Utc::now() }.as_str(), "paused");
    }

    #[test]
    fn context_pulls_compose_project_and_service() {
        let l = labels(&[
            ("com.docker.compose.project", "blog"),
            ("com.docker.compose.service", "web"),
        ]);
        let ctx =
            policy_context_from_container(Some(&l), Some("prod"), Some("HOSTHEX"), "blog-web-1");
        assert_eq!(ctx.fleet.as_deref(), Some("prod"));
        assert_eq!(ctx.stack.as_deref(), Some("blog"));
        assert_eq!(ctx.service.as_deref(), Some("web"));
        assert_eq!(ctx.host_id_hex.as_deref(), Some("HOSTHEX"));
        assert_eq!(ctx.container_name.as_deref(), Some("blog-web-1"));
    }

    #[test]
    fn context_handles_missing_labels() {
        let ctx = policy_context_from_container(None, None, None, "lone-container");
        assert!(ctx.fleet.is_none());
        assert!(ctx.stack.is_none());
        assert!(ctx.service.is_none());
        assert_eq!(ctx.container_name.as_deref(), Some("lone-container"));
    }

    #[test]
    fn pinned_strategy_skips() {
        let policy = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Utc::now());
        assert_eq!(dec, PolicyDecision::Skip(SkipReason::Pinned));
    }

    #[test]
    fn paused_until_in_future_skips_with_until() {
        let until = Utc::now() + Duration::hours(1);
        let policy = Policy {
            paused_until: Some(until),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Utc::now());
        assert_eq!(dec, PolicyDecision::Skip(SkipReason::Paused { until }));
    }

    #[test]
    fn paused_until_in_past_does_not_skip() {
        let policy = Policy {
            paused_until: Some(Utc::now() - Duration::hours(1)),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Utc::now());
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    #[test]
    fn empty_snapshot_proceeds() {
        let snapshot: Vec<LoadedPolicy> = vec![];
        let owned = policy_context_from_container(None, None, None, "x");
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Utc::now());
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    #[test]
    fn service_scope_pinned_skips_matching_service() {
        let policy = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Service,
            scope_key: "web".into(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x").with_service("web".into());
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Utc::now());
        assert_eq!(dec, PolicyDecision::Skip(SkipReason::Pinned));
    }

    #[test]
    fn service_scope_pinned_does_not_skip_other_service() {
        let policy = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Service,
            scope_key: "web".into(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x").with_service("api".into());
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Utc::now());
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    impl OwnedPolicyContext {
        fn with_service(mut self, s: String) -> Self {
            self.service = Some(s);
            self
        }
    }
}
