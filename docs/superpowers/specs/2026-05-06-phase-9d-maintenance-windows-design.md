# Phase 9d: Maintenance Windows

Honors the `policy.window` field that was deferred from Plan A (9a-9d Policy Foundation). When a resolved policy carries a window, the updater skips updates outside the window and emits `update.deferred(next_window=...)`. UI gains a window picker in `<PolicyEditor />` and a window line in `<PolicyRow />`.

Vault reference: [[Update Policies & Approval Flow]] §"Maintenance window".
Predecessor spec: `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md` (the field was intentionally absent there).
Closes: GitHub issue #46.

## Scope at the bit-level

1. New core type: `MaintenanceWindow { cron_expr: String, timezone: Option<String> }` with serde defaults.
2. Extend `Policy` with `pub window: Option<MaintenanceWindow>` (backwards-compatible: existing rows without `window` deserialize cleanly via `#[serde(default)]`).
3. Extend `ResolvedPolicy` and `ResolvedProvenance` with the same field; resolver merges per existing layered precedence (`Option::or` semantics, container wins).
4. New module `isengard-core::policy::window` exposing `is_in_window(window, now)` and `next_window_after(window, now)`.
5. Updater integration: in `policy_decision`, after Pinned + paused checks and before the existing approval branch, if the resolved policy has a window AND `is_in_window(now) == false`, return `PolicyDecision::Deferred { next_window: DateTime<Utc> }`. The cycle emits `update.deferred` with the `next_window` payload field, increments a `deferred` counter, and continues without recreating.
6. REST: no new endpoints. The existing `POST /api/v1/policies` and `PUT /api/v1/policies/{scope_type}/{*scope_key}` already accept arbitrary `body`. Add validation: malformed `cron_expr` returns 400 with a parser-level message.
7. UI: extend `<PolicyEditor />` with a Window picker (override checkbox + cron text input + timezone select with common values + custom text fallback) and live "Next 3 firings" preview computed client-side. Extend `<PolicyRow />` with a window summary line.

## Out of scope (deferred)

- Configurable window duration. v1 hard-codes 1h; the field can be added to `MaintenanceWindow` later without a migration since the type is JSON-encoded.
- Cron syntax beyond standard 5-field minute/hour/day-of-month/month/day-of-week. Quartz seconds field, `@reboot`, and `@yearly` macros are out of v1.
- Day-of-year and step expressions on weekday (`*/3`) are accepted by `croner` but not officially documented in the helper text.
- Window enforcement on the approval gate path. If a user manually approves outside-window via the dashboard, the apply happens immediately. Documented; not blocking.

## Storage

