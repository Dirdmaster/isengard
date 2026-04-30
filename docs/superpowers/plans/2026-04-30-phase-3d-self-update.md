# Phase 3d: Self-Update with Rename-Then-Replace

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** The agent can update its own container without dying mid-update. End state: when the candidate is the agent's own container and `needs_update`, the agent renames itself, creates the replacement under the original name, starts it, and exits cleanly. The replacement boots, discovers and removes the renamed-old sibling, and continues normal operation.

**Architecture:** A new `self_update.rs` module owns the self-update sequence. It reuses `recreate::pull_image` and `recreate::capture_config` for the heavy lifting, but replaces `update_container`'s "stop → remove → create" with "rename → create" so the orchestrator (the old agent) stays alive long enough to finish placing the replacement. After the replacement starts, the old process schedules a clean `std::process::exit(0)` (Docker will not restart it under any restart policy because the exit code is 0 with status `exited`). On startup, the updater's `init` scans for `<my-container-name>-replaced-*` siblings and removes them — this is the "new agent cleans up old" pattern.

**Tech stack:** No new workspace deps. Adds `bollard::container::RenameContainerOptions`. Uses the same network reconnect pattern from 3c. Process exit via `std::process::exit(0)` from a `tokio::spawn`'d task with a 200ms grace window for log flush.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-29-platform-pivot-design.md` §9.1 (rename-then-replace ordering for self-update is one of the "subtle bits" §10 calls out).

---

## Scope

**In:**
- `self_update.rs` module under `crates/isengard-plugins/updater/src/`:
  - `update_self(docker, self_id, image_ref) -> anyhow::Result<()>` — orchestration
  - `cleanup_replaced_siblings(docker, my_name) -> ()` — best-effort startup cleanup
- `update_self` sequence: inspect self → pull new image → rename self with `-replaced-<unix-ts>` suffix → create replacement under original name → start replacement → reconnect networks → schedule `std::process::exit(0)` 200ms later.
- Modify `do_cycle` in `lib.rs`: when the self-protection check triggers (the candidate IS this process's container), instead of WARN-and-skip, call `self_update::update_self`. On error, log WARN and continue (we're still running; let the next cycle try again).
- Modify the `Updater` plugin's `init()` to: after the docker version probe, detect this process's container ID via `self_id::current_container_id()`. If we have one, inspect to learn our own container name, then call `cleanup_replaced_siblings(docker, my_name)`. Best-effort; never fail init.
- Unit tests for the `update_self` orchestration where possible (the parts that don't need a real Docker daemon — name parsing, timestamp formatting, the "what to call the renamed container" helper).
- Smoke documentation in the plan: how to verify self-update manually with a real Docker daemon.

**Out (deferred):**
- Real e2e self-update integration test — requires the agent to actually run inside a container, which is a 3e responsibility (testcontainers). 3d's tests cover the parts that don't need that.
- Old image cleanup (3e)
- Healthcheck-driven verification before exit (v1.x)
- Notification of self-update events (Phase 4 with the journal + notifier)
- Multi-replica self-update coordination (v1.x)
- Rollback on failed self-update (v1.x — for now, a failed self-update means the old agent keeps running and the next cycle retries)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline 97 + new unit tests
3. `just ci-local` clean
4. Manual self-update smoke (only if Docker is on this box AND the agent is run from a container — typically skipped in dev): build the agent into a Docker image, run it, label its own container, retag the image to force `needs_update`, observe rename + new container start + clean exit
5. Tag `v0.1.0-alpha.phase3d` set locally
6. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-plugins/updater/
├── Cargo.toml                       # UNCHANGED
├── src/
│   ├── lib.rs                       # MODIFY: init does cleanup_replaced_siblings; do_cycle calls update_self for self
│   ├── image_ref.rs                 # UNCHANGED
│   ├── labels.rs                    # UNCHANGED
│   ├── auth.rs                      # UNCHANGED
│   ├── registry.rs                  # UNCHANGED
│   ├── self_id.rs                   # UNCHANGED
│   ├── recreate.rs                  # UNCHANGED (3c version)
│   └── self_update.rs               # NEW: update_self + cleanup_replaced_siblings
└── tests/
    ├── plugin_loads.rs              # UNCHANGED
    ├── registry_e2e.rs              # UNCHANGED
    ├── recreate_e2e.rs              # UNCHANGED
    └── self_update_unit.rs          # NEW (or unit tests inline in self_update.rs)
```

`self_update.rs` is a focused module (~150 lines): orchestration + cleanup. Keeping it separate from `recreate.rs` is deliberate — the rename-first ordering is a different policy from stop-first, even though they share helpers.

---

## Task 1: self_update module with update_self orchestration

**Files:**
- Create: `crates/isengard-plugins/updater/src/self_update.rs`
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add `pub mod self_update;`)

- [ ] **Step 1: Create the module**

Create `crates/isengard-plugins/updater/src/self_update.rs`:

```rust
//! Self-update via rename-then-replace.
//!
//! When the agent's own container is `needs_update`, naive stop+remove kills
//! the orchestrator mid-update. The fix: rename the old container out of the
//! way, create the replacement under the original name, start it, then exit
//! cleanly. Docker's restart policies won't restart a container that exited 0.
//!
//! On startup, `cleanup_replaced_siblings` removes any leftover `-replaced-*`
//! containers — that's how the new agent (us, after a self-update) cleans up
//! after the old one.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::container::{
    CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    RenameContainerOptions,
};
use bollard::network::ConnectNetworkOptions;
use tracing::{info, warn};

