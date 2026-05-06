# Phase 9 Plan A (9a-9d): Update Policies Foundation

Translates the design (vault: [[Update Policies & Approval Flow]]) into a build-ready slice covering 9a through 9d:

- **9a**: Policy struct + storage table + lookup function (no UI, no enforcement)
- **9b**: Updater consults policy, respects `Pinned` + `paused_until`
- **9c**: Settings to Update policies UI: list view + add/edit/remove rows
- **9d**: Effective policy preview on Stack detail (per-service)

Phases 9e-9j (approval flow, notifier callbacks, maintenance windows, Minor strategy, Rollback) are out of scope here. They build on this foundation.

## Scope at the bit-level

What this slice ships, end to end:

1. A `policies` table with polymorphic scope.
2. A `Policy` struct in `isengard-core` with optional fields (None means inherit).
3. A `PolicyResolver` that walks the layered scopes (global, fleet, stack, service, container-label) and produces a resolved Policy with field-level provenance.
4. An updater that consults the resolved policy and skips `Pinned` services + services with active `paused_until`.
5. A `policy_skipped` event emitted when the updater skips a candidate.
6. REST endpoints under `/api/v1/policies` for list, create, update, delete.
7. A Settings to Policies page (`/settings/policies`) with list view, add modal, edit modal, remove confirm.
8. An `<EffectivePolicyPreview />` component slotted onto Stack detail per service, showing resolved policy + provenance.

## Out of scope (deferred)

- `gate=Approval` enforcement (Phase 9e)
- Notifier interactive messages (Phase 9f-9g)
- Maintenance windows (Phase 9h)
- `strategy=Minor` semver-aware tag bumping (Phase 9i)
- `on_failure=Rollback` integration (Phase 9j; couples with Phase 10)
- Container-label policy override discovery from Docker labels (defer to 9b.1: detect at compose-ingest time)
- Policy versioning / audit trail (Phase 9.x)
- Bulk operations on the queue (Phase 9.x)

## Storage

### Migration 0016

```sql
CREATE TABLE policies (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_type  TEXT NOT NULL CHECK (scope_type IN
                  ('global', 'fleet', 'stack', 'service', 'container')),
    scope_key   TEXT NOT NULL,
    body_json   TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(scope_type, scope_key)
);

CREATE INDEX idx_policies_scope_type ON policies(scope_type);
```

`scope_key` shape:
- `global`: empty string
- `fleet`: fleet name (matches `fleets.name`)
- `stack`: `<fleet>/<stack>` (matches stacks rendered by inventory)
- `service`: `<fleet>/<stack>/<service>`
- `container`: `<host_id_hex>/<container_name>` (only set when explicit container-label override is discovered; not authored via UI in this slice)

### Policy struct (in `isengard-core`)

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub strategy: Option<UpdateStrategy>,
    pub gate: Option<UpdateGate>,
    pub paused_until: Option<DateTime<Utc>>,
    pub on_failure: Option<FailureHandling>,
    pub approver_channel: Option<String>,
    // Note: `window` (maintenance window) deferred to 9h; field absent here.
}

pub enum UpdateStrategy { Pinned, TagOnly, Minor, Any }
pub enum UpdateGate { Auto, Approval, Never }
pub enum FailureHandling { Rollback, Keep, Notify }
```

Default Policy is "all None": inherit everything. The implicit root resolved value when no rows exist is `strategy=TagOnly, gate=Auto, on_failure=Notify, paused_until=None, approver_channel=None`. Documented as constants.

### DAO (in `isengard-storage::policy`)

```rust
pub struct InsertPolicy { pub scope_type: PolicyScopeType, pub scope_key: String, pub body: Policy }

impl Inventory {
    pub async fn insert_policy(&self, ins: InsertPolicy) -> Result<PolicyRow>;
    pub async fn get_policy(&self, scope_type: PolicyScopeType, scope_key: &str) -> Result<Option<PolicyRow>>;
    pub async fn list_policies(&self) -> Result<Vec<PolicyRow>>;
    pub async fn upsert_policy(&self, scope_type: PolicyScopeType, scope_key: &str, body: &Policy) -> Result<PolicyRow>;
    pub async fn delete_policy(&self, scope_type: PolicyScopeType, scope_key: &str) -> Result<bool>;
}
```

`PolicyRow` includes id, scope_type, scope_key, body (deserialized Policy), created_at, updated_at.

## Resolver

In `isengard-core::policy::resolve`. Pure function over loaded rows; storage is the caller's job.

```rust
pub struct PolicyContext<'a> {
    pub fleet: Option<&'a str>,
    pub stack: Option<&'a str>,
    pub service: Option<&'a str>,
    pub host_id_hex: Option<&'a str>,
    pub container_name: Option<&'a str>,
}

pub struct ResolvedPolicy {
    pub strategy: UpdateStrategy,
    pub gate: UpdateGate,
    pub paused_until: Option<DateTime<Utc>>,
    pub on_failure: FailureHandling,
    pub approver_channel: Option<String>,
    pub provenance: ResolvedProvenance,
}

pub struct ResolvedProvenance {
    pub strategy: PolicyOrigin,
    pub gate: PolicyOrigin,
    pub paused_until: PolicyOrigin,
    pub on_failure: PolicyOrigin,
    pub approver_channel: PolicyOrigin,
}

pub enum PolicyOrigin { Default, Global, Fleet, Stack, Service, Container }

