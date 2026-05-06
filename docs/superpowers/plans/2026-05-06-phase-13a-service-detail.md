# Phase 13 Plan A: Service detail page

Implementation plan for [[2026-05-06-phase-13a-service-detail-design]]. Closes issue #56.

Branch: `feat/phase-13a`
Worktree: `~/Projects/isengard/.worktrees/phase-13a`
Base: `next` at `56f6d9a`
Migration slot: not needed (read-only API, reuses existing tables)

## Standing self-review (every task)

Before declaring done:

1. `cargo build --workspace`
2. `cargo test --workspace --exclude isengard-plugins` for non-plugin tasks; full workspace tests for plugin tasks
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Grep changed files for em dash (U+2014) and en dash (U+2013): zero tolerance
6. `bun run build` in `crates/isengard-plugins/dashboard/web` for UI tasks
7. Cite exact files added/modified

## Tasks

### T1: REST endpoint `GET /api/v1/services/:stack_id/:service_name`

**Goal**: ship the read-only service detail handler returning a single envelope.

**Files to add/modify**:
- `crates/isengard-plugins/dashboard/src/dto.rs`: add `ServiceDetailDto`, refresh `ServiceDto` to carry `hostname`, `last_seen_at`, `deploy_strategy_override`. Add a `From<Service>` impl that takes a hostname argument or accepts `None` for batch translation.
- `crates/isengard-plugins/dashboard/src/api.rs`: add a `get_service_detail` handler and route. Reuse the existing `policies::resolve_policy` for the effective policy.
- `crates/isengard-plugins/dashboard/tests/services_endpoints.rs` (new): integration tests.
- `crates/isengard-plugins/dashboard/Cargo.toml`: add a `[[test]]` entry if needed (defaults usually pick it up automatically).

**Acceptance**:
- Endpoint returns `ServiceDetailDto` JSON on success.
- 404 on missing stack id, missing service.
- 6 tests pass: missing stack, missing service, single-host happy path, embedded effective policy, multi-host other_instances populated, attached routing rules surfaced.

**Cite the design**: spec §"REST endpoint".

### T2: Page route `pages/stacks/[id]/services/[name].vue`

**Goal**: render the two-column page with metadata + logs placeholder + events + routing.

**Files to add/modify**:
- `crates/isengard-plugins/dashboard/web/composables/useServiceDetail.ts` (new): lazy fetcher around the new endpoint, returns `{ data, loading, error, reload }`.
- `crates/isengard-plugins/dashboard/web/pages/stacks/[id]/services/[name].vue` (new).

**Components reused**:
- `PageHeader`, `EmptyState`, `EffectivePolicyPreview`, `EventRow`, `KvRow`, `StatusPill`, `TopBar`.

**Layout**:
- Outer `flex-1 flex flex-col min-h-0` wrapping `<TopBar>` + body.
- Body uses `grid grid-cols-[1fr_2fr] gap-4 p-6`.
- Left column: metadata, effective policy (lazy collapsible), last deployment.
- Right column: logs placeholder, routing rules list, recent events, other instances.

**Acceptance**:
- Route renders without console errors.
- 404 path renders `<EmptyState icon="alert-circle" title="Service not found">`.
- Logs panel placeholder reads "Logs streaming arrives in Phase 13B" with a `<a>` to https://github.com/Dirdmaster/isengard/issues/57.

### T3: ServiceChip drilldown wiring

**Goal**: clicking a service row on stack overview routes to the new detail page.

**Files to modify**:
- `crates/isengard-plugins/dashboard/web/components/stacks/StackOverviewTab.vue`: wrap the per-service row in `<NuxtLink :to="...">`. Inline Expose button stays clickable; add `@click.stop` to keep it from triggering navigation.

**Acceptance**: bun build green, manual click navigates correctly. No new tests because the existing build is the gate.

### T4: Update design tracker + release notes

**Goal**: document what shipped in this slice and what is still pending.

**Files to modify**:
- `design/pages/service-detail.md`: flip `status` from `phase-13-pending` to `partial`, add a status_note describing the 13A delivery and what 13B+ still owes.
- `docs/RELEASE_NOTES_PHASE_13A.md` (new): operator-facing summary.

**Acceptance**:
- Both files committed.
- Release notes call out what works (metadata, events, policy preview, routing) and what is deferred (logs streaming, restart, exec shell).

### T5: Final gate sweep + PR

**Goal**: workspace gates green, branch pushed, PR opened against `next`.

Steps:
1. `cargo build --workspace`
2. `cargo test -p isengard-plugin-dashboard`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. `cd crates/isengard-plugins/dashboard/web && bun install && bun run build`
6. `git push -u origin feat/phase-13a`
7. `gh pr create --base next --title "feat: phase 13a (service detail page)" --body "Closes #56 ..."`

**Acceptance**: PR URL captured. CI green or expected to be green.

## File map

```
crates/isengard-plugins/dashboard/src/api.rs           (modify: add get_service_detail handler + route)
crates/isengard-plugins/dashboard/src/dto.rs           (modify: ServiceDetailDto + ServiceDto fields)
crates/isengard-plugins/dashboard/tests/services_endpoints.rs   (new)
crates/isengard-plugins/dashboard/web/composables/useServiceDetail.ts  (new)
crates/isengard-plugins/dashboard/web/pages/stacks/[id]/services/[name].vue  (new)
crates/isengard-plugins/dashboard/web/components/stacks/StackOverviewTab.vue (modify: wrap row in NuxtLink)
design/pages/service-detail.md                                              (modify: status + note)
docs/superpowers/specs/2026-05-06-phase-13a-service-detail-design.md        (new)
docs/superpowers/plans/2026-05-06-phase-13a-service-detail.md               (new)
docs/RELEASE_NOTES_PHASE_13A.md                                             (new)
```

## Risks

- The dashboard REST already exposes `/services` and `/services/:id` as stubs returning empty lists / 404. The new path uses `:stack_id/:service_name` so it does not collide. Tests guard the routing.
- Multi-host detection in `other_instances` is an N+1 risk if a fleet has hundreds of hosts: we list services once filtered by stack id, plus a single inventory.list_hosts to map host_id to hostname. Two queries total, fine for v1.
- Routing rules listing per host is one query per visit; cheap.
