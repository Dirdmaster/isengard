# Phase 3e: Configurable Cycle Interval + Old Image Cleanup + E2E

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Phase 3 lands. Cycle interval becomes plugin-configurable (still defaults to 30s). After a successful update, the old image is removed (best-effort, errors silently if other containers still use it). A real end-to-end test exercises the full cycle: a labelled container with an outdated image gets discovered, classified, recreated, and the old image is cleaned up.

**Architecture:** No new modules. Three focused changes: (1) `Updater` reads `cycle_interval_seconds` from `PluginContext.config`; (2) `recreate::update_container` and `self_update::update_self` capture the old image ID before replacement and call `docker.remove_image` after the new container starts (best-effort, anything-but-success is logged at INFO and swallowed); (3) a new `tests/cycle_e2e.rs` integration test runs an in-process updater cycle against a real Docker daemon with a deliberately-stale tag.

**Tech stack:** No new workspace deps. Uses `bollard::image::RemoveImageOptions` (already present in 0.18). Test fixture uses `nginx:1.24-alpine` (small, public, has a known different digest than `nginx:1.25-alpine`).

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-29-platform-pivot-design.md` §9.1 (cleanup of old images post-update) + §12 (integration harness).

---

## Scope

**In:**
- `Updater` reads `PluginContext.config.cycle_interval_seconds` (u64). Defaults to 30 if missing or wrong type. Validated as `>= 5` (sub-5s ticks would hammer registries).
- `recreate::update_container`: capture `old_image_id = inspect.image.clone()` before pull. After successful start + reconnect, call `docker.remove_image(old_image_id, opts, None)` with `force=false, noprune=false`. Log INFO on success, INFO on failure with `error = e` ("old image still in use" is a successful no-op).
- `self_update::update_self`: same pattern — capture old image ID, schedule its removal in the spawned exit task (BEFORE the `process::exit(0)` so the call has a chance to start, even if it doesn't complete).
- `tests/cycle_e2e.rs`: full e2e:
  1. Skip if Docker unavailable
  2. `docker pull nginx:1.24-alpine` and `docker pull nginx:1.25-alpine` (or whatever the latest is at test time — use `:1.24` and `:1.25` as the explicit pair so we don't depend on `:latest` moving)
  3. Tag `1.24-alpine` as `nginx:1.25-alpine` locally (this creates the "stale" condition — local `:1.25-alpine` digest now points to 1.24's contents)
  4. Run a labelled `nginx:1.25-alpine` container
  5. Construct an `Updater`, init it, and call `AgentPlugin::run_cycle` once
  6. Inspect the container — its image ID should now match the real `nginx:1.25-alpine` (which is back in registry)
  7. Cleanup: remove test container + restore original `:1.25-alpine` tag (re-pull)

**Out (deferred to v1.x):**
- Per-container update policy via labels (e.g., `isengard.policy=manual`)
- Healthcheck-driven rollback
- Concurrent updates of multiple candidates
- Pull retry/backoff
- Notification of update events (Phase 4 with the notifier)
- Image cleanup audit log (Phase 4 with journal)
- Pre/post-update hooks (`hooks` plugin)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline 100 + new tests
3. `just ci-local` clean
4. `cycle_e2e` passes against a real Docker daemon (or skips if absent)
5. Tag `v0.1.0-alpha.phase3e` set locally
6. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-plugins/updater/
├── Cargo.toml                       # UNCHANGED
├── src/
│   ├── lib.rs                       # MODIFY: read cycle_interval from config
│   ├── image_ref.rs                 # UNCHANGED
│   ├── labels.rs                    # UNCHANGED
│   ├── auth.rs                      # UNCHANGED
│   ├── registry.rs                  # UNCHANGED
│   ├── self_id.rs                   # UNCHANGED
│   ├── recreate.rs                  # MODIFY: capture old image id, remove after start
│   └── self_update.rs               # MODIFY: capture old image id, schedule remove in exit task
└── tests/
    ├── plugin_loads.rs              # UNCHANGED
    ├── registry_e2e.rs              # UNCHANGED
    ├── recreate_e2e.rs              # UNCHANGED
    └── cycle_e2e.rs                 # NEW: full cycle e2e against real Docker
```

