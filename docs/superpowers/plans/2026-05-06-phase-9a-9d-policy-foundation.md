# Phase 9 Plan A (9a-9d): Update Policies Foundation

Implementation plan for [[2026-05-06-phase-9a-9d-policy-foundation-design]]. Subagent-driven workflow: each task is a self-contained dispatch with explicit context, expected output, and gates.

Branch: `feat/phase-9a-9d`
Worktree: `~/Projects/isengard/.worktrees/phase-9a-9d`
Base: `next` at `27ef61d` (Home rebuild)
Migration slot: `0016`

Implementer model: **Opus** for every task (per session preference).

## Standing self-review (every task)

Before declaring done, the implementer must:

1. Run `cargo build --workspace`
2. Run `cargo test --workspace --exclude isengard-plugins` for non-plugin tasks; full workspace test for plugin tasks
3. Run `cargo clippy --workspace --all-targets -- -D warnings`
4. Run `cargo fmt --check`
5. Grep changed files for em dash (U+2014) and en dash (U+2013); zero tolerance
6. Confirm migration up-down applies cleanly (for storage tasks)
7. Confirm `bun run build` in `crates/isengard-plugins/dashboard/web` (for UI tasks)
8. Cite exact files added/modified in the report; cite line numbers for non-obvious chunks

## Task list

### T1: Storage migration 0016 + Policy DAO

**Goal**: ship the `policies` table, the `Policy` struct in `isengard-core`, and the DAO functions on `Inventory`.

**Files to add/modify**:
- `crates/isengard-storage/migrations/0016_policies.sql` (new)
- `crates/isengard-core/src/policy.rs` (new): Policy struct, enums, defaults; serde derives
- `crates/isengard-core/src/lib.rs`: re-export `policy::*`
- `crates/isengard-storage/src/policy.rs` (new): PolicyRow, PolicyScopeType, DAO methods on Inventory
- `crates/isengard-storage/src/lib.rs`: register the new module
- `crates/isengard-storage/tests/policy_dao.rs` (new)

**Acceptance**:
- 8 unit tests covering: insert + get, list ordering by scope rank, upsert (insert new, update existing), delete (existing + missing), unique constraint on (scope_type, scope_key), invalid scope_type rejected, JSON roundtrip preserves all fields, paused_until RFC3339 round-trip.
- `Policy::default()` returns all-None.
- `PolicyScopeType` implements `as_str` + `from_str` symmetrically.
- Documented constants for resolver defaults: `DEFAULT_STRATEGY = TagOnly`, `DEFAULT_GATE = Auto`, `DEFAULT_ON_FAILURE = Notify`.

**Cite the design**: spec section "Storage" + "Policy struct".

### T2: Core PolicyResolver

**Goal**: pure resolver from `&[PolicyRow]` + `PolicyContext` to `ResolvedPolicy` with provenance.

**Files to add/modify**:
- `crates/isengard-core/src/policy/resolve.rs` (new) or extend `policy.rs`
- `crates/isengard-core/src/lib.rs`: re-export
- `crates/isengard-core/tests/policy_resolve.rs` (new)

**Algorithm**:
```rust
pub fn resolve_policy(rows: &[PolicyRow], ctx: &PolicyContext) -> ResolvedPolicy {
    // 1. Filter rows that apply to ctx (global always, fleet matches, etc.)
    // 2. Sort applicable rows by scope rank: Global < Fleet < Stack < Service < Container
    // 3. For each field, walk rows in rank order; first non-None wins (later overrides earlier).
    //    Track origin per field.
    // 4. Fall back to DEFAULT_* constants for any field still None.
}
```

**Acceptance**:
- 6 unit tests:
  - Empty rows: returns DEFAULTS with origin Default for every field.
  - Global only: respects global field overrides; missing fields fall back to Default.
  - Fleet+Global: fleet wins on overlapping fields.
  - Service+Stack+Fleet+Global: service wins; provenance reflects origin.
  - Container override: container wins over everything (final say).
  - Mixed origins: provenance tracks per-field origin, not per-row.