No migration required. The `policies.body_json` column is opaque. Existing rows deserialize because the new field defaults to `None`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    // ... existing fields ...
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window: Option<MaintenanceWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceWindow {
    pub cron_expr: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timezone: Option<String>,
}
```

`timezone == None` resolves as UTC. Unparseable IANA names (logged warn; fall back to UTC) so a stale row never blocks the cycle.

## Window evaluator

Module: `isengard-core::policy::window`.

```rust
pub fn is_in_window(window: &MaintenanceWindow, now: DateTime<Utc>) -> bool;
pub fn next_window_after(window: &MaintenanceWindow, now: DateTime<Utc>) -> Option<DateTime<Utc>>;
```

Algorithm:

1. Parse `cron_expr` with `croner::Cron::new(...).with_dom_and_dow().parse()`. On error, return `false` for `is_in_window` (fail-closed: a broken window blocks updates so the operator notices the validation error in the UI). REST already rejects malformed exprs at write time.
2. Resolve the timezone: try `chrono_tz::Tz::from_str(tz)`; default to `Utc` on `None` or parse-error.
3. `is_in_window(now)`:
   - Convert `now` to the timezone.
   - Compute the previous firing time via `cron.find_previous_occurrence(now, true)`.
   - If it exists AND `now - prev < 1h`, return `true`.
4. `next_window_after(now)`:
   - Compute the next firing time via `cron.find_next_occurrence(now, true)`.
   - Return as UTC `DateTime`.

Default window duration: **1 hour**. Hard-coded in the const `WINDOW_DURATION = Duration::hours(1)`. Configurable in a follow-up.

Tests (>= 6):
1. `is_in_window` returns `true` 30 min after the 02:00 firing.
2. `is_in_window` returns `false` 90 min after the 02:00 firing (past the 1h envelope).
3. `is_in_window` returns `false` before the very first occurrence.
4. Timezone honored: `"0 2 * * 0"` in `Europe/Zurich` while now is 02:30 Zurich (00:30 UTC in winter) returns `true`.
5. Malformed cron returns `false`.
6. Unknown tz falls back to UTC.
7. `next_window_after` returns the upcoming occurrence as UTC.

## Resolver

`ResolvedPolicy` and `ResolvedProvenance` gain `window: Option<MaintenanceWindow>` / `window: PolicyOrigin`. The resolver loop walks rows in rank order: `Some(window)` overrides; `None` inherits. No change to the implicit defaults block (window has no default, stays `None`).

## Updater integration

`PolicyDecision` gains a new variant:

```rust
pub enum PolicyDecision {
    Skip(SkipReason),
    Deferred { next_window: DateTime<Utc> },
    Proceed,
    PendingApproval(PendingApprovalBody),
}
```

`decision_from_resolved` order:
1. `Pinned` → `Skip(Pinned)`
2. `paused_until > now` → `Skip(Paused { until })`
3. `window.is_some() && !is_in_window(now)` → `Deferred { next_window }`
4. `gate=Approval` (unchanged)
5. else → `Proceed`

In `do_cycle`:
- New counter `deferred: usize`. The `update.checked` summary mentions it.
- On `Deferred`: emit `update.deferred` event with `next_window` (RFC3339). No recreate, no approval row. `continue` to next candidate.
- New event kind `update.deferred` in `event.rs::kinds`.

Tests (>= 3 integration):
1. Window matches now → cycle proceeds (no recreate skipped).
2. Window misses now → cycle emits `update.deferred` with non-empty `next_window`.
3. Window + Pinned → Pinned wins (emitted as `update.policy_skipped`, NOT `update.deferred`).

## REST validation

In `dashboard::policies::validate_policy`: when `body.window.is_some()`, parse the cron expression. On error return 400 `{ "error": "invalid window cron: <msg>" }`. Timezone parsing is lenient (warn-only) so users can paste a custom tz without the API rejecting it.

## UI

### `<PolicyEditor />`

Add a Window field section between `paused_until` and `on_failure`:

- Override checkbox.
- Cron expression `<input>` with helper line "Use cron syntax: minute hour day-of-month month day-of-week".
- Timezone `<select>` with common entries (`UTC`, `Europe/Zurich`, `America/New_York`, `Asia/Tokyo`, `custom`). When `custom` is picked, render a free-form text input next to it.
- Live "Next 3 firings" preview computed client-side via the npm `cronstrue`-free helper using a tiny inline parser (see implementation note below) — if the parser fails, show "(invalid expression)".
- Provenance label when not overridden: "Currently: <expr> (<tz>) (inherited from STACK)".

### `<PolicyRow />`

Add a summary line: "Window: `0 2 * * 0` (Europe/Zurich)" when `body.window` is set.

### Implementation note (UI parser)

`cronstrue` is heavy; instead we parse next-firings client-side with a small JS helper using `Date.toLocaleString` and a manual cron-tick walker bounded to 7 days. If that turns out to be heavier than expected, we ship just the raw cron string with no preview (acceptable per spec). The preview is best-effort.

## Acceptance criteria

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (new tests >= 6 unit + 3 integration)
- [ ] `cargo clippy --workspace --all-targets --all-features -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo deny check` clean (croner + chrono-tz licensed permissive)
- [ ] `bun run build` clean
- [ ] Existing policies without `window` deserialize unchanged
- [ ] No em dashes (U+2014) or en dashes (U+2013) in any new file

## Risks + open questions

- **Cron crate fit**: `croner` (v3) accepts standard 5-field syntax + has `find_previous_occurrence` / `find_next_occurrence`. `cron` (zslayton) requires 7-field. Picking `croner` matches the 5-field UI helper.
- **TZ binary size**: `chrono-tz` ships the full IANA database (~1MB). Acceptable for the controller binary; we already ship a non-trivial Nuxt SPA. If ever a problem, we can `chrono-tz` feature-filter to common zones.
- **Window enforcement vs approval**: an operator-approved update applies immediately, even outside the window. Acceptable: the operator made the call. Documented in release notes.
- **DST edges**: `croner` + `chrono-tz` jointly handle ambiguous local times. We trust the libraries; no special-case logic.

## References

- Vault: [[Update Policies & Approval Flow]] (§"Maintenance window", §"Edge cases")
- Plan A: `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`
- Existing: `crates/isengard-core/src/policy/{mod,resolve}.rs`
- Existing: `crates/isengard-plugins/updater/src/policy.rs`
- Issue: `Dirdmaster/isengard#46`