---

## Task 1: Read cycle_interval_seconds from PluginContext config

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs`

- [ ] **Step 1: Replace the hardcoded constant with a config read in `init` and `start`**

In `crates/isengard-plugins/updater/src/lib.rs`:

1. Remove the `const CYCLE_INTERVAL: Duration = Duration::from_secs(30);` line (or keep it as a default value — see step 2).

2. Replace it with a constant for the default and a minimum:

```rust
const DEFAULT_CYCLE_INTERVAL_SECS: u64 = 30;
const MIN_CYCLE_INTERVAL_SECS: u64 = 5;
```

3. Add a field to `Updater`:

```rust
pub struct Updater {
    docker: Option<Docker>,
    registry: Option<Arc<RegistryClient>>,
    cycle_interval: Duration,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}
```

Update `Updater::new()` to default it:

```rust
impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            registry: None,
            cycle_interval: Duration::from_secs(DEFAULT_CYCLE_INTERVAL_SECS),
            cancel: Arc::new(Notify::new()),
            task: None,
        }
    }
}
```

4. In `init`, read the config (insert near the top of `init`, before the docker connect):

```rust
        let cycle_interval_secs = ctx
            .config
            .get("cycle_interval_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_CYCLE_INTERVAL_SECS)
            .max(MIN_CYCLE_INTERVAL_SECS);
        self.cycle_interval = Duration::from_secs(cycle_interval_secs);
        info!(cycle_interval_secs, "updater cycle interval configured");
```

5. In `start`, replace `tokio::time::interval(CYCLE_INTERVAL)` with `tokio::time::interval(self.cycle_interval)` (or capture into a local before the `tokio::spawn`).

The full updated `start` looks like:

```rust
    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let registry = self
            .registry
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let cancel = self.cancel.clone();
        let interval = self.cycle_interval;

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel.notified() => {
                        debug!("updater cycle task cancelled");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = do_cycle(&docker, &registry).await {
                            warn!(error = %e, "updater cycle failed");
                        }
                    }
                }
            }
        });

        self.task = Some(task);
        info!("updater started");
        Ok(())
    }
```

- [ ] **Step 2: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -10
```

Expected: clean. Same lib test count.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): cycle_interval_seconds configurable via plugin context (default 30s, min 5s)"
```

**Self-review checklist:**
- [ ] Build + clippy clean
- [ ] All unit tests pass
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Capture + remove old image on recreate

**Files:**
- Modify: `crates/isengard-plugins/updater/src/recreate.rs`

- [ ] **Step 1: Capture old image ID + remove after start**

In `crates/isengard-plugins/updater/src/recreate.rs`:

1. Add the import for `RemoveImageOptions` to the existing bollard imports:

```rust
use bollard::image::{CreateImageOptions, RemoveImageOptions};
```

2. In `update_container`, capture the old image ID right after `inspect`:

```rust
    let old_image_id = inspect.image.clone();
```

3. After the network reconnect loop, before the final `info!("update complete")`, add the cleanup:

```rust
    if let Some(old) = old_image_id {
        match docker
            .remove_image(
                &old,
                Some(RemoveImageOptions {
                    force: false,
                    noprune: false,
                }),
                None,
            )
            .await
        {
            Ok(_) => info!(old_image = %old, "removed old image"),
            Err(e) => info!(old_image = %old, error = %e, "old image not removed (likely still in use)"),
        }
    }
```

- [ ] **Step 2: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 3: Confirm recreate_e2e still passes**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test recreate_e2e 2>&1 | tail -10
```

Expected: pass (or skip cleanly).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/recreate.rs
cd ~/Projects/isengard && git commit -m "feat(updater): remove old image after successful recreate (best-effort)"
```

**Self-review checklist:**
- [ ] Build + clippy clean
- [ ] All tests pass
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 3: Capture + remove old image on self-update

**Files:**
- Modify: `crates/isengard-plugins/updater/src/self_update.rs`

- [ ] **Step 1: Capture old image ID + schedule removal in the exit task**

In `crates/isengard-plugins/updater/src/self_update.rs`:

1. Add the `RemoveImageOptions` import to the existing bollard imports:

```rust
use bollard::image::RemoveImageOptions;
```

2. In `update_self`, capture the old image ID right after `inspect`:

```rust
    let old_image_id = inspect.image.clone();
