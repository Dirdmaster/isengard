//! Policy-aware skip helpers for the updater plugin.
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
//! are surfaced by the resolver but NOT enforced here. A later phase adds them.

use chrono::{DateTime, Utc};
use isengard_core::approval_store::PendingApprovalBody;
use isengard_core::policy::{
    GateDecision, PolicyContext, PolicyScopeType, ResolvedPolicy, UpdateGate, UpdateStrategy,
    is_in_window, next_window_after, resolve_policy,
};
use isengard_core::{HostId, LoadedPolicy};
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
/// `update.policy_skipped` event, `Proceed` falls through to the existing
/// recreate path, `Deferred` emits `update.deferred` with the
/// next firing time, and `PendingApproval` parks the candidate
/// by persisting an approval row + emitting `update.pending_approval`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Skip this candidate and emit `update.policy_skipped`.
    Skip(SkipReason),
    /// Outside the resolved policy's maintenance window.
    /// `next_window` is when the next firing is expected, in UTC. Used by
    /// the cycle to populate the `update.deferred` event payload.
    Deferred {
        /// Next maintenance-window firing time. `None` when the cron
        /// has no future occurrence.
        next_window: Option<DateTime<Utc>>,
    },
    /// Fall through to the existing recreate / dispatch path.
    Proceed,
    /// `gate=Approval` resolved. The cycle persists this body via the
    /// `ApprovalStore` (idempotently) and emits `update.pending_approval`.
    PendingApproval(PendingApprovalBody),
}

/// Reason a candidate was skipped. Translated to the `reason` field of the
/// `update.policy_skipped` event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `strategy = Pinned`.
    Pinned,
    /// `paused_until` is in the future.
    Paused {
        /// Wall-clock time when the pause expires.
        until: DateTime<Utc>,
    },
    /// The configured `external_gate` returned `reject`.
    GateRejected {
        /// Reason text supplied by the gate, when any.
        reason: Option<String>,
    },
}

impl SkipReason {
    /// Stable string for the event payload. Stays stable across phases:
    /// downstream notifier rules key off these values.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Paused { .. } => "paused",
            Self::GateRejected { .. } => "gate_rejected",
        }
    }
}

/// Owned variant of `PolicyContext` so the updater can build it from short-lived
/// strings (compose labels, host fleet name) without manual lifetime gymnastics.
/// Convert to a borrowed `PolicyContext<'_>` via [`OwnedPolicyContext::as_ref`].
#[derive(Debug, Clone, Default)]
pub struct OwnedPolicyContext {
    /// Fleet name for the host.
    pub fleet: Option<String>,
    /// Compose project / stack name.
    pub stack: Option<String>,
    /// Compose service name.
    pub service: Option<String>,
    /// Host id rendered as hex.
    pub host_id_hex: Option<String>,
    /// Container name (final fallback for the resolver).
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

/// Digest + image context threaded into [`decision_from_resolved`] when the
/// caller already knows the candidate is `needs_update`. Used to build the
/// `PendingApproval` body. `None` for the early-cycle skip check (before
/// the registry probe runs); `Some` once the cycle has both digests in hand.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalContext<'a> {
    /// Image reference (with tag) the candidate is running.
    pub image: &'a str,
    /// Local digest already running on the host.
    pub current_digest: &'a str,
    /// Remote digest the registry returned.
    pub proposed_digest: &'a str,
}

