# Phase 12 Plan A: Outbound Webhooks

Implementation plan for [[2026-05-06-phase-12a-outbound-webhooks-design]]. Closes #53.

Branch: `feat/phase-12a`
Worktree: `~/Projects/isengard/.worktrees/phase-12a`
Base: `next` at `56f6d9a` (phase 9e+9f merged)
Migration slot: `0020`

Implementer model: Opus for every task (per session preference).

## Standing self-review (every task)

Before declaring done, the implementer must:

1. `cargo build --workspace`
2. `cargo test --workspace` (full workspace; new plugin has unit + integration tests)
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Grep changed files for em dash (U+2014) + en dash (U+2013); zero tolerance
6. Confirm migration applies cleanly against an in-memory DB
7. `bun run build` in `crates/isengard-plugins/dashboard/web` (UI tasks)
8. Cite exact files added/modified in the report

## Task list

### T1: Storage migration 0020 + DAO

**Goal**: ship the `webhooks` + `webhook_deliveries` tables and the DAO methods on `Inventory`.

**Files**:
- `crates/isengard-storage/migrations/0020_webhooks.sql`
- `crates/isengard-storage/src/webhook.rs`
- `crates/isengard-storage/src/lib.rs` (register module + re-exports)
- `crates/isengard-storage/tests/webhook_dao.rs`

**Acceptance**: 6 unit tests covering: insert + get, list, update, delete cascades deliveries, insert delivery, claim_pending_deliveries respects next_retry_at, mark_delivery_success / pending / failed / exhausted state machine.

**Cite the design**: spec §"Storage" + §"DAO".

### T2: New crate `isengard-plugin-webhooks`

**Goal**: scaffold the crate, wire `Plugin::init` + `Plugin::start` + `Plugin::stop`. Spawn the subscriber + worker tasks with shared `Arc<Inventory>`.

**Files**:
- `crates/isengard-plugins/webhooks/Cargo.toml`
- `crates/isengard-plugins/webhooks/src/lib.rs`
- `crates/isengard-plugins/webhooks/src/subscriber.rs`
- `crates/isengard-plugins/webhooks/src/worker.rs`
- `crates/isengard-plugins/webhooks/src/sign.rs`
- `crates/isengard-plugins/webhooks/src/backoff.rs`
- root `Cargo.toml`: workspace member + dep entry
- `crates/isengard/Cargo.toml`: depend on it
- `crates/isengard/src/main.rs`: `use isengard_plugin_webhooks as _;`

**Acceptance**:
- Plugin registers via `inventory::submit!`.
- `sign::compute_signature(secret, body)` returns the `sha256=<hex>` header value, hex lowercase.
- `backoff::next_delay(attempts)` returns `Some(Duration)` for attempts 1..=4 mapped to 30s, 1m, 5m, 30m, 2h, and `None` for attempts >= 5.
- 4 unit tests on signing + backoff.

### T3: Subscriber wiring

**Goal**: subscribe to the `EventBus`; on each event, find matching enabled webhooks, insert a `webhook_deliveries` row.

**Files**: `crates/isengard-plugins/webhooks/src/subscriber.rs`

**Acceptance**:
- Match logic: `*` token matches every kind; otherwise comma-split tokens are exact-match. Helper `kind_matches(filter: &str, kind: &str) -> bool` is unit-testable.
- 4 unit tests: `*`, single kind match, multi kind match, no match, disabled webhook is skipped.
- Lag handling: on `RecvError::Lagged(n)` log a warn and continue.

### T4: Delivery worker

**Goal**: tick every 5s, claim pending due deliveries, POST with HMAC, update row state.

**Files**: `crates/isengard-plugins/webhooks/src/worker.rs`, plus a `reqwest::Client` in lib.

**Acceptance**:
- `wiremock` integration tests covering: 200 success, 500 retry then exhausts, 4xx no-retry, signature header present and correct.
- `reqwest::Client` has 10s timeout.
- Worker tick claims up to 100 deliveries per pass to bound work.
- 4 integration tests in `crates/isengard-plugins/webhooks/tests/delivery.rs`.

### T5: REST endpoints

**Goal**: mount `/api/v1/webhooks` routes from the dashboard plugin.

**Files**:
- `crates/isengard-plugins/dashboard/src/webhooks.rs` (new)
- `crates/isengard-plugins/dashboard/src/lib.rs` (mount router)
- `crates/isengard-plugins/dashboard/tests/webhooks_api.rs`

**Acceptance**:
- 6 endpoints: GET list, POST create, GET one, PUT update, DELETE, GET deliveries, POST test.
- POST returns the secret in plaintext exactly once (on create); GET returns the masked secret (`****` + last 4 chars).
- POST `/webhooks/{id}/test` enqueues a `webhook.test` delivery and returns the row.
- 4 integration tests with an `axum::Router`-driven test harness.

### T6: UI Settings -> Webhooks tab

**Goal**: add the Webhooks tab + list + add modal + deliveries panel.

**Files**:
- `crates/isengard-plugins/dashboard/web/components/WebhooksSettings.vue`
- `crates/isengard-plugins/dashboard/web/components/AddWebhookModal.vue`
- `crates/isengard-plugins/dashboard/web/components/WebhookDeliveriesPanel.vue`
- `crates/isengard-plugins/dashboard/web/composables/useWebhooks.ts`
- `crates/isengard-plugins/dashboard/web/pages/settings/index.vue` (add tab entry)

**Acceptance**:
- New tab `webhooks` shows up in `SettingsTabs`.
- Add modal: URL + secret (with auto-generate button) + kinds + enabled.
- After create, the modal flashes the secret once with a copy button and a warning string about not being shown again.
- Per-webhook deliveries panel filterable by status.
- Empty state: explainer + primary CTA inside the container (per `feedback_empty_states`).
- `bun run build` succeeds; the SPA bundle is regenerated and embedded.

### T7: Wrap-up

**Goal**: docs + status flips + gate sweep + PR.

**Files**:
- `design/pages/settings-webhooks.md`: status -> `phase-12a-implemented`, add Implementation status block citing the merged plan
- `docs/RELEASE_NOTES_PHASE_12A.md`: operator-facing notes with a Python receiver example for verifying signatures

**Acceptance**:
- All gates green: `cargo build`, `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, `cargo deny check`.
- No em dashes / en dashes in any new file.
- Branch pushed; PR opened against `next` titled `feat: phase 12a (outbound webhooks)` with body `Closes #53`.
