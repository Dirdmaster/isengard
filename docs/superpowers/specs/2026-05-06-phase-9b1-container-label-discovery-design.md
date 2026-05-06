# Phase 9b.1: Container-label policy discovery

Closes the container-scope hole left by Plan A (9a-9d). The resolver already
honors `PolicyScopeType::Container` rows (with `scope_key` of
`<host_id>/<container_name>`) but no producer ever writes one. This phase wires
producers: a Docker-label parser that extracts `isengard.policy.*` labels and a
controller-side ingest path that upserts a container-scope policy row whenever
a labeled container appears.

The resolver is unchanged: container rows already win against fleet/stack/
service rows by virtue of `PolicyScopeType::rank()`.

## Scope

In:

1. New label parser: `isengard.policy.{strategy,gate,paused_until,on_failure,approver_channel}`.
2. Reuse the existing agent watcher (`isengard-agent::labels::watch`) so a
   container with `isengard.policy.*` (even without `isengard.expose*`) is
   reported up the sync stream.
3. Controller-side `ingest_policy_labels` that upserts a container-scope row,
   and `ingest_policy_labels_removed` that deletes the row on stop / die /
   destroy.
4. Periodic reaper: a 24h-interval task that deletes orphaned container-scope
   rows (no agent ever reported them again). Belt-and-braces for events lost
   during agent downtime.
5. UI: enable the Container scope option in `PolicyEditor` as read-only
   ("discovered automatically from compose labels"). Mark container rows in
   the list view with a label-source pill.

Out of scope (deferred):

- Maintenance windows in labels (`window` field is itself deferred to 9h).
- Validation against agent-reported live state (no hard guarantee the
  container actually exists when the row is upserted).
- Conflict resolution between hand-authored container rows and label-discovered
  ones (we never let users author container rows; first writer is always the
  agent).

## Label parser (`isengard-core::policy::labels`)

Pure module, no I/O. Mirrors `isengard-core::labels::parse_labels` shape so
tests can drop in `HashMap<String, String>`.

```rust
pub const LABEL_PREFIX: &str = "isengard.policy.";
pub const LABEL_STRATEGY: &str = "isengard.policy.strategy";
pub const LABEL_GATE: &str = "isengard.policy.gate";
pub const LABEL_PAUSED_UNTIL: &str = "isengard.policy.paused_until";
pub const LABEL_ON_FAILURE: &str = "isengard.policy.on_failure";
pub const LABEL_APPROVER_CHANNEL: &str = "isengard.policy.approver_channel";

pub fn parse_policy_labels(
    labels: &HashMap<String, String>,
) -> Result<Policy, ParseLabelError>;
```

Parsing rules:

- Each label is independently optional. Unset labels stay `None`.
- Enum values accept both kebab-case (`tag-only`) and snake_case (`tag_only`)
  for ergonomic tolerance; canonical form is kebab-case.
- `paused_until` is RFC 3339 (e.g. `2026-06-15T00:00:00Z`).
- Unknown / malformed values return `Err(ParseLabelError { label, value })`
  rather than silently dropping. The ingest caller logs at warn and skips
  upsert; it does not crash.
- All-unset (no `isengard.policy.*` keys present) returns `Ok(Policy::default())`,
  which the caller treats as "nothing to do, no row to write".

## Discovery path

1. Agent's `isengard-agent::labels::watch` already listens for `start` /
   `update` / `stop` / `die` / `destroy`. Today its filter at the
   `inspect_to_report` site only emits when a label key starts with
   `isengard.expose`. Widen the filter to also include keys starting with
   `isengard.policy.`.
2. The wire payload (`ContainerLabelsReport`) already carries the full label
   map, so no proto change is needed.
3. Controller's `service.rs` already routes `ContainerLabelsReport` to
   `routing.ingest_labels`. Add a parallel call to
   `policy::ingest_policy_labels` (new module) for the same payload.