use crate::image_ref::ImageRef;
use crate::recreate::{capture_config, pull_image};

/// Self-update orchestration. Inspect → pull → rename → create → start →
/// reconnect networks → schedule clean exit.
///
/// On success, this function returns `Ok(())` AND schedules the current
/// process to exit ~200ms later (giving log flush a moment). The caller
/// should not perform further work after this returns Ok.
pub async fn update_self(
    docker: &Docker,
    self_id: &str,
    new_image_ref: &ImageRef,
) -> anyhow::Result<()> {
    let inspect = docker
        .inspect_container(self_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("inspect self {self_id}: {e}"))?;

    let original_name = inspect
        .name
        .as_deref()
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_default();
    if original_name.is_empty() {
        return Err(anyhow::anyhow!("self container has no name"));
    }

    pull_image(docker, new_image_ref).await?;

    let new_image_str = format!(
        "{}/{}:{}",
        new_image_ref.registry, new_image_ref.repository, new_image_ref.tag
    );
    let spec = capture_config(&inspect, &new_image_str);

    let renamed = renamed_self_name(&original_name);
    info!(
        from = %original_name,
        to = %renamed,
        "renaming self before replacement"
    );
    docker
        .rename_container(self_id, RenameContainerOptions { name: renamed.clone() })
        .await
        .map_err(|e| anyhow::anyhow!("rename self {self_id}: {e}"))?;

    info!(name = %original_name, image = %spec.image, "creating replacement under original name");
    let create = docker
        .create_container(
            Some(CreateContainerOptions::<String> {
                name: original_name.clone(),
                platform: None,
            }),
            spec.config,
        )
        .await
        .map_err(|e| {
            // Best-effort rollback: rename ourselves back so the next cycle
            // can try again. If this fails too, we're in a stuck state but
            // still alive.
            let docker = docker.clone();
            let renamed = renamed.clone();
            let original_name = original_name.clone();
            let self_id = self_id.to_string();
            tokio::spawn(async move {
                let _ = docker
                    .rename_container(
                        &self_id,
                        RenameContainerOptions { name: original_name.clone() },
                    )
                    .await
                    .map_err(|e| warn!(error = %e, "failed to rename self back after create error"));
                let _ = renamed; // keep moved
            });
            anyhow::anyhow!("create replacement {original_name}: {e}")
        })?;

    info!(name = %original_name, new_id = %create.id, "starting replacement");
    docker
        .start_container::<String>(&create.id, None)
        .await
        .map_err(|e| anyhow::anyhow!("start replacement {original_name}: {e}"))?;

    for (net_name, settings) in &spec.networks {
        let opts = ConnectNetworkOptions {
            container: create.id.clone(),
            endpoint_config: settings.clone(),
        };
        if let Err(e) = docker.connect_network(net_name, opts).await {
            warn!(network = %net_name, error = %e, "network reattach failed for replacement");
        }
    }

    info!("self-update complete; exiting current process in 200ms");
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    });

    Ok(())
}

