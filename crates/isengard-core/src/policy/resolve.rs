#![doc = include_str!("../../docs/policy-resolve.md")]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    ExternalGate, FailureHandling, MaintenanceWindow, Policy, PolicyScopeType, UpdateGate,
    UpdateStrategy,
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
    /// Fleet name the target host belongs to.
    pub fleet: Option<&'a str>,
    /// Compose stack name the target container belongs to.
    pub stack: Option<&'a str>,
    /// Service name within the stack.
    pub service: Option<&'a str>,
    /// Host id in hex form. Paired with `container_name` for
    /// container-scoped lookups.
    pub host_id_hex: Option<&'a str>,
    /// Docker container name. Paired with `host_id_hex` for
    /// container-scoped lookups.
    pub container_name: Option<&'a str>,
}

/// The scope a resolved field came from.
///
/// [`Default`](Self::Default) means no row supplied the field and the
/// implicit root constant (see [`super::defaults`]) was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyOrigin {
    /// No row supplied this field; the root default constant was used.
    Default,
    /// Field came from a `Global` row.
    Global,
    /// Field came from a `Fleet` row.
    Fleet,
    /// Field came from a `Stack` row.
    Stack,
    /// Field came from a `Service` row.
    Service,
    /// Field came from a `Container` row.
    Container,
}

impl PolicyOrigin {
    /// Map a [`PolicyScopeType`] to the matching [`PolicyOrigin`].
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
    /// Where the resolved `strategy` came from.
    pub strategy: PolicyOrigin,
    /// Where the resolved `gate` came from.
    pub gate: PolicyOrigin,
    /// Where the resolved `paused_until` came from.
    pub paused_until: PolicyOrigin,
    /// Where the resolved `on_failure` came from.
    pub on_failure: PolicyOrigin,
    /// Where the resolved `approver_channel` came from.
    pub approver_channel: PolicyOrigin,
    /// Where the resolved `window` came from.
    pub window: PolicyOrigin,
    /// Where the resolved `external_gate` came from.
    pub external_gate: PolicyOrigin,
}

impl Default for ResolvedProvenance {
    fn default() -> Self {
        Self {
            strategy: PolicyOrigin::Default,
            gate: PolicyOrigin::Default,
            paused_until: PolicyOrigin::Default,
            on_failure: PolicyOrigin::Default,
            approver_channel: PolicyOrigin::Default,
            window: PolicyOrigin::Default,
            external_gate: PolicyOrigin::Default,
        }
    }
}

/// The fully resolved policy for a target.
///
/// Every field has a concrete value (or `None` for the genuinely optional
/// fields: `paused_until`, `approver_channel`, `window`, and
/// `external_gate`) and a recorded origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPolicy {
    /// Effective update strategy.
    pub strategy: UpdateStrategy,
    /// Effective approval gating.
    pub gate: UpdateGate,
    /// Pause-until instant, if set.
    pub paused_until: Option<DateTime<Utc>>,
    /// Effective failure-handling mode.
    pub on_failure: FailureHandling,
    /// Notifier channel id, if set.
    pub approver_channel: Option<String>,
    /// Maintenance window. `None` means "no window constraint".
    pub window: Option<MaintenanceWindow>,
    /// External-action gate.
    ///
    /// `None` means "no gate"; the updater proceeds to the existing
    /// decision logic.
    pub external_gate: Option<ExternalGate>,
    /// Per-field origin tracking.
    pub provenance: ResolvedProvenance,
}

/// Resolve the effective policy for the given context.
///
/// `rows` is a slice of `(scope_type, scope_key, body)` tuples. The
/// function is total and side-effect free: no I/O, no allocation beyond
/// the optional `approver_channel` clone.
///
/// See the module-level docs for the full algorithm.
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
    let mut window: Option<MaintenanceWindow> = None;
    let mut external_gate: Option<ExternalGate> = None;

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
        if let Some(v) = &body.window {
            window = Some(v.clone());
            prov.window = origin;
        }
        if let Some(v) = &body.external_gate {
            external_gate = Some(v.clone());
            prov.external_gate = origin;
        }
    }

    ResolvedPolicy {
        strategy: strategy.unwrap_or(DEFAULT_STRATEGY),
        gate: gate.unwrap_or(DEFAULT_GATE),
        paused_until,
        on_failure: on_failure.unwrap_or(DEFAULT_ON_FAILURE),
        approver_channel,
        window,
        external_gate,
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