/// Apply the skip rules + window check + approval gate to a resolved
/// policy.
///
/// Order:
///
/// 1. `strategy=Pinned` -> `Skip(Pinned)`.
/// 2. `paused_until > now` -> `Skip(Paused { until })`.
/// 3. `window.is_some() && !is_in_window(now)` -> `Deferred { next_window }`.
/// 4. `gate=Approval` AND `approval_ctx.is_some()` -> `PendingApproval(body)`.
/// 5. Otherwise -> `Proceed`.
///
/// The window check runs after Pinned and paused so a pinned-AND-windowed
/// service still emits `update.policy_skipped` (the more specific signal),
/// matching the design's edge-case table.
///
/// `approval_ctx` is `None` when the cycle is in its early skip phase
/// (digests not yet fetched). The `gate=Approval` branch only triggers
/// once the cycle has confirmed `needs_update` and has both digests
/// to thread into the body. Calling with `None` and `gate=Approval`
/// returns `Proceed` (the caller will re-invoke after the registry probe).
///
/// `now` is supplied by the caller so tests can drive the time-sensitive
/// edges (paused_until, window) without sleeping or freezing the clock.
pub fn decision_from_resolved(
    resolved: &ResolvedPolicy,
    ctx: &PolicyContext<'_>,
    approval_ctx: Option<ApprovalContext<'_>>,
    now: DateTime<Utc>,
) -> PolicyDecision {
    if resolved.strategy == UpdateStrategy::Pinned {
        return PolicyDecision::Skip(SkipReason::Pinned);
    }
    if let Some(until) = resolved.paused_until {
        if until > now {
            return PolicyDecision::Skip(SkipReason::Paused { until });
        }
    }
    if let Some(window) = &resolved.window {
        if !is_in_window(window, now) {
            return PolicyDecision::Deferred {
                next_window: next_window_after(window, now),
            };
        }
    }
    if resolved.gate == UpdateGate::Approval {
        if let Some(approval) = approval_ctx {
            let body = build_pending_approval_body(resolved, ctx, &approval);
            return PolicyDecision::PendingApproval(body);
        }
    }
    PolicyDecision::Proceed
}

/// Build a [`PendingApprovalBody`] from the resolver output + the caller's
/// per-candidate context. Pure; no I/O. Falls back to empty strings for
/// missing context fields so the body always has a stable shape (the
/// dashboard renders empty strings as `<unknown>` rather than panicking on
/// `Option`).
///
/// `host_id`: parsed from `ctx.host_id_hex` if present, else nil ULID.
/// `stack` / `service` / `container_name`: from ctx or empty.
/// `image` / `current_digest` / `proposed_digest`: from `approval`.
/// `diff_url`: GHCR-only via [`ghcr_compare_url`]; `None` for non-GHCR images.
/// `approver_channel`: from resolved policy.
fn build_pending_approval_body(
    resolved: &ResolvedPolicy,
    ctx: &PolicyContext<'_>,
    approval: &ApprovalContext<'_>,
) -> PendingApprovalBody {
    let host_id = ctx
        .host_id_hex
        .and_then(|h| HostId::from_string(h).ok())
        .unwrap_or_else(HostId::nil);
    let stack = ctx.stack.unwrap_or("").to_string();
    let service = ctx.service.unwrap_or("").to_string();
    let container_name = ctx.container_name.unwrap_or("").to_string();
    let diff_url = ghcr_compare_url(
        approval.image,
        approval.current_digest,
        approval.proposed_digest,
    );
    PendingApprovalBody {
        host_id,
        stack,
        service,
        container_name,
        image: approval.image.to_string(),
        current_digest: approval.current_digest.to_string(),
        proposed_digest: approval.proposed_digest.to_string(),
        diff_url,
        approver_channel: resolved.approver_channel.clone(),
    }
}

/// GHCR-only diff URL. Returns `Some("https://github.com/<owner>/<repo>/compare/<cur>...<new>")`
/// when `image` looks like a GHCR ref (`ghcr.io/<owner>/<repo>...`) and both
/// digests look like sha256 strings. Returns `None` for any other registry
/// or malformed input.
///
/// The compare URL is best-effort: GHCR images are commonly tagged from a
/// GitHub repo with the same `<owner>/<repo>` slug, so the link works for
/// the typical case. When it doesn't, the dashboard renders no diff link
/// (the field stays `None`).
fn ghcr_compare_url(image: &str, current_digest: &str, proposed_digest: &str) -> Option<String> {
    let stripped = image.strip_prefix("ghcr.io/")?;
    // Drop any tag (`:vN`) suffix on the repo segment so the slug is clean.
    let repo_slug = stripped.split(':').next()?;
    // Compare URL only makes sense for `<owner>/<repo>` slugs; deeper
    // namespaces (rare but legal on ghcr) get the first two segments.
    let mut parts = repo_slug.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    // sha256 prefix is canonical for OCI digests; bail out on anything
    // exotic so we don't render broken links.
    if !current_digest.starts_with("sha256:") || !proposed_digest.starts_with("sha256:") {
        return None;
    }
    Some(format!(
        "https://github.com/{owner}/{repo}/compare/{current_digest}...{proposed_digest}"
    ))
}