/// Generate the name to rename our own container to before replacing it.
/// Uses a unix-second timestamp suffix so multiple replacements don't collide.
pub fn renamed_self_name(original: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{original}-replaced-{ts}")
}

/// Best-effort: find any container whose name starts with
/// `<my_name>-replaced-` and remove it. Called from updater `init`.
pub async fn cleanup_replaced_siblings(docker: &Docker, my_name: &str) {
    let prefix = format!("{my_name}-replaced-");

    let mut filters = HashMap::new();
    // bollard's filter expects a substring/prefix match for "name"
    filters.insert("name".to_string(), vec![prefix.clone()]);

    let opts = ListContainersOptions::<String> {
        all: true,
        filters,
        ..Default::default()
    };

    let containers = match docker.list_containers(Some(opts)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "list_containers failed during replaced-sibling cleanup");
            return;
        }
    };

    for c in containers {
        let Some(id) = c.id else { continue };
        // Bollard returns names with leading "/"; verify the prefix match
        // didn't false-positive on substring (it shouldn't, but defensive).
        let name_matches = c
            .names
            .as_ref()
            .map(|ns| {
                ns.iter()
                    .any(|n| n.trim_start_matches('/').starts_with(&prefix))
            })
            .unwrap_or(false);
        if !name_matches {
            continue;
        }

        info!(container = %id, "removing replaced sibling");
        if let Err(e) = docker
            .remove_container(
                &id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            )
            .await
        {
            warn!(container = %id, error = %e, "failed to remove replaced sibling");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_self_name_includes_original_and_replaced_marker() {
        let r = renamed_self_name("isengard-agent");
        assert!(r.starts_with("isengard-agent-replaced-"));
    }

    #[test]
    fn renamed_self_name_timestamp_is_numeric() {
        let r = renamed_self_name("foo");
        let suffix = r.strip_prefix("foo-replaced-").unwrap();
        assert!(suffix.chars().all(|c| c.is_ascii_digit()));
        assert!(!suffix.is_empty());
    }

    #[test]
    fn renamed_self_name_handles_dashes_in_original() {
        let r = renamed_self_name("my-app-agent");
        assert!(r.starts_with("my-app-agent-replaced-"));
    }
}
```

- [ ] **Step 2: Add `pub mod self_update;` to lib.rs**

In `crates/isengard-plugins/updater/src/lib.rs`, after the other `pub mod` lines:

```rust
pub mod self_update;
```

- [ ] **Step 3: Build + test + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater self_update:: 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: build clean, 3 tests pass, clippy clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/self_update.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): self_update module — rename-then-replace + cleanup of replaced siblings"
```

**Self-review checklist:**
- [ ] All 3 unit tests pass
- [ ] Build + clippy clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Wire updater init to cleanup replaced siblings

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (add cleanup call to `init`)

- [ ] **Step 1: Add cleanup invocation in init**

In `crates/isengard-plugins/updater/src/lib.rs`, find the `init` function. After the existing block that constructs the registry client and stores it (the `self.registry = Some(Arc::new(registry));` line), add:

```rust
        // If we're running inside a container, clean up any leftover
        // `<our-name>-replaced-*` siblings from a prior self-update.
        if let Some(my_id) = crate::self_id::current_container_id() {
            match docker.inspect_container(&my_id, None).await {
                Ok(self_inspect) => {
                    let my_name = self_inspect
                        .name
                        .as_deref()
                        .map(|s| s.trim_start_matches('/').to_string())
                        .unwrap_or_default();
                    if !my_name.is_empty() {
                        crate::self_update::cleanup_replaced_siblings(&docker, &my_name).await;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "could not inspect self container; skipping replaced-sibling cleanup");
                }
            }
        }
```

- [ ] **Step 2: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -10
```

Expected: clean. Lib tests count unchanged (no new tests — this is integration-only behaviour).

- [ ] **Step 3: Confirm phase 3a integration test still passes**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test plugin_loads 2>&1 | tail -10
```

Expected: pass (or skip if no Docker). The init path now does extra work, but `cleanup_replaced_siblings` short-circuits when `current_container_id()` returns None (which it does on macOS dev).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): init cleans up replaced siblings from prior self-update"
```

**Self-review checklist:**
- [ ] All updater unit tests pass
- [ ] Phase 3a integration test passes (or skips)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 3: Wire do_cycle to call update_self for self

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs` (replace the WARN-and-skip from 3c with `update_self` call)

