//! Pure policy resolver: walks layered scopes to produce a `ResolvedPolicy`
//! with field-level provenance.
//!
//! See spec §"Resolver" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! This module is intentionally storage-free. The caller (typically the
//! updater plugin or a REST handler) loads `PolicyRow` values from
//! `isengard-storage`, projects them down to `(PolicyScopeType, scope_key,
//! &Policy)` tuples, then hands them to [`resolve_policy`] together with a
//! [`PolicyContext`] describing the target.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    FailureHandling, Policy, PolicyScopeType, UpdateGate, UpdateStrategy,
    defaults::{DEFAULT_GATE, DEFAULT_ON_FAILURE, DEFAULT_STRATEGY},
};

/// The target whose effective policy we want to compute.
///
/// All fields are optional. When a field is `None`, rows of the matching
/// `scope_type` are filtered out. For example, a context with `fleet =
/// None` ignores every fleet-scoped row, even if some fleet row's
/// `scope_key` happens to be empty.
#[derive(Debug, Clone, Default)]
pub struct PolicyContext<'a> {
    pub fleet: Option<&'a str>,
    pub stack: Option<&'a str>,
    pub service: Option<&'a str>,
    pub host_id_hex: Option<&'a str>,
    pub container_name: Option<&'a str>,
}

/// The scope a resolved field came from. `Default` means no row supplied
/// the field and the implicit root constant (see
/// [`super::defaults`]) was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyOrigin {
    Default,
    Global,
    Fleet,
    Stack,
    Service,
    Container,
}

impl PolicyOrigin {
    fn from_scope(scope: PolicyScopeType) -> Self {
        match scope {
            PolicyScopeType::Global => Self::Global,
            PolicyScopeType::Fleet => Self::Fleet,
            PolicyScopeType::Stack => Self::Stack,
            PolicyScopeType::Service => Self::Service,
            PolicyScopeType::Container => Self::Container,
        }
    }
}

/// Per-field origin tracking. Mirrors the fields of [`ResolvedPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProvenance {
    pub strategy: PolicyOrigin,
    pub gate: PolicyOrigin,
    pub paused_until: PolicyOrigin,
    pub on_failure: PolicyOrigin,
    pub approver_channel: PolicyOrigin,
}

impl Default for ResolvedProvenance {
    fn default() -> Self {
        Self {
            strategy: PolicyOrigin::Default,
            gate: PolicyOrigin::Default,
            paused_until: PolicyOrigin::Default,
            on_failure: PolicyOrigin::Default,
            approver_channel: PolicyOrigin::Default,
        }
    }
}

/// The fully resolved policy for a target. Every field has a concrete value
/// (or `None` for the two genuinely optional fields, `paused_until` and
/// `approver_channel`) and a recorded origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPolicy {
    pub strategy: UpdateStrategy,
    pub gate: UpdateGate,
    pub paused_until: Option<DateTime<Utc>>,
    pub on_failure: FailureHandling,
    pub approver_channel: Option<String>,
    pub provenance: ResolvedProvenance,
}

/// Resolve the effective policy for the given context.
///
/// `rows` is a slice of `(scope_type, scope_key, body)` tuples. The
/// resolver:
///
/// 1. Filters rows that apply to `ctx` (a `Fleet` row only applies if
///    `ctx.fleet == Some(scope_key)`, etc; `Global` always applies).
/// 2. Sorts the survivors by `scope_type.rank()` ascending so more
///    specific scopes overwrite less specific ones.
/// 3. For each policy field, walks rows in rank order and overwrites
///    whenever the row's field is `Some`. The provenance for that field
///    is updated to the row's origin.
/// 4. Any field still unset after the walk falls back to the
///    `defaults::DEFAULT_*` constant with origin `Default`.
///
/// The function is total and side-effect free. It does no I/O and does
/// not allocate beyond the optional `approver_channel` clone.
pub fn resolve_policy(
    rows: &[(PolicyScopeType, &str, &Policy)],
    ctx: &PolicyContext<'_>,
) -> ResolvedPolicy {
    let mut applicable: Vec<(PolicyScopeType, &Policy)> = rows
        .iter()
        .filter_map(|(scope, key, body)| {
            if scope_applies(*scope, key, ctx) {
                Some((*scope, *body))
            } else {
                None
            }
        })
        .collect();
    // Stable sort by rank so equal-rank rows preserve input order. In
    // practice the (scope_type, scope_key) UNIQUE constraint upstream
    // means there can be at most one row per (scope_type, scope_key)
    // applicable to a given context, so equal-rank ties are rare.
    applicable.sort_by_key(|(scope, _)| scope.rank());

    let mut strategy: Option<UpdateStrategy> = None;
    let mut gate: Option<UpdateGate> = None;
    let mut paused_until: Option<DateTime<Utc>> = None;
    let mut on_failure: Option<FailureHandling> = None;
    let mut approver_channel: Option<String> = None;

    let mut prov = ResolvedProvenance::default();

    for (scope, body) in &applicable {
        let origin = PolicyOrigin::from_scope(*scope);
        if let Some(v) = body.strategy {
            strategy = Some(v);
            prov.strategy = origin;
        }
        if let Some(v) = body.gate {
            gate = Some(v);
            prov.gate = origin;
        }
        if let Some(v) = body.paused_until {
            paused_until = Some(v);
            prov.paused_until = origin;
        }
        if let Some(v) = body.on_failure {
            on_failure = Some(v);
            prov.on_failure = origin;
        }
        if let Some(v) = &body.approver_channel {
            approver_channel = Some(v.clone());
            prov.approver_channel = origin;
        }
    }

    ResolvedPolicy {
        strategy: strategy.unwrap_or(DEFAULT_STRATEGY),
        gate: gate.unwrap_or(DEFAULT_GATE),
        paused_until,
        on_failure: on_failure.unwrap_or(DEFAULT_ON_FAILURE),
        approver_channel,
        provenance: prov,
    }
}

/// Decides whether a row of the given `(scope_type, scope_key)` applies
/// to `ctx`.
///
/// Global rows always apply; their `scope_key` is conventionally empty
/// but the resolver does not enforce that here (the storage layer does).
fn scope_applies(scope: PolicyScopeType, key: &str, ctx: &PolicyContext<'_>) -> bool {
    match scope {
        PolicyScopeType::Global => true,
        PolicyScopeType::Fleet => ctx.fleet == Some(key),
        PolicyScopeType::Stack => ctx.stack == Some(key),
        PolicyScopeType::Service => ctx.service == Some(key),
        PolicyScopeType::Container => match (ctx.host_id_hex, ctx.container_name) {
            (Some(host), Some(name)) => {
                let expected = format!("{host}/{name}");
                expected == key
            }
            _ => false,
        },
    }
}
