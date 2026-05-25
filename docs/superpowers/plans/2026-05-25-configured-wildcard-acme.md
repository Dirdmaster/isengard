# Configured Wildcard ACME Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make wildcard ACME reconcile live from `isd configure` without controller recreation.

**Architecture:** Add a configured ACME reconciler under `isengard-controller::acme`. It resolves current configure keys on every tick, falls back to boot env when configure is unset, then reuses the existing scheduler tick and routing pusher.

**Tech Stack:** Rust, Tokio, existing controller config dispatcher, existing ACME DNS-01 scheduler.

---

### Task 1: Config resolver

**Files:**
- Create: `crates/isengard-controller/src/acme/configured.rs`
- Modify: `crates/isengard-controller/src/acme/mod.rs`

- [x] **Step 1: Write failing tests**

Add tests for `resolve_desired_config`:

- configured wildcard zone creates `*.zone` plus apex group
- non-wildcard zone is ignored
- configure values override fallback env config
- fallback env config still works when configure is unset

- [x] **Step 2: Run tests to verify failure**

Run: `cargo test -p isengard-controller configured_acme --lib`

Expected: fail because `acme::configured` does not exist.

- [x] **Step 3: Implement resolver**

Create `configured.rs` with a `DesiredAcmeConfig` struct and async resolver using `ConfigDispatcher::get`.

- [x] **Step 4: Verify tests pass**

Run: `cargo test -p isengard-controller configured_acme --lib`

Expected: pass.

- [x] **Step 5: Commit**

```sh
git add crates/isengard-controller/src/acme/configured.rs crates/isengard-controller/src/acme/mod.rs docs/superpowers/specs/2026-05-25-configured-wildcard-acme-design.md docs/superpowers/plans/2026-05-25-configured-wildcard-acme.md
git commit -m "docs: design configured wildcard acme"
```

### Task 2: Runtime reconciler

**Files:**
- Modify: `crates/isengard-controller/src/acme/configured.rs`
- Modify: `crates/isengard-controller/src/lib.rs`

- [x] **Step 1: Write failing test**

Add a test proving the reconciler tick calls the scheduler path only when configured wildcard groups exist.

- [x] **Step 2: Run test to verify failure**

Run: `cargo test -p isengard-controller configured_acme --lib`

Expected: fail because the tick function is not implemented.

- [x] **Step 3: Implement minimal reconciler**

Add `spawn_configured_scheduler` and `tick_configured_scheduler`. Build an `AcmeDns01Client` from current config and call `scheduler::tick`.

- [x] **Step 4: Wire controller boot**

Replace the static env-only scheduler block in `run_controller` with the configured scheduler. Pass the existing boot `AcmeConfig` as fallback.

- [x] **Step 5: Verify**

Run: `cargo test -p isengard-controller configured_acme acme::scheduler --lib`

Expected: pass.

- [ ] **Step 6: Commit**

```sh
git add crates/isengard-controller/src/acme/configured.rs crates/isengard-controller/src/lib.rs
git commit -m "fix: reconcile wildcard acme from configure"
```

### Task 3: Final verification

- [ ] **Step 1: Format**

Run: `cargo fmt --check`

- [ ] **Step 2: Focused tests**

Run: `cargo test -p isengard-controller configured_acme acme::scheduler --lib`

- [ ] **Step 3: Controller clippy**

Run: `cargo clippy -p isengard-controller --all-targets -- -D warnings`

- [ ] **Step 4: Commit any polish**

Commit only if verification required changes.