pub fn resolve_policy(rows: &[PolicyRow], ctx: &PolicyContext) -> ResolvedPolicy;
```

Walks the candidate rows in increasing-specificity order. Each `Some(value)` overwrites the resolved field and records the origin. Container-level overrides have final word.

## Updater integration (9b)

- `do_cycle` looks up the policies table once per cycle (cheap; SQLite, small table).
- For each candidate container, derives `PolicyContext` and calls `resolve_policy`.
- Skip rules:
  - `strategy == Pinned`: skip; emit `update.policy_skipped(reason="pinned")`.
  - `paused_until.is_some_and(|t| t > now())`: skip; emit `update.policy_skipped(reason="paused", until=t)`.
  - All others: proceed with the existing flow (no behavioral change yet for `gate=Approval` etc; that's 9e).
- Counters added: `pinned`, `paused` (both incremented on skip).

`update.policy_skipped` event payload:
```json
{ "service": "...", "container": "...", "reason": "pinned|paused", "until": "..." }
```

## REST API (9c)

Mounted under existing dashboard plugin at `/api/v1/policies`:

| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/policies` | List all policies, ordered by scope_type rank (global, fleet, stack, service, container). |
| POST | `/api/v1/policies` | Create. Body: `{ scope_type, scope_key, body }`. 409 on duplicate. |
| PUT | `/api/v1/policies/:scope_type/:scope_key` | Upsert body. |
| DELETE | `/api/v1/policies/:scope_type/:scope_key` | Delete. 404 if absent. |

Validation:
- `scope_type` must be one of the five enums.
- `scope_key` non-empty for non-global; empty for global.
- `body.paused_until`, when set, must be RFC3339.
- Reject `body.gate=Approval` until 9e (return 422 with explanatory message). This keeps the data model honest while UI lets users see the value as inactive.

Effective preview also reachable as a derived endpoint:

| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/policies/effective?fleet=&stack=&service=` | Returns ResolvedPolicy with provenance for the given context. |

## Dashboard UI (9c + 9d)

### Page: `/settings/policies`

- New file: `web/pages/settings/policies.vue`
- Uses existing `SettingsTabs` already wired in `web/pages/settings/index.vue`. Add a `policies` tab entry between `enrollment` and `deployments`.
- Top-level layout: PageHeader (title "Update policies", CTA "+ Add policy"), then a list of `<PolicyRow />` cards in scope-rank order.

### `<PolicyRow />`

Bordered card with:
- Header line: scope label (e.g., "FLEET . prod") + gate badge (Auto / Approval / Never) + strategy chip
- Body line: human-readable summary of the fields this row sets, e.g., "Override gate: ask before applying.   Approver: Telegram to ops-team-chat"
- Action buttons: Edit, Remove (Resume button visible only when paused_until is set)

### `<PolicyEditor />`

Modal driven from the row's Edit button or the page-level "+ Add policy". Fields-with-inheritance form: each field shows the inherited value as a placeholder, with a checkbox "Override at this level" that activates the input. On Save, PUTs the upsert.

For 9a-9d we ship: scope picker, strategy radio, gate radio (Approval option visually disabled with "Phase 9e" tooltip), paused_until date input, on_failure radio, approver_channel text. Window field omitted (Phase 9h).

### `<EffectivePolicyPreview />` (9d)

A collapsible card mounted on Stack detail per service row (in the existing services chip area). Shows resolved policy with provenance:

```
Effective policy
strategy:        tag-only       (from GLOBAL DEFAULT)
gate:            auto           (from GLOBAL DEFAULT)
paused_until:    -
on_failure:      notify         (from GLOBAL DEFAULT)
approver:        -
```

Implementation: query `/api/v1/policies/effective?fleet=&stack=&service=` and render as a 2-column table.

## Acceptance criteria

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (new tests included)
- [ ] `cargo clippy --workspace --all-targets -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo deny check` clean
- [ ] `bun run build` (dashboard) clean
- [ ] Migration 0016 applies and rolls forward; sample data inserted in tests
- [ ] Updater integration test: pinned service is skipped, normal service updates
- [ ] REST round-trip test for each verb
- [ ] Dashboard renders Policies page and Effective preview without console errors
- [ ] No em dashes in any new file (vault hard rule, applied to repo via `.git/hooks` already in place)

## Risks + open questions

- **Migration ordering**: 0016 is the next slot. If another phase ships in parallel, rebase carefully.
- **Container-label discovery**: 9b respects container-scope rows but does not yet *populate* them from Docker labels. We need a follow-up (9b.1) to read `isengard.policy.*` labels at compose-ingest time. Documented; not blocking.
- **Effective preview cost**: query runs once per service row on Stack detail. Small N, but if a stack has 50 services we'd hammer the API. Acceptable for v1; consider batch endpoint if profiling shows hot.
- **Approver channel validation**: we store as free-form string; future 9f will add a notifier-channel registry. For now, document that the value is informational.

## References

- Vault: [[Update Policies & Approval Flow]] (full design)
- Vault: [[Blue-Green Deployment]] (Rollback handler couples in 9j)
- Existing: `crates/isengard-storage/src/fleet.rs`, `crates/isengard-storage/src/service.rs` (scope-key construction patterns)
- Existing: `crates/isengard-plugins/updater/src/lib.rs::do_cycle` (where the policy check slots in)
- Existing: `crates/isengard-plugins/dashboard/web/pages/settings/index.vue` (settings tabs host)
