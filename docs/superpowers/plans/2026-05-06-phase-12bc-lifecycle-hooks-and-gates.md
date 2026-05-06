# Plan: Phase 12b + 12c (Lifecycle Hooks + External-Action Gates)

Spec: `docs/superpowers/specs/2026-05-06-phase-12bc-lifecycle-hooks-and-gates-design.md`. Issues: #54 + #55.

## Sequencing rule

This slice rides on Phase 12a (#53). The 12a worker stays unchanged; we extend the row schema beneath it. Lifecycle hooks land first (label parser -> ingest -> subscriber); gates land second (types -> evaluator -> updater wiring -> UI). Each task ends with a green workspace.

## Tasks

### T1 - storage migration + delivery DAO extension
- [ ] Write `crates/isengard-storage/migrations/0021_lifecycle_hooks_and_gates.sql` (schema in spec)
- [ ] Add `DeliverySource` enum to `crates/isengard-storage/src/webhook.rs` with `as_str()` + `FromStr` (`webhook | lifecycle | gate`)
- [ ] Make `WebhookDelivery.webhook_id: Option<i64>`, add `source`, `url: Option<String>`, `secret: Option<String>` fields
- [ ] Update `row_to_delivery` decoder; update `InsertDelivery` to keep working as the legacy 12a path
- [ ] Add `insert_lifecycle_delivery(InsertLifecycleDelivery)` and `insert_gate_delivery(InsertGateDelivery)` methods on Inventory
- [ ] Add `list_deliveries_by_source(source, limit)` returning chronological-DESC slice
- [ ] Run storage tests; existing 12a tests must still pass

### T2 - container hooks DAO
- [ ] Create `crates/isengard-storage/src/container_hooks.rs` with `ContainerHooks` row + `UpsertContainerHooks` + `Inventory` impls (`upsert`, `delete_by_name`, `delete_by_container_id`, `get`, `list_by_host`)
- [ ] Wire from `lib.rs`
- [ ] Tests in `tests/container_hooks_dao.rs`: insert/get/upsert overwrites/delete returns bool/list_by_host filters

### T3 - hook label parser
- [ ] Add `crates/isengard-core/src/hooks.rs` with `parse_hook_labels(map) -> ParsedHooks` and `has_any_hook_label`
- [ ] Wire from `crates/isengard-core/src/lib.rs`
- [ ] Tests (label_parser): all-three-URLs, just-pre, missing all returns empty, optional secret, invalid URL passes through (worker surfaces it later)

### T4 - controller-side hook label ingest
- [ ] Create `crates/isengard-controller/src/hook_ingest.rs` mirroring `policy_ingest.rs` patterns
- [ ] Hook into the same agent-message dispatch in `service.rs` so `ContainerLabelsReport` and `ContainerLabelsRemoved` flow into both ingests
- [ ] Tests: insert path, no-hooks-removes-row path, container-removed path, malformed URL warns + still upserts

### T5 - lifecycle subscriber on webhooks plugin
- [ ] Add `crates/isengard-plugins/webhooks/src/lifecycle.rs`: deployment.* event tap + container_hooks lookup + insert_lifecycle_delivery
- [ ] Spawn it from `lib.rs::start` alongside the existing subscriber
- [ ] Tests: event maps to correct hook kind, missing hook row is silent, all four states -> correct kinds, blue + green both consulted

### T6 - delivery worker source-aware dispatch
- [ ] Refactor `worker::dispatch_one` so URL+secret resolution is a small function `resolve_endpoint(&inventory, &delivery)` returning `(url, secret)`
- [ ] For source='webhook' the resolver loads the webhook row (existing); for source='lifecycle' / 'gate' it returns the row's own `url`/`secret`
- [ ] Tests: lifecycle delivery dispatches with own URL+secret; webhook delivery dispatches via webhooks row (regression for 12a)

### T7 - external gate types in core
- [ ] Add `crates/isengard-core/src/policy/gate.rs`: `ExternalGate`, `GateDecision`, `GatePayload`
- [ ] Re-export from `policy/mod.rs`
- [ ] Add `Policy.external_gate: Option<ExternalGate>` with serde default; resolver merges per scope precedence with `ResolvedPolicy.external_gate`
- [ ] Update `ResolvedProvenance` with `external_gate: PolicyOrigin`
- [ ] Tests: round-trip serde, resolver merges, provenance tracking, label parser ignores (no env-var label support for gates yet)

### T8 - gate evaluator in updater plugin
- [ ] Create `crates/isengard-plugins/updater/src/gate.rs` with `evaluate_gate(client, gate, payload) -> GateDecision`
- [ ] Add hmac/sha2/hex/wiremock to updater Cargo.toml
- [ ] Tests with wiremock: 200 approve / 200 reject / 200 defer / 200 manual / 5xx -> Manual / timeout -> Manual / malformed -> Manual / connection refused -> Unreachable / signature header verification

### T9 - updater integration
- [ ] Extend `policy.rs::policy_decision` with a gate hook called by the cycle: returns either a fully-decided `PolicyDecision` or a sentinel that means "fall through to the existing post-gate logic"
- [ ] In the cycle (lib.rs / wherever the candidate loop lives), when `resolved.external_gate.is_some()` and the existing decision is `Proceed` or `PendingApproval(..)`, call `evaluate_gate`
- [ ] Map decisions per spec; emit `update.gated_*` events; persist gate delivery rows
- [ ] Tests: 4 integration scenarios in `tests/policy_resolve.rs` or a new file

### T10 - REST + UI for external_gate
- [ ] Extend `PolicyDto` in dashboard with `externalGate?: { url; secret?; timeoutSecs }`
- [ ] PolicyEditor.vue: new "External gate" section
- [ ] Tests: PUT round-trip, GET masks secret

### T11 - REST endpoint for deliveries by source
- [ ] Add `GET /api/v1/webhooks/deliveries?source={lifecycle|gate|webhook}&limit=N` to dashboard webhooks router
- [ ] Tests: source filter returns only matching rows; bad source returns 400

### T12 - WebhooksSettings.vue lifecycle subtab
- [ ] Tab strip ("Webhooks" | "Lifecycle hooks" | "Gates") with the All-source variants
- [ ] Reuses WebhookDeliveriesPanel pattern but consumes the by-source endpoint
- [ ] Quick smoke test (component-level)

### T13 - design + release docs
- [ ] Update `design/pages/settings-webhooks.md` (lifecycle subtab section)
- [ ] Update `design/pages/settings-policies.md` (external_gate field section)
- [ ] Write `docs/RELEASE_NOTES_PHASE_12BC.md`: Python receiver examples for both lifecycle hooks and external gates, signing instructions, decision JSON shapes

### T14 - final gate sweep + PR
- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] No em dashes (U+2014) or en dashes (U+2013) anywhere new
- [ ] Push branch + open PR vs `next` titled `feat: phase 12b+c (lifecycle hooks + external-action gates)`, body "Closes #54 #55"

## Risk register

- **SQLite ALTER limitations.** Cannot make `webhook_id` nullable in place; we recreate the table via shadow + INSERT-SELECT. Migration is offline-safe (controller process restart), and 12a deliveries pre-migration are tiny (typically < 1k rows).
- **Lifecycle subscriber double-firing.** `deployment.aborted` and `deployment.failed` both map to `on_failure`. Spec deduplicates per-deployment-id+kind at insertion time? Not for v1: the spec says fire once per event; if both fire we accept two on_failure deliveries. Document in release notes.
- **Gate evaluator blocking the cycle.** The cycle loop runs through candidates one at a time. A 10s gate evaluation per candidate is fine for fleets up to ~50 services. For larger fleets we'll parallelise in a later phase.
- **Per-row secret leak surface.** Lifecycle hook rows store the secret plaintext in `webhook_deliveries.secret`. Same threat model as the existing `webhooks.secret` column; mention in release notes.
