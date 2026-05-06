# Phase 9d Plan: Maintenance Windows

Spec: `docs/superpowers/specs/2026-05-06-phase-9d-maintenance-windows-design.md`. Closes issue #46.

Five tasks, one commit per task. All work on branch `feat/phase-9d` off `next` HEAD `56f6d9a`.

## T1: core types + window evaluator

Files:
- `crates/isengard-core/src/policy/mod.rs`: add `MaintenanceWindow` struct + `window` field on `Policy`. Re-export `MaintenanceWindow`.
- `crates/isengard-core/src/policy/window.rs` (new): `is_in_window`, `next_window_after`, `WINDOW_DURATION` constant, internal cron + tz parsers.
- `crates/isengard-core/Cargo.toml`: add `croner` and `chrono-tz` deps; bump workspace deps in root `Cargo.toml`.

Tests in `window.rs`:
1. in-window 30 min after 02:00 firing
2. out-of-window 90 min after 02:00 firing
3. before first occurrence (e.g., schedule starts in future month)
4. timezone honored (Europe/Zurich Sunday 02:00)
5. malformed cron returns false (fail-closed)
6. unknown tz falls back to UTC
7. `next_window_after` returns next occurrence as UTC

`mod.rs` smoke tests:
- `Policy { window: Some(...) }` round-trips through JSON
- `Policy::default().window.is_none()`
- old JSON without `window` key deserializes (backwards-compat)

Commit: `feat(core): MaintenanceWindow type + cron evaluator (#46)`

## T2: resolver + updater decision

Files:
- `crates/isengard-core/src/policy/resolve.rs`: extend `ResolvedPolicy` with `window: Option<MaintenanceWindow>`, extend `ResolvedProvenance` with `window: PolicyOrigin`, walk it in the merge loop.
- `crates/isengard-plugins/updater/src/policy.rs`: extend `PolicyDecision` with `Deferred { next_window }`. Update `decision_from_resolved` to check the window after Pinned + paused but before the approval branch.

Tests:
- resolver: per-scope window override (global -> stack)
- updater unit tests: `decision_from_resolved` returns `Deferred` outside-window
- updater unit tests: `decision_from_resolved` returns `Proceed` in-window
- updater unit tests: Pinned wins over window

Commit: `feat(updater): policy decision returns Deferred outside maintenance window (#46)`

## T3: do_cycle integration + event

Files:
- `crates/isengard-core/src/event.rs`: add `kinds::UPDATE_DEFERRED`.
- `crates/isengard-plugins/updater/src/lib.rs`: handle the `Deferred` variant in `do_cycle`, increment counter, emit `update.deferred`. Add `deferred` to the `update.checked` cycle summary.

Integration tests added to existing `crates/isengard-plugins/updater/tests/policy_integration.rs` (or analogous file already in place):
- in-window cycle proceeds
- outside-window cycle emits `update.deferred`
- Pinned + window: Pinned wins

Commit: `feat(updater): emit update.deferred on outside-window candidates (#46)`

## T4: REST validation

Files:
- `crates/isengard-plugins/dashboard/src/policies.rs`: extend `validate_policy` to parse `body.window.cron_expr` when present. Return 400 on parse error.
- `crates/isengard-plugins/dashboard/Cargo.toml`: add `croner` (no need for tz parsing here; tz validation is lenient).

Tests in `crates/isengard-plugins/dashboard/tests/policies_endpoints.rs`:
- POST with malformed window returns 400
- POST with valid window round-trips
- PUT with malformed window returns 400

Commit: `feat(dashboard): validate maintenance window cron on policy write (#46)`

## T5: UI + design + release notes

Files:
- `crates/isengard-plugins/dashboard/web/composables/usePolicies.ts`: extend `PolicyBody` with `window?: { cron_expr: string; timezone?: string }`.
- `crates/isengard-plugins/dashboard/web/composables/useEffectivePolicy.ts`: extend `ResolvedPolicy` and `ResolvedProvenance` with `window`.
- `crates/isengard-plugins/dashboard/web/components/policies/PolicyEditor.vue`: window section (override checkbox + cron input + tz dropdown + custom tz input + live preview).
- `crates/isengard-plugins/dashboard/web/components/policies/PolicyRow.vue`: summary line for window.
- `crates/isengard-plugins/dashboard/web/components/policies/EffectivePolicyPreview.vue`: render window row.
- `crates/isengard-plugins/dashboard/web/lib/cron-preview.ts` (new): tiny client-side cron walker for 3-firings preview. Bounded to 7-day lookahead, falls back to "(invalid expression)" on parse error.
- `design/pages/settings-policies.md`: mark window shipped, remove from deferred list.
- `docs/RELEASE_NOTES_PHASE_9D.md`: operator-facing notes including "set update window to Sunday 02:00 only" example.

Verify:
- `bun run build` clean (no console errors, no unused-import warnings).

Commit: `feat(dashboard): policy editor window picker + release notes (#46)`

## Final gate sweep

Run before opening the PR:
```sh
cargo fmt --check
cargo build --workspace
cargo clippy --workspace --all-targets --all-features -D warnings
cargo test --workspace
cargo deny check
cd crates/isengard-plugins/dashboard/web && bun run build
```

## PR

- Branch: `feat/phase-9d`
- Base: `next`
- Title: `feat: phase 9d (maintenance windows)`
- Body: `Closes #46.` plus the bullet list of what shipped.