```

3. Replace the existing `tokio::spawn(async { sleep + exit })` block at the end of the function with one that also attempts the image removal first:

```rust
    let docker_exit = docker.clone();
    tokio::spawn(async move {
        if let Some(old) = old_image_id {
            match docker_exit
                .remove_image(
                    &old,
                    Some(RemoveImageOptions {
                        force: false,
                        noprune: false,
                    }),
                    None,
                )
                .await
            {
                Ok(_) => info!(old_image = %old, "removed old self image"),
                Err(e) => info!(old_image = %old, error = %e, "old self image not removed"),
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    });
```

- [ ] **Step 2: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/self_update.rs
cd ~/Projects/isengard && git commit -m "feat(updater): remove old self image in exit task (best-effort)"
```

**Self-review checklist:**
- [ ] Build + clippy clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 4: Full cycle e2e test

**Files:**
- Create: `crates/isengard-plugins/updater/tests/cycle_e2e.rs`

- [ ] **Step 1: Create the test**

Create `crates/isengard-plugins/updater/tests/cycle_e2e.rs`:

```rust
//! Full-cycle e2e: pull two real nginx versions, label one as the "current"
//! version of the other (creating a stale local digest), run an in-process
//! updater cycle, verify the container was recreated against the real digest.
//!
//! This is the most expensive test in the suite — pulls two ~10MB images and
//! does a real recreate. Skips if Docker isn't available.

#![allow(clippy::result_large_err)]

use std::time::Duration;

use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions};
use bollard::image::{CreateImageOptions, TagImageOptions};
use futures_util::StreamExt;
use isengard_core::{AgentPlugin, HostMode, Plugin, PluginContext};
use isengard_plugin_updater::Updater;

const OLD_VERSION: &str = "nginx:1.24-alpine";
const NEW_TAG: &str = "nginx:1.25-alpine";
const TEST_CONTAINER: &str = "isengard-3e-cycle";

async fn docker_available() -> Option<Docker> {
    let docker = Docker::connect_with_local_defaults().ok()?;
    docker.version().await.ok()?;
    Some(docker)
}

async fn pull(docker: &Docker, image: &str) -> anyhow::Result<()> {
    let (repo, tag) = image.split_once(':').unwrap_or((image, "latest"));
    let opts = CreateImageOptions::<String> {
        from_image: repo.to_string(),
        tag: tag.to_string(),
        ..Default::default()
    };
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(frame) = stream.next().await {
        if let Err(e) = frame {
            return Err(anyhow::anyhow!("pull {image}: {e}"));
        }
    }
    Ok(())
}

async fn cleanup(docker: &Docker) {
    let _ = docker
        .remove_container(
            TEST_CONTAINER,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

#[tokio::test]
async fn cycle_recreates_stale_container_against_real_registry() {
    let Some(docker) = docker_available().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    cleanup(&docker).await;

    // 1. Pull both versions.
    pull(&docker, OLD_VERSION)
        .await
        .expect("pull 1.24-alpine");
    pull(&docker, NEW_TAG).await.expect("pull 1.25-alpine");

    // 2. Re-tag 1.24 as 1.25 — this creates the "stale" condition: local
    //    nginx:1.25-alpine now points at 1.24's contents, but the registry
    //    still has the real 1.25. The updater should detect this and pull.
    let (new_repo, new_tag_part) = NEW_TAG.split_once(':').unwrap();
    docker
        .tag_image(
            OLD_VERSION,
            Some(TagImageOptions {
                repo: new_repo,
                tag: new_tag_part,
            }),
        )
        .await
        .expect("retag 1.24 as 1.25");

    // 3. Inspect the local 1.25 to confirm the swap took effect.
    let stale_inspect = docker
        .inspect_image(NEW_TAG)
        .await
        .expect("inspect stale 1.25");
    let stale_id = stale_inspect.id.clone().expect("stale image has ID");

    // 4. Run a labelled container against the (now stale) 1.25 tag.
    let mut labels = std::collections::HashMap::new();
    labels.insert("isengard.enable".to_string(), "true".to_string());
    let create = docker
        .create_container(
            Some(CreateContainerOptions::<String> {
                name: TEST_CONTAINER.to_string(),
                platform: None,
            }),
            Config {
                image: Some(NEW_TAG.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .expect("create container");
    docker
        .start_container::<String>(&create.id, None)
        .await
        .expect("start container");

    // 5. Construct an Updater in-process and run one cycle.
    let mut plugin = Updater::new();
    let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);
    plugin.init(&ctx).await.expect("plugin init");
    plugin.start(&ctx).await.expect("plugin start");

    // run_cycle is the AgentPlugin entry point — synchronous one-cycle run
    // that doesn't depend on the internal ticker.
    plugin
        .run_cycle(&ctx)
        .await
        .expect("run_cycle should succeed");

    // Give the recreate path a moment to settle. run_cycle awaits the full
    // recreate, so there should be no race, but Docker's eventual-consistency
    // on inspect can lag.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 6. Inspect the test container — its image should now be the REAL 1.25.
    let updated_inspect = docker
        .inspect_container(TEST_CONTAINER, None)
        .await
        .expect("inspect after update");
    let new_image_id = updated_inspect.image.expect("container has image");

    assert_ne!(
        new_image_id, stale_id,
        "container image should have changed after update; old={stale_id} new={new_image_id}"
    );

    // 7. Cleanup
    plugin.stop().await.expect("plugin stop");
    cleanup(&docker).await;
    // Re-tag pull to restore real 1.25 (defensive; the local repo is shared
    // with the host's docker engine).
    let _ = pull(&docker, NEW_TAG).await;
}
```

- [ ] **Step 2: Run the test (this will take a while — pulls two images)**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test cycle_e2e -- --nocapture 2>&1 | tail -30
```

Expected: passes against real Docker, or skips cleanly if Docker unavailable. Pull time can be 30-60s on first run.

- [ ] **Step 3: Confirm clippy clean**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/tests/cycle_e2e.rs
cd ~/Projects/isengard && git commit -m "test(updater): full cycle e2e — stale tag detected + recreated against real registry"
```

**Self-review checklist:**
- [ ] Test passes (or skips cleanly)
- [ ] Build + clippy clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 5: CI gate + tag + Phase 3 retrospective

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 3e`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 100 (Phase 3d baseline) + 1 cycle_e2e = 101+. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase3e -m "phase 3e: configurable cycle interval + old image cleanup + cycle e2e"
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase3-complete -m "phase 3 complete: updater plugin reaches feature parity with legacy go watchtower behavior"
cd ~/Projects/isengard && git tag -l | grep -E "phase3"
```

Don't push.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 100 baseline + 1 e2e, zero failures
- [ ] `just ci-local` clean
- [ ] Tags `v0.1.0-alpha.phase3e` + `v0.1.0-alpha.phase3-complete` exist locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§9.1, §10, §12) | Plan task |
|---|---|
| Cleanup of old images post-update | Task 2 (recreate) + Task 3 (self-update) |
| Configurable cycle interval | Task 1 |
| Integration harness (§12) — at least one true e2e | Task 4 (cycle_e2e) |

§9.1 Phase 3 is fully addressed across 3a–3e:
- 3a: list containers + log
- 3b: digest check + auth
- 3c: pull + recreate preserving config
- 3d: rename-then-replace self-update
- 3e: cleanup + configurability + e2e

**Type consistency check:**
- `cycle_interval: Duration` field on `Updater` (Task 1) — set in `init`, read in `start`.
- `RemoveImageOptions { force, noprune }` (Tasks 2, 3) — bollard 0.18 type.
- `TagImageOptions { repo, tag }` (Task 4) — used in test setup.

**No new workspace deps.**

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-3e-cleanup-and-config.md`. Subagent-driven execution.
