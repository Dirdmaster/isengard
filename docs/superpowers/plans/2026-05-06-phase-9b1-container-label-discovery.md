# Phase 9b.1 plan: Container-label policy discovery

Spec: `docs/superpowers/specs/2026-05-06-phase-9b1-container-label-discovery-design.md`.

Small phase, single PR. Five tasks, gated by `just ci-local` at each step.

## T1. Label parser in `isengard-core`

File: `crates/isengard-core/src/policy/labels.rs` (new). Wire it into
`isengard-core/src/policy/mod.rs` with `pub mod labels;`.

- Constants for the five label keys.
- `parse_policy_labels(&HashMap<String, String>) -> Result<Policy, ParseLabelError>`.
- `ParseLabelError { label: String, value: String, reason: String }`.
- Accept kebab-case + snake_case for enum values; canonicalize internally.
- 8+ unit tests per spec §"Tests > Unit".

Exit: `cargo test -p isengard-core` green.

## T2. Agent watcher: include `isengard.policy.*`

File: `crates/isengard-agent/src/labels.rs`. Widen
`inspect_to_report` so a container with any `isengard.policy.*` key is
reported even if it has no `isengard.expose*`. Keep the unchanged path for
expose-only containers.

Exit: `cargo test -p isengard-agent` green; existing
`proxy_label_discovery_e2e.rs` still passes.

## T3. Controller-side ingest

File: `crates/isengard-controller/src/policy_ingest.rs` (new). Wire into
`crates/isengard-controller/src/lib.rs` as `pub mod policy_ingest;`.

- `pub struct PolicyLabelIngest { inv: Arc<Inventory>, by_container: Mutex<HashMap<(HostId, String), String>> }`
  where the value is the scope_key.
- `ingest_policy_labels(host_id, report)`: parse labels; if all-unset, delete
  any existing row for `<host_id>/<container_name>`. Else upsert. Records
  the `(host_id, container_id) -> scope_key` mapping for cleanup.
- `ingest_policy_labels_removed(host_id, removed_event)`: look up the
  scope_key by `(host_id, container_id)`; if found, delete the row.
- Hook into `service.rs` so `ContainerLabelsReport` and
  `ContainerLabelsRemoved` route to the new module in parallel with the
  existing routing-rule ingest.

Exit: `cargo test -p isengard-controller` green.

## T4. Periodic reaper

File: `crates/isengard-controller/src/policy_ingest.rs` (extend).

- `pub async fn reap_orphaned_container_policies(inv: &Inventory, now: DateTime<Utc>) -> Result<usize>`:
  scans all `Container`-scope rows, deletes those whose
  `updated_at < now - 24h`. The agent re-emits a fresh report at every
  Docker `start` / `update` and on the initial scan at sync-stream open, so
  any genuinely live container's row will have been touched in the last
  hour. 24h is conservative.
- Spawn the task in `controller::service::run` (or wherever the controller
  task pool is set up). Run every hour.

Exit: unit test on `reap_orphaned_container_policies` covering "live row
not reaped, stale row reaped".

## T5. Resolver integration test

File: `crates/isengard-controller/tests/policy_label_ingest_e2e.rs` (new).

- `ingest_policy_labels` with `strategy=pinned`, then load all rows and
  call `resolve_policy` against the matching container context. Assert
  resolved `strategy = Pinned` and `provenance.strategy = Container`.
- `ingest_policy_labels_removed` deletes the row.
- Malformed enum value (`isengard.policy.strategy = pinneded`) does not
  crash; no row is upserted; pre-existing row is preserved.
- `parse_policy_labels` of `Policy::default()` -> ingest deletes any
  pre-existing row (label removed but key still present).

Exit: `cargo test -p isengard-controller` green.

## T6. UI tweaks

Files:
- `crates/isengard-plugins/dashboard/web/components/policies/PolicyEditor.vue`
- `crates/isengard-plugins/dashboard/web/components/policies/PolicyRow.vue`

Edits:
- Editor: replace the `(Phase 9b.1)` chip with
  `(read-only)` and the helper text with
  "Discovered automatically from compose labels."
- Row: when `policy.scopeType === 'container'`, render a small label icon
  pill (mono `[label]`) next to the scope label with
  `title="Discovered from compose labels"`. Hide Edit, replace Remove with
  a disabled chip whose tooltip points to the compose file.

Exit: visual smoke (no automated UI test for this slice; existing storybook
snapshot tests, if any, still pass).

## T7. Wrap-up

- `design/pages/settings-policies.md`: bump the Phase 9b.1 deferred line to
  shipped.
- `docs/RELEASE_NOTES_PHASE_9B1.md`: operator-facing release notes with
  compose YAML example.
- `just ci-local` final pass.
- Commit + push branch + open PR vs `next`.

## Hard rules

- No em dashes (U+2014) or en dashes (U+2013) anywhere.
- Plan A's `resolve_policy` is unchanged; do not touch the resolver.
- Reference issue #49 in commits.