- [ ] **Step 1: Replace the self-protection branch**

In `crates/isengard-plugins/updater/src/lib.rs`, find the `(Some(local), Some(remote)) =>` arm of `do_cycle` that includes the self-protection check from Phase 3c. The current logic is:

```rust
                // Self-protection: skip if this container is the agent itself.
                if let Some(self_id) = crate::self_id::current_container_id() {
                    if c.id.as_deref() == Some(self_id.as_str()) {
                        warn!(
                            container = %name,
                            "skipping self-update; deferred to phase 3d"
                        );
                        continue;
                    }
                }
```

Replace it with:

```rust
                // Self-update: when the candidate is this process's own
                // container, use rename-then-replace ordering (3d).
                if let Some(self_id) = crate::self_id::current_container_id() {
                    if c.id.as_deref() == Some(self_id.as_str()) {
                        info!(container = %name, "self-update path: rename-then-replace");
                        match self_update::update_self(docker, &self_id, &image_ref).await {
                            Ok(()) => {
                                // update_self schedules process exit; bail out
                                // of the cycle so we don't keep working.
                                info!("self-update succeeded; cycle ending early");
                                return Ok(());
                            }
                            Err(e) => {
                                warn!(error = %e, "self-update failed; will retry next cycle");
                                continue;
                            }
                        }
                    }
                }
```

Add the import alongside the existing crate-local references at the top of `lib.rs` (note `pub mod self_update;` already binds the name, but a `use` for nested path readability is fine — match how `recreate` is currently referenced; if `recreate::update_container` is referenced as `recreate::...` directly without a `use`, do the same here):

The existing reference to `recreate` is via `recreate::update_container(...)`. Use the same convention here: `self_update::update_self(...)`. No new `use` line needed.

- [ ] **Step 2: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -10
```

Expected: clean. Same lib test count.

- [ ] **Step 3: Confirm integration tests still pass**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test plugin_loads --test registry_e2e --test recreate_e2e 2>&1 | tail -15
```

Expected: all pass (or skip cleanly if Docker/network unavailable).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): cycle takes self-update path on own container instead of skipping"
```

**Self-review checklist:**
- [ ] All updater unit tests pass
- [ ] All integration tests pass (or skip)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 4: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 3d`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 97 (Phase 3c baseline) + 3 self_update unit tests = 100. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase3d -m "phase 3d: self-update via rename-then-replace + replaced-sibling cleanup"
cd ~/Projects/isengard && git tag -l | grep phase3d
```

Don't push.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 97 baseline + 3 new tests, zero failures
- [ ] `just ci-local` clean
- [ ] Tag `v0.1.0-alpha.phase3d` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§9.1, §10) | Plan task |
|---|---|
| Rename-first self-update (subtle bit) | Task 1 (`update_self`) |
| Cleanup of post-self-update containers | Task 1 (`cleanup_replaced_siblings`) + Task 2 (called from init) |
| Wired into the cycle | Task 3 (`do_cycle` self branch) |

Self-update e2e validation requires the agent to actually run inside Docker — that's a 3e responsibility (testcontainers).

**Type consistency check:**
- `update_self(&Docker, &str, &ImageRef)` (Task 1) — called from `do_cycle` (Task 3) with `(docker, &self_id, &image_ref)`.
- `renamed_self_name(&str) -> String` (Task 1) — called inside `update_self`.
- `cleanup_replaced_siblings(&Docker, &str)` (Task 1) — called from `init` (Task 2) with `(&docker, &my_name)`.
- `pull_image(&Docker, &ImageRef)` and `capture_config(&ContainerInspectResponse, &str) -> RecreateSpec` from 3c — used by `update_self`.
- `self_id::current_container_id() -> Option<String>` from 3c — used by both `init` (Task 2) and `do_cycle` (Task 3).

**No new workspace deps.** All bollard types used (`RenameContainerOptions`, `RemoveContainerOptions`, `CreateContainerOptions`, `ListContainersOptions`, `ConnectNetworkOptions`) are part of the existing `bollard = "0.18.1"` dep.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-3d-self-update.md`. Subagent-driven execution.
