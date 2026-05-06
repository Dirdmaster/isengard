//! Integration tests for the pure policy resolver.
//!
//! See spec §"Resolver" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.

use chrono::{TimeZone, Utc};
use isengard_core::policy::{
    FailureHandling, Policy, PolicyContext, PolicyOrigin, PolicyScopeType, UpdateGate,
    UpdateStrategy,
    defaults::{DEFAULT_GATE, DEFAULT_ON_FAILURE, DEFAULT_STRATEGY},
    resolve_policy,
};

fn ctx_full<'a>() -> PolicyContext<'a> {
    PolicyContext {
        fleet: Some("prod"),
        stack: Some("prod/blog"),
        service: Some("prod/blog/web"),
        host_id_hex: Some("0123456789abcdef"),
        container_name: Some("prod-blog-web-1"),
    }
}

#[test]
fn empty_rows_returns_defaults_with_default_origin() {
    let rows: Vec<(PolicyScopeType, &str, &Policy)> = vec![];
    let resolved = resolve_policy(&rows, &ctx_full());

    assert_eq!(resolved.strategy, DEFAULT_STRATEGY);
    assert_eq!(resolved.gate, DEFAULT_GATE);
    assert_eq!(resolved.paused_until, None);
    assert_eq!(resolved.on_failure, DEFAULT_ON_FAILURE);
    assert_eq!(resolved.approver_channel, None);

    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Default);
    assert_eq!(resolved.provenance.gate, PolicyOrigin::Default);
    assert_eq!(resolved.provenance.paused_until, PolicyOrigin::Default);
    assert_eq!(resolved.provenance.on_failure, PolicyOrigin::Default);
    assert_eq!(resolved.provenance.approver_channel, PolicyOrigin::Default);
}

#[test]
fn global_only_overrides_some_fields_rest_fall_back_to_default() {
    let global = Policy {
        strategy: Some(UpdateStrategy::Pinned),
        gate: None,
        paused_until: None,
        on_failure: None,
        approver_channel: Some("ops".to_string()),
    };
    let rows = vec![(PolicyScopeType::Global, "", &global)];

    let resolved = resolve_policy(&rows, &ctx_full());

    // Fields the row set:
    assert_eq!(resolved.strategy, UpdateStrategy::Pinned);
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Global);
    assert_eq!(resolved.approver_channel.as_deref(), Some("ops"));
    assert_eq!(resolved.provenance.approver_channel, PolicyOrigin::Global);

    // Fields the row left None: default + Default origin.
    assert_eq!(resolved.gate, DEFAULT_GATE);
    assert_eq!(resolved.provenance.gate, PolicyOrigin::Default);
    assert_eq!(resolved.on_failure, DEFAULT_ON_FAILURE);
    assert_eq!(resolved.provenance.on_failure, PolicyOrigin::Default);
    assert_eq!(resolved.paused_until, None);
    assert_eq!(resolved.provenance.paused_until, PolicyOrigin::Default);
}

#[test]
fn fleet_overrides_global_global_fills_remaining_fields() {
    // Global sets strategy + on_failure.
    // Fleet overrides strategy, sets gate.
    // Expected: strategy from Fleet, gate from Fleet, on_failure from
    // Global, everything else default.
    let global = Policy {
        strategy: Some(UpdateStrategy::TagOnly),
        gate: None,
        paused_until: None,
        on_failure: Some(FailureHandling::Notify),
        approver_channel: None,
    };
    let fleet = Policy {
        strategy: Some(UpdateStrategy::Pinned),
        gate: Some(UpdateGate::Never),
        paused_until: None,
        on_failure: None,
        approver_channel: None,
    };
    let rows = vec![
        (PolicyScopeType::Global, "", &global),
        (PolicyScopeType::Fleet, "prod", &fleet),
    ];

    let resolved = resolve_policy(&rows, &ctx_full());

    assert_eq!(resolved.strategy, UpdateStrategy::Pinned);
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Fleet);

    assert_eq!(resolved.gate, UpdateGate::Never);
    assert_eq!(resolved.provenance.gate, PolicyOrigin::Fleet);

    assert_eq!(resolved.on_failure, FailureHandling::Notify);
    assert_eq!(resolved.provenance.on_failure, PolicyOrigin::Global);

    assert_eq!(resolved.paused_until, None);
    assert_eq!(resolved.provenance.paused_until, PolicyOrigin::Default);

    assert_eq!(resolved.approver_channel, None);
    assert_eq!(resolved.provenance.approver_channel, PolicyOrigin::Default);
}