**Cite the design**: spec section "Resolver".

### T3: Updater integration

**Goal**: updater respects `Pinned` and active `paused_until`. Emits `update.policy_skipped`.

**Files to modify**:
- `crates/isengard-plugins/updater/src/lib.rs::do_cycle`: load policies, resolve per candidate, branch on skip.
- `crates/isengard-plugins/updater/src/labels.rs` (or new helper): build `PolicyContext` from container metadata.
- `crates/isengard-core/src/event.rs` (or wherever event kinds live): add `update.policy_skipped` constant.
- `crates/isengard-plugins/updater/tests/policy_skip.rs` (new): integration tests.

**Behavior**:
- One policies-load per cycle (`Inventory::list_policies()`).
- For each candidate container:
  - Build `PolicyContext` (fleet from host, stack from compose label, service from compose label, host_id_hex, container_name).
  - Call `resolve_policy`.
  - If `strategy == Pinned`: increment `pinned` counter, emit event, continue.
  - If `paused_until` is in future: increment `paused` counter, emit event with `until`, continue.
  - Otherwise: existing flow.
- Cycle log includes `pinned=N paused=M`.

**Acceptance**:
- 4 integration tests using sqlx test pool + in-memory bollard mocks (or the existing test harness):
  - Pinned service is skipped (no docker pull called).
  - Paused service is skipped while paused_until > now; would update once paused_until passes (simulate by direct DB write).
  - Global policy with strategy=Any allows update of every candidate (regression: ensures resolver default doesn't accidentally skip).
  - update.policy_skipped event reaches the journal with correct payload.

**Cite the design**: spec section "Updater integration (9b)".

### T4: REST endpoints

**Goal**: ship `/api/v1/policies` CRUD + `/api/v1/policies/effective`.

**Files to add/modify**:
- `crates/isengard-plugins/dashboard/src/policies.rs` (new): handlers + router function.
- `crates/isengard-plugins/dashboard/src/lib.rs`: mount router.
- `crates/isengard-plugins/dashboard/tests/policies_endpoints.rs` (new).

**Endpoints**:
- `GET /api/v1/policies` returns 200 with `[ { id, scope_type, scope_key, body, created_at, updated_at } ]`, ordered by scope rank.
- `POST /api/v1/policies` body `{ scope_type, scope_key, body }`. 201 on success, 409 on UNIQUE conflict, 400 on invalid scope_type or empty scope_key for non-global.
- `PUT /api/v1/policies/:scope_type/:scope_key` upserts body. 200 on success.
- `DELETE /api/v1/policies/:scope_type/:scope_key` 204 on success, 404 if absent.
- `GET /api/v1/policies/effective?fleet=&stack=&service=&host_id=&container=` returns 200 with `ResolvedPolicy`.
- 422 with message when body.gate == Approval (Phase 9e gate, not yet enforced).

**Acceptance**:
- 8 endpoint tests covering each verb + the effective query + the 422 + 409 + 404 paths.
- Hand-tested via `curl` against `cargo run --bin isengard controller`; smoke included in test report.

**Cite the design**: spec section "REST API (9c)".

### T5: Settings to Policies page

**Goal**: ship the list view at `/settings/policies` with PolicyRow cards.

**Files to add/modify**:
- `crates/isengard-plugins/dashboard/web/pages/settings/policies.vue` (new)
- `crates/isengard-plugins/dashboard/web/components/policies/PolicyRow.vue` (new)
- `crates/isengard-plugins/dashboard/web/composables/usePolicies.ts` (new)
- `crates/isengard-plugins/dashboard/web/pages/settings/index.vue`: add "Policies" tab between Enrollment and Deployments.

**Behavior**:
- usePolicies fetches GET /api/v1/policies; SWR-style cache.
- Empty state: GLOBAL DEFAULT row only, "+ Add policy" CTA in container per [feedback_empty_states](feedback) rule.
- Each PolicyRow renders scope label, gate badge, strategy chip, body summary, Edit + Remove buttons. Resume button visible only when paused_until is set.

**Acceptance**:
- Page renders without console errors at `bun run dev`.
- Empty state visible when only global default exists.
- "+ Add policy" button opens PolicyEditor (T6).
- Remove button shows confirm modal, calls DELETE on confirm, refreshes list.
- Resume button shows on paused rows, calls PUT clearing paused_until.

**Cite the design**: spec section "Dashboard UI (9c + 9d)" and `design/pages/settings-policies.md`.

### T6: PolicyEditor modal

**Goal**: fields-with-inheritance form for create/edit.

**Files to add/modify**:
- `crates/isengard-plugins/dashboard/web/components/policies/PolicyEditor.vue` (new)

**Behavior**:
- Props: `mode: 'create' | 'edit'`, `existing?: PolicyRow`, `effective?: ResolvedPolicy` (so placeholders show inherited values).
- Scope picker (radio + scope_key field) only visible in create mode; in edit mode, scope is fixed.
- Each field has an "Override at this level" checkbox. When unchecked, the field's input is disabled and shows the inherited value as placeholder.
- Save: PUT `/api/v1/policies/:scope_type/:scope_key` with body containing only overridden fields (others remain None).
- Approval gate option is rendered but visually disabled with tooltip "Phase 9e".

**Acceptance**:
- Create round-trip: open modal, set fleet=prod, override gate=Auto, save, list updates.
- Edit round-trip: change paused_until, save, row reflects new value.
- Approval gate option is non-selectable.

### T7: EffectivePolicyPreview component

**Goal**: per-service collapsible preview on Stack detail.

**Files to add/modify**:
- `crates/isengard-plugins/dashboard/web/components/policies/EffectivePolicyPreview.vue` (new)
- `crates/isengard-plugins/dashboard/web/components/stacks/StackOverviewTab.vue` (existing): mount preview per service in the services area.
- `crates/isengard-plugins/dashboard/web/composables/useEffectivePolicy.ts` (new): tiny wrapper over GET /api/v1/policies/effective.

**Behavior**:
- Collapsible (closed by default to avoid cost).
- On expand, fetch `/api/v1/policies/effective?fleet=&stack=&service=` and render 5-row table: strategy, gate, paused_until, on_failure, approver. Each row: value + provenance label.

**Acceptance**:
- Expand on a service with no policy rows: shows DEFAULT for every field with origin "Default".
- Expand on a service with a stack-level override: row shows STACK as origin for the overridden fields.
- No layout regression on Stack detail (visual check).

### T8: Wiring + design status update + spec/plan tracking

**Goal**: surface the new page in nav, mark related design docs as shipped, update phase status.

**Files to modify**:
- `crates/isengard-plugins/dashboard/web/components/TopBar.vue` or wherever main nav lives: confirm Settings entry already covers; no change needed if Policies is just a tab.
- `design/pages/settings-policies.md`: change `status: phase-9-pending` to `status: shipped` and add Implementation status block matching the actual scope shipped (note that approval gate is visually disabled, etc.).
- `design/pages/stack-detail.md`: add EffectivePolicyPreview to the Implementation status shipped list.
- `docs/RELEASE_NOTES_PHASE_9A.md` (new): brief operator-facing release note describing the slice.

**Acceptance**:
- `bun run build` still green.
- Design docs reflect shipped reality.
- Release note covers: what's new, breaking changes (none), follow-ups (9e-9j).

## Execution order

T1 -> T2 -> T3 -> T4 in series (each depends on the previous).
T5 + T6 can run after T4. T6 depends on T5's PolicyEditor mount point but the editor is mostly self-contained; T5 stub the modal as a placeholder, T6 fills it in.
T7 can run after T4.
T8 last.

## Final gates (after T8)

Run from worktree root:

```sh
just check        # build + test + clippy + fmt + deny
just smoke        # if it exists; otherwise skip
cd crates/isengard-plugins/dashboard/web && bun run build
git diff --stat   # confirm scope matches plan
```

Open PR `feat/phase-9a-9d` against `next`. PR body lists shipped items + remaining 9e-9j follow-ups. Tag `v0.1.0-alpha.phase9a-complete` once merged.