/// One-shot helper composing the resolver + the skip / approval rules. Used
/// by the integration tests so they don't have to assemble the resolver call
/// by hand.
///
/// `approval_ctx` is `None` for the early-cycle skip check and `Some` once
/// the registry probe has yielded `current` + `proposed` digests.
pub fn policy_decision(
    snapshot: &[LoadedPolicy],
    ctx: &PolicyContext<'_>,
    approval_ctx: Option<ApprovalContext<'_>>,
    now: DateTime<Utc>,
) -> (ResolvedPolicy, PolicyDecision) {
    let projected = project_for_resolver(snapshot);
    let resolved = resolve_policy(&projected, ctx);
    let decision = decision_from_resolved(&resolved, ctx, approval_ctx, now);
    (resolved, decision)
}

/// Map a [`GateDecision`] to the matching [`PolicyDecision`].
///
/// Pure function (no I/O). Caller does the side effects: emits
/// `update.gated_<x>` events, persists the gate-evaluation `webhook_deliveries`
/// audit row, upserts service-scope `paused_until` for `Defer` /
/// `Unreachable`, and persists the approval row for `Manual`.
///
/// `pending_body` is built by the caller from `ctx + approval_ctx + resolved`
/// (the same body the existing `gate=Approval` path uses). It is consumed
/// only when the gate decision is `Manual`. For `Approve` the caller falls
/// through to the existing post-policy logic (which is the original
/// `policy_decision` output before gate evaluation).
pub fn policy_decision_from_gate(
    gate: GateDecision,
    pending_body: PendingApprovalBody,
) -> PolicyDecision {
    match gate {
        GateDecision::Approve => PolicyDecision::Proceed,
        GateDecision::Reject { reason } => {
            PolicyDecision::Skip(SkipReason::GateRejected { reason })
        }
        GateDecision::Defer { until } => PolicyDecision::Deferred {
            next_window: Some(until),
        },
        GateDecision::Manual => PolicyDecision::PendingApproval(pending_body),
        GateDecision::Unreachable => {
            // 1h backoff per spec. Caller persists this as paused_until on
            // the service-scope policy row before returning Deferred.
            let until = Utc::now() + chrono::Duration::hours(1);
            PolicyDecision::Deferred {
                next_window: Some(until),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use isengard_core::policy::{MaintenanceWindow, Policy, UpdateGate, UpdateStrategy};

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
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
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
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
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
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    #[test]
    fn empty_snapshot_proceeds() {
        let snapshot: Vec<LoadedPolicy> = vec![];
        let owned = policy_context_from_container(None, None, None, "x");
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
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
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
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
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    /// Gate=Approval with `approval_ctx=Some` returns
    /// `PendingApproval` carrying a body built from ctx + approval_ctx.
    #[test]
    fn gate_approval_with_digests_returns_pending_approval() {
        let policy = Policy {
            gate: Some(UpdateGate::Approval),
            approver_channel: Some("ops-team".into()),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Service,
            scope_key: "web".into(),
            body: policy,
        }];
        let l = labels(&[
            ("com.docker.compose.project", "blog"),
            ("com.docker.compose.service", "web"),
        ]);
        // host_id_hex must be a valid ULID string for the body to populate it.
        let host_ulid = HostId::new();
        let host_str = host_ulid.to_string();
        let owned = policy_context_from_container(
            Some(&l),
            Some("prod"),
            Some(host_str.as_str()),
            "blog-web-1",
        );
        let approval = ApprovalContext {
            image: "ghcr.io/foo/bar:latest",
            current_digest: "sha256:1111",
            proposed_digest: "sha256:2222",
        };
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), Some(approval), Utc::now());
        match dec {
            PolicyDecision::PendingApproval(body) => {
                assert_eq!(body.host_id, host_ulid);
                assert_eq!(body.stack, "blog");
                assert_eq!(body.service, "web");
                assert_eq!(body.container_name, "blog-web-1");
                assert_eq!(body.image, "ghcr.io/foo/bar:latest");
                assert_eq!(body.current_digest, "sha256:1111");
                assert_eq!(body.proposed_digest, "sha256:2222");
                assert_eq!(body.approver_channel.as_deref(), Some("ops-team"));
                assert_eq!(
                    body.diff_url.as_deref(),
                    Some("https://github.com/foo/bar/compare/sha256:1111...sha256:2222")
                );
            }
            other => panic!("expected PendingApproval, got {other:?}"),
        }
    }

    /// Gate=Approval with `approval_ctx=None` (pre-digest stage)
    /// falls through to `Proceed`. The cycle's early-skip phase only sees
    /// Skip vs Proceed; gate enforcement waits for the registry probe.
    #[test]
    fn gate_approval_without_digests_proceeds() {
        let policy = Policy {
            gate: Some(UpdateGate::Approval),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Service,
            scope_key: "web".into(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x").with_service("web".into());
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, Utc::now());
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    /// GHCR helper: well-formed ghcr.io image + sha256 digests yield a
    /// compare URL.
    #[test]
    fn ghcr_compare_url_for_well_formed_inputs() {
        let url = ghcr_compare_url("ghcr.io/owner/repo:v1", "sha256:aaa", "sha256:bbb");
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/owner/repo/compare/sha256:aaa...sha256:bbb")
        );
    }

    /// Non-GHCR images get no diff URL.
    #[test]
    fn ghcr_compare_url_returns_none_for_non_ghcr() {
        assert!(ghcr_compare_url("docker.io/library/nginx", "sha256:a", "sha256:b").is_none());
        assert!(ghcr_compare_url("registry.example.com/x/y", "sha256:a", "sha256:b").is_none());
    }

    /// Non-sha256 digests bail out (we don't render broken links).
    #[test]
    fn ghcr_compare_url_returns_none_for_non_sha_digests() {
        assert!(ghcr_compare_url("ghcr.io/o/r", "md5:abc", "sha256:def").is_none());
    }

    /// A window matching `now` lets the cycle proceed.
    #[test]
    fn window_in_window_proceeds() {
        let policy = Policy {
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        // 2026-05-03 02:30 UTC is 30 min after the Sunday 02:00 firing.
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 2, 30, 0).unwrap();
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, now);
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    /// A window not matching `now` returns Deferred with the
    /// upcoming firing time as `next_window`.
    #[test]
    fn window_outside_window_returns_deferred() {
        let policy = Policy {
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        // Tuesday 12:00 UTC: next firing is Sunday 02:00 UTC.
        let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, now);
        match dec {
            PolicyDecision::Deferred { next_window } => {
                let next = next_window.expect("next_window populated");
                assert_eq!(next, Utc.with_ymd_and_hms(2026, 5, 10, 2, 0, 0).unwrap());
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
    }

    /// Edge case: Pinned wins over an outside-window check. The
    /// more specific signal (`Skip(Pinned)`) is what the cycle emits.
    #[test]
    fn window_pinned_wins_over_outside_window() {
        let policy = Policy {
            strategy: Some(UpdateStrategy::Pinned),
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        // Outside the Sunday 02:00 window.
        let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, now);
        assert_eq!(dec, PolicyDecision::Skip(SkipReason::Pinned));
    }

    /// Edge case: paused_until in the future wins over the window
    /// check. Same precedence rule as Pinned.
    #[test]
    fn window_paused_wins_over_outside_window() {
        let until = Utc.with_ymd_and_hms(2026, 5, 7, 0, 0, 0).unwrap();
        let policy = Policy {
            paused_until: Some(until),
            window: Some(MaintenanceWindow {
                cron_expr: "0 2 * * 0".to_string(),
                timezone: None,
            }),
            ..Default::default()
        };
        let snapshot = vec![LoadedPolicy {
            scope_type: PolicyScopeType::Global,
            scope_key: String::new(),
            body: policy,
        }];
        let owned = policy_context_from_container(None, None, None, "x");
        let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        let (_, dec) = policy_decision(&snapshot, &owned.as_ref(), None, now);
        assert_eq!(dec, PolicyDecision::Skip(SkipReason::Paused { until }));
    }

    impl OwnedPolicyContext {
        fn with_service(mut self, s: String) -> Self {
            self.service = Some(s);
            self
        }
    }

    /// The four gate outcomes map to the four expected
    /// `PolicyDecision` shapes.
    #[test]
    fn gate_approve_maps_to_proceed() {
        let body = PendingApprovalBody {
            host_id: HostId::nil(),
            stack: "blog".into(),
            service: "web".into(),
            container_name: "blog-web-1".into(),
            image: "i".into(),
            current_digest: "sha256:1".into(),
            proposed_digest: "sha256:2".into(),
            diff_url: None,
            approver_channel: None,
        };
        let dec = policy_decision_from_gate(GateDecision::Approve, body);
        assert_eq!(dec, PolicyDecision::Proceed);
    }

    #[test]
    fn gate_reject_maps_to_skip_gate_rejected_with_reason() {
        let body = PendingApprovalBody {
            host_id: HostId::nil(),
            stack: "blog".into(),
            service: "web".into(),
            container_name: "x".into(),
            image: "i".into(),
            current_digest: "sha256:1".into(),
            proposed_digest: "sha256:2".into(),
            diff_url: None,
            approver_channel: None,
        };
        let dec = policy_decision_from_gate(
            GateDecision::Reject {
                reason: Some("too risky".into()),
            },
            body,
        );
        assert_eq!(
            dec,
            PolicyDecision::Skip(SkipReason::GateRejected {
                reason: Some("too risky".into())
            })
        );
    }

    #[test]
    fn gate_defer_maps_to_deferred_next_window_until() {
        let body = PendingApprovalBody {
            host_id: HostId::nil(),
            stack: "blog".into(),
            service: "web".into(),
            container_name: "x".into(),
            image: "i".into(),
            current_digest: "sha256:1".into(),
            proposed_digest: "sha256:2".into(),
            diff_url: None,
            approver_channel: None,
        };
        let until = Utc::now() + Duration::hours(2);
        let dec = policy_decision_from_gate(GateDecision::Defer { until }, body);
        assert_eq!(
            dec,
            PolicyDecision::Deferred {
                next_window: Some(until)
            }
        );
    }

    #[test]
    fn gate_manual_maps_to_pending_approval_with_body() {
        let body = PendingApprovalBody {
            host_id: HostId::nil(),
            stack: "blog".into(),
            service: "web".into(),
            container_name: "x".into(),
            image: "i".into(),
            current_digest: "sha256:1".into(),
            proposed_digest: "sha256:2".into(),
            diff_url: None,
            approver_channel: Some("ops".into()),
        };
        let dec = policy_decision_from_gate(GateDecision::Manual, body.clone());
        assert_eq!(dec, PolicyDecision::PendingApproval(body));
    }

    #[test]
    fn gate_unreachable_maps_to_deferred_one_hour() {
        let body = PendingApprovalBody {
            host_id: HostId::nil(),
            stack: "blog".into(),
            service: "web".into(),
            container_name: "x".into(),
            image: "i".into(),
            current_digest: "sha256:1".into(),
            proposed_digest: "sha256:2".into(),
            diff_url: None,
            approver_channel: None,
        };
        let dec = policy_decision_from_gate(GateDecision::Unreachable, body);
        match dec {
            PolicyDecision::Deferred {
                next_window: Some(t),
            } => {
                let now = Utc::now();
                let lower = now + Duration::minutes(55);
                let upper = now + Duration::minutes(65);
                assert!(t >= lower && t <= upper, "expected ~1h ahead, got {t}");
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
    }
}