4. `ingest_policy_labels` builds the container scope_key
   `<host_id>/<container_name>` (matching the resolver's expectation), parses
   the label set, and `upsert_policy(Container, key, body)` if any policy
   field is set. If the parsed body is `Policy::default()` (no policy labels),
   the existing row (if any) is deleted so a removed label cleanly drops the
   row.

## Cleanup

Two layers:

1. **Event-driven**: `ContainerLabelsRemoved` on stop / die / destroy already
   reaches `service.rs`. Add a parallel `ingest_policy_labels_removed`
   that deletes the container-scope policy row by `scope_key` derived from
   the same host_id and a name lookup. Since `ContainerLabelsRemoved` only
   carries `container_id`, we record the (host_id, container_id) -> scope_key
   association at upsert time in a small in-memory map on the
   `PolicyLabelIngest` struct so the remove path can resolve a scope_key
   without re-querying Docker.
2. **Periodic reaper**: a controller-side task running every hour iterates
   all `Container`-scope policy rows older than 24h and deletes any whose
   host_id no longer appears in the connected senders OR whose
   `<container_name>` is not present in the agent's last reported live label
   set. Belt-and-braces against missed `destroy` events.

## Resolver

No change. Plan A's `resolve_policy` already handles
`PolicyScopeType::Container` (highest rank, wins over service / stack /
fleet / global). We add an integration test that exercises the full path:
discover label -> upsert row -> resolve_policy returns container-origin
strategy.

## UI

Two surgical changes to `crates/isengard-plugins/dashboard/web/components/policies/`:

- `PolicyEditor.vue`: keep the Container radio disabled in edit/create, but
  swap the tooltip text from `"Phase 9b.1"` to
  `"Discovered automatically from compose labels: read-only here."` and
  the `scopeKeyHelper` text from `"Discovered from compose labels in Phase
  9b.1."` to a sentence describing the label naming.
- `PolicyRow.vue`: when `policy.scopeType === 'container'`, render a small
  label-icon pill next to the scope label with title
  `"Discovered from compose labels"`. Hide the Edit button (the row is
  read-only) and keep Remove disabled with a tooltip pointing the user to
  the compose file.

## Tests

Unit (label parser, in `isengard-core::policy::labels::tests`):

1. All-unset returns `Policy::default()`.
2. Each field set independently round-trips (`strategy`, `gate`,
   `paused_until`, `on_failure`, `approver_channel`).
3. snake_case accepted for enum values.
4. kebab-case accepted for enum values.
5. Malformed enum value returns `Err(ParseLabelError)` carrying the label
   name.
6. Malformed `paused_until` (non-RFC3339) returns Err.
7. Full set (every field) round-trips.
8. Unknown `isengard.policy.unknown` keys are ignored (forward-compat).

Integration (controller-side, in
`crates/isengard-controller/tests/policy_label_ingest_e2e.rs`):

1. `ingest_policy_labels` with `strategy=pinned` upserts a container-scope
   row with `body.strategy = Some(Pinned)`.
2. After ingest, `resolve_policy` for that container returns
   `provenance.strategy = Container`.
3. `ingest_policy_labels_removed` deletes the container-scope row.
4. Malformed enum value logs warn but does NOT crash; no row is upserted
   and any pre-existing row remains intact.

## Operator-facing example

```yaml
services:
  cache:
    image: redis:7
    labels:
      isengard.enable: "true"
      isengard.policy.strategy: "any"
      isengard.policy.gate: "auto"
  api:
    image: ghcr.io/acme/api:1.4.2
    labels:
      isengard.enable: "true"
      isengard.policy.strategy: "pinned"
      isengard.policy.paused_until: "2026-06-15T00:00:00Z"
      isengard.policy.approver_channel: "ops"
```

## Migration

None. Reuses migration 0016 (`policies`).

## Naming convention rationale

`isengard.policy.<field>` matches the existing `isengard.<feature>.<prop>`
pattern (`isengard.enable`, `isengard.expose`, `isengard.expose.port`). Field
names match the `Policy` struct exactly so vault docs round-trip cleanly.

## Cross-references

- Source design: `1 Projects/Isengard/Update Policies & Approval Flow.md`
  (vault), §"Per-container policy via compose label override".
- Existing producer scaffolding: `crates/isengard-agent/src/labels.rs`
  (`isengard.expose*` watcher we extend).
- Existing consumer scaffolding: `crates/isengard-controller/src/routing.rs`
  (`ingest_labels` parallel).
- Resolver: `crates/isengard-core/src/policy/resolve.rs` (unchanged).