#[test]
fn service_wins_when_all_four_scopes_set_strategy() {
    let strategy_only = |s: UpdateStrategy| Policy {
        strategy: Some(s),
        gate: None,
        paused_until: None,
        on_failure: None,
        approver_channel: None,
    };
    let global = strategy_only(UpdateStrategy::Pinned);
    let fleet = strategy_only(UpdateStrategy::TagOnly);
    let stack = strategy_only(UpdateStrategy::Minor);
    let service = strategy_only(UpdateStrategy::Any);

    // Intentionally feed rows in non-rank order to confirm sorting.
    let rows = vec![
        (PolicyScopeType::Service, "prod/blog/web", &service),
        (PolicyScopeType::Global, "", &global),
        (PolicyScopeType::Stack, "prod/blog", &stack),
        (PolicyScopeType::Fleet, "prod", &fleet),
    ];

    let resolved = resolve_policy(&rows, &ctx_full());

    assert_eq!(resolved.strategy, UpdateStrategy::Any);
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Service);
    // Other fields untouched: defaults all the way down.
    assert_eq!(resolved.gate, DEFAULT_GATE);
    assert_eq!(resolved.provenance.gate, PolicyOrigin::Default);
}

#[test]
fn container_override_beats_everything() {
    let global = Policy {
        strategy: Some(UpdateStrategy::TagOnly),
        gate: Some(UpdateGate::Auto),
        paused_until: None,
        on_failure: Some(FailureHandling::Notify),
        approver_channel: None,
    };
    let fleet = Policy {
        strategy: Some(UpdateStrategy::Any),
        gate: Some(UpdateGate::Never),
        paused_until: None,
        on_failure: None,
        approver_channel: None,
    };
    let service = Policy {
        strategy: Some(UpdateStrategy::Minor),
        gate: None,
        paused_until: None,
        on_failure: Some(FailureHandling::Keep),
        approver_channel: Some("svc-channel".to_string()),
    };
    let container = Policy {
        strategy: Some(UpdateStrategy::Pinned),
        gate: Some(UpdateGate::Approval),
        paused_until: None,
        on_failure: Some(FailureHandling::Rollback),
        approver_channel: Some("container-channel".to_string()),
    };

    let container_key = "0123456789abcdef/prod-blog-web-1";
    let rows = vec![
        (PolicyScopeType::Global, "", &global),
        (PolicyScopeType::Fleet, "prod", &fleet),
        (PolicyScopeType::Service, "prod/blog/web", &service),
        (PolicyScopeType::Container, container_key, &container),
    ];

    let resolved = resolve_policy(&rows, &ctx_full());

    assert_eq!(resolved.strategy, UpdateStrategy::Pinned);
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Container);

    assert_eq!(resolved.gate, UpdateGate::Approval);
    assert_eq!(resolved.provenance.gate, PolicyOrigin::Container);

    assert_eq!(resolved.on_failure, FailureHandling::Rollback);
    assert_eq!(resolved.provenance.on_failure, PolicyOrigin::Container);

    assert_eq!(
        resolved.approver_channel.as_deref(),
        Some("container-channel")
    );
    assert_eq!(
        resolved.provenance.approver_channel,
        PolicyOrigin::Container
    );
}

#[test]
fn provenance_tracks_per_field_origin_not_per_row() {
    // Mixed-origin scenario: gate from Fleet, paused_until from Service,
    // on_failure from Stack. Strategy and approver_channel never set.
    let pause = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();

    let fleet = Policy {
        strategy: None,
        gate: Some(UpdateGate::Never),
        paused_until: None,
        on_failure: None,
        approver_channel: None,
    };
    let stack = Policy {
        strategy: None,
        gate: None,
        paused_until: None,
        on_failure: Some(FailureHandling::Keep),
        approver_channel: None,
    };
    let service = Policy {
        strategy: None,
        gate: None,
        paused_until: Some(pause),
        on_failure: None,
        approver_channel: None,
    };
    let rows = vec![
        (PolicyScopeType::Fleet, "prod", &fleet),
        (PolicyScopeType::Stack, "prod/blog", &stack),
        (PolicyScopeType::Service, "prod/blog/web", &service),
    ];

    let resolved = resolve_policy(&rows, &ctx_full());

    // Per-field origins, independently:
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Default);
    assert_eq!(resolved.provenance.gate, PolicyOrigin::Fleet);
    assert_eq!(resolved.provenance.paused_until, PolicyOrigin::Service);
    assert_eq!(resolved.provenance.on_failure, PolicyOrigin::Stack);
    assert_eq!(resolved.provenance.approver_channel, PolicyOrigin::Default);

    // And the values themselves are correct.
    assert_eq!(resolved.strategy, DEFAULT_STRATEGY);
    assert_eq!(resolved.gate, UpdateGate::Never);
    assert_eq!(resolved.paused_until, Some(pause));
    assert_eq!(resolved.on_failure, FailureHandling::Keep);
    assert_eq!(resolved.approver_channel, None);
}

#[test]
fn rows_for_other_scopes_are_filtered_out() {
    // A fleet=staging row should not affect a context with fleet=prod.
    let other_fleet = Policy {
        strategy: Some(UpdateStrategy::Pinned),
        gate: None,
        paused_until: None,
        on_failure: None,
        approver_channel: None,
    };
    let rows = vec![(PolicyScopeType::Fleet, "staging", &other_fleet)];

    let resolved = resolve_policy(&rows, &ctx_full());

    // Strategy stays at default: the staging row was ignored.
    assert_eq!(resolved.strategy, DEFAULT_STRATEGY);
    assert_eq!(resolved.provenance.strategy, PolicyOrigin::Default);
}
