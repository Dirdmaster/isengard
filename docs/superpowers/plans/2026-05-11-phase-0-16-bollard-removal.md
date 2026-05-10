# Phase 0.16 Bollard Removal: Implementation Plan

> Spec: [`2026-05-11-phase-0-16-bollard-removal-design.md`](../specs/2026-05-11-phase-0-16-bollard-removal-design.md). Branch target: `phase/0-16-bollard-removal` (stacks on `next`).

## Scope

Staged removal of `bollard` from the workspace. Eight sequenced steps, each ending in a commit that compiles + tests green at HEAD. Each step is defensible in isolation: a reviewer can land step N without locking in step N+1, and the workspace stays buildable + the agent stays runnable in wisp mode at every commit boundary.

## Prerequisite gates (BEFORE step 1)

Both must be confirmed before this arc starts. If either is missing, file as a wisp follow-up phase and pause this arc.

- **Wisp pull-auth audit.** `wisp-image::pull` must consume the same registry-credentials shape the updater plugin loads via `auth::DockerConfig::load_default`. Confirm by spinning a private-registry test against a wisp host. If gap exists, file `wisp/pull-auth` task and block.
- **Trait field gap audit for blue-green driver.** Walk every `cfg_in.*` field that `RealDriverDeps::start_green` (and `pull_and_recreate_at_digest`) copies from blue's `bollard::Config<String>`. Verify each field has a corresponding `ContainerCreateSpec` field. If any (e.g. `OpenStdin`, `Tty`, raw `HostConfig.PidMode`) is missing, extend `ContainerCreateSpec` + both backend impls FIRST, in a separate prep commit on `next` that ships independently of this arc.

## Steps

Eight commits. Each lists files touched, the verification gates that must pass at the end of the step, and the commit message subject.

### Step 1: delete dead bollard-typed code paths

Pure delete; no behavior change. Smallest possible step that drops bollard touch-points without anyone noticing.

- Delete `RunningService::from_inspect` from `crates/isengard-agent/src/compose_reconciler.rs` (no callers; verify with `grep -rn from_inspect crates/`).
- Delete `list_container_snapshots()` (no-arg variant) from `crates/isengard-agent/src/container_snapshot.rs`. Update the doc comment on `snapshots_via_backend_or_legacy` to drop the fallback; rename to `snapshots_via_backend` and make `backend: &Arc<dyn RuntimeBackend>` non-optional.
- Delete `MaterialisedSecret::as_bollard_bind` from `crates/isengard-agent/src/secret_fetch.rs` + its sole unit test `materialised_secret_renders_bollard_bind`.
- Delete `pick_container_ip` (the bollard EndpointSettings picker) from `crates/isengard-agent/src/proxy/discovery.rs` + its tests. Keep `pick_snapshot_ip` + `resolve_container_ip`.
- Update the `proxy/discovery.rs` docstring to drop the bollard-shaped-surface explanation.

Verify:
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace -D warnings`

Commit: `refactor(agent): delete dead bollard-typed helpers`

### Step 2: move the deployment driver onto RuntimeBackend

The load-bearing refactor. Single step because half-moving the driver leaves a worse state than starting.

- Change `DeploymentSupervisor::new(.., docker: Arc<bollard::Docker>, ..)` to take `backend: Arc<dyn RuntimeBackend>`. Update the field type. Update `lib.rs` call site to pass `backend.clone()` instead of `docker`. Update all three `tests/deployment_blue_green_*.rs` to construct a `WispBackend` (or a mock; see below).
- Refactor `RealDriverDeps` to hold `Arc<dyn RuntimeBackend>` instead of `Arc<bollard::Docker>`. Rewrite `start_green`, `stop_and_remove`, `pull_and_recreate_at_digest` to drive the trait.
  - `start_green`: trait `inspect_container(blue_id)` -> ContainerSnapshot, build a `ContainerCreateSpec` for the green (clone every field except image / name), call `ensure_image` + `create_container` + `connect_network` for each network from snapshot.network_settings.ip_addresses keys + `start_container`. Re-inspect to get the green IP.
  - `stop_and_remove`: trait `stop_container(id, 10)` + `remove_container(id, force=true)`.
  - `pull_and_recreate_at_digest`: trait `ensure_image(repo@digest)` + the same create / start path.
- For the deployment tests, refactor to use `WispBackend::from_env` inside the existing wisp OrbStack VM (matching `wisp_compose_e2e`'s setup pattern). Mark them `#[ignore]` with the existing `WISP_E2E=1` gating env var.

Verify:
- `cargo test -p isengard-agent` (Mac side without docker)
- `WISP_E2E=1 cargo test -p isengard-agent --test deployment_blue_green_happy -- --ignored` inside the wisp VM
- `cargo clippy --workspace -D warnings`

Commit: `refactor(agent): blue-green driver drives RuntimeBackend`

### Step 3: replace BollardLogSource with BackendLogSource

Logs are well-isolated; pure refactor.

- New `BackendLogSource` struct in `crates/isengard-agent/src/logs.rs`, wrapping `Arc<dyn RuntimeBackend>`. Implement `LogSource::tail` by calling `backend.stream_logs(container_name, LogOptions { follow, tail, timestamps: true, .. })` and mapping `LogChunk` -> `DecodedLine`.
- Replace `BollardLogSource` construction site in `lib.rs` with `BackendLogSource { backend: backend.clone() }`.
- Drop `decode_bollard_frame` + the `bollard::container::LogOutput` match (the trait surface already emits typed `LogChunk` variants).
- Delete `BollardLogSource` struct + its `LogSource` impl.

Verify:
- `cargo test -p isengard-agent` (logs unit tests pass against the new struct)
- Live smoke in the wisp VM: tail a running container and verify `LogChunk`s reach the controller.

Commit: `refactor(agent): logs source drives RuntimeBackend.stream_logs`

### Step 4: delete compose_import + compose_export + the example

Drop the v0.3c migration path. The release notes for this PR carry the rationale: wisp installs have nothing to import; operators who need to migrate from docker stay on v0.5.x for that operation.

- Delete `crates/isengard-agent/src/compose_import.rs`.
- Delete `crates/isengard-agent/src/compose_export.rs`.
- Delete `crates/isengard-agent/examples/compose_smoke.rs`.
- Remove the `compose_import::spawn(...)` call from `crates/isengard-agent/src/lib.rs` (the entire `if let Some(docker) = docker.as_ref() { compose_import::spawn(...) }` block).
- Remove `pub mod compose_import;` and `pub mod compose_export;` from `lib.rs`.
- Search for any other references to `compose_import` / `compose_export` / `build_compose` and clean up.
- Update `docs/RELEASE_NOTES_*.md` is not required here; the next phase's release notes carry the deprecation entry.

Verify:
- `cargo build --workspace`
- `cargo test --workspace`
- `grep -rn "compose_import\|compose_export" crates/` returns no hits.

Commit: `refactor(agent): drop compose_import + compose_export (v0.3c migration retires with bollard)`

### Step 5: refactor updater plugin onto RuntimeBackend

The plugin's bollard-direct surface is independent of the agent's. Refactor in one step to avoid a partially-trait-driven plugin.

- Add `runtime: Option<Arc<dyn RuntimeBackend>>` to `crates/isengard-plugins/updater/src/lib.rs::Updater`. Wire it through `PluginContext` (new accessor: `PluginContext::runtime() -> Option<Arc<dyn RuntimeBackend>>`).
- Update `Updater::init` to pull the runtime out of context; replace `Docker::connect_with_local_defaults` construction with the trait handle.
- Rewrite `cycle` to drive `runtime.list_containers(ListFilter::default())` instead of `docker.list_containers`. Map the resulting `ContainerSnapshot` slice through `dispatch_helpers` (which itself moves to consuming `ContainerSnapshot` instead of `ContainerInspectResponse`).
- Rewrite `recreate.rs`:
  - `capture_config(inspect, new_image)` -> `capture_config(snapshot, new_image)` (consume `ContainerSnapshot`, produce a `ContainerCreateSpec`).
  - `pull_image(docker, image)` -> `runtime.ensure_image(image)`.
  - `update_container(docker, ...)` -> drive the trait: stop + remove blue, create green from spec, connect networks, start.
- Delete `updater/src/self_update.rs` (superseded by `crates/isengard-agent/src/self_update.rs`).
- Rewrite `updater/src/dispatch_helpers.rs` to consume `ContainerSnapshot` instead of `ContainerInspectResponse` / `ContainerSummary`.
- Update `Cargo.toml`: drop `bollard`, add `isengard-agent` path-dep behind a re-export OR pull `RuntimeBackend` + `ContainerSnapshot` from a new shared crate (`isengard-runtime`?). The cleanest path is to lift the trait + types into `isengard-core` so both `isengard-agent` and `isengard-plugin-updater` depend on it without circular dep. Recommended: lift to `isengard-core` in a dedicated prep commit BEFORE this step lands; that commit is independent of the bollard removal and can be reviewed on its own.
- Refactor all updater tests (`cycle_e2e`, `recreate_e2e`, `plugin_loads`, `policy_*`) to drive a wisp-backed runtime. Mark live-runtime ones `#[ignore]` with the existing `WISP_E2E=1` gate.

Verify:
- `cargo build -p isengard-plugin-updater`
- `cargo test -p isengard-plugin-updater` (mock-backed unit tests)
- `WISP_E2E=1 cargo test -p isengard-plugin-updater --test cycle_e2e -- --ignored` inside the wisp VM.
- `cargo clippy --workspace -D warnings`

Commit: `refactor(updater): plugin drives RuntimeBackend (drop bollard)`

(Optional pre-commit if the trait-lift is needed: `refactor(core): lift RuntimeBackend trait into isengard-core for cross-crate use`. Lands first, on its own.)

### Step 6: deprecation warning + soft-cutover release (Release N == v0.5.0)

The spec recommends a two-release deprecation window. This step is the FIRST release boundary.

- Add to `crates/isengard-agent/src/runtime/select.rs::select_backend`: when `BackendChoice::Docker` is selected, log a `warn!` with the explicit text `"ISENGARD_RUNTIME=docker is deprecated and will be removed in v0.6. Migrate to wisp; see docs/MIGRATING_TO_WISP.md"`.
- New `docs/MIGRATING_TO_WISP.md` covering:
  - The two-release deprecation window.
  - The `isd stack down <stack>` migration script.
  - How to flip `ISENGARD_RUNTIME` in the systemd env file.
  - What does and does not carry forward (volumes by name: yes; bind mounts with absolute paths: yes; docker-network attachments: rewrite to wisp networks).
- New `docs/RELEASE_NOTES_v0_5_0_BOLLARD_DEPRECATION.md` summarising the deprecation, with the same migration steps inline and the explicit "v0.6 deletes it" promise.
- NO code deletion in this step. BollardBackend still works; the warning is the only behavior change.

Verify:
- `cargo build --workspace`
- `cargo test --workspace`
- Manual smoke: `ISENGARD_RUNTIME=docker isengard agent ...` logs the deprecation warning at boot.

Commit: `feat(agent): deprecate ISENGARD_RUNTIME=docker; warning at boot (v0.5 release boundary)`

Tag boundary: after step 6 lands on `next`, the v0.5.0 release tag goes out. Operators get one cycle to migrate.

### Step 7: delete BollardBackend (Release N+1 == v0.6.0)

After v0.5.0 has shipped and the deprecation window has elapsed (operator + calendar gate; do NOT merge step 7 the same week as step 6).

- Delete `crates/isengard-agent/src/runtime/bollard_backend.rs`.
- Delete `crates/isengard-agent/tests/bollard_backend_regression.rs`.
- Delete `crates/isengard-agent/tests/proxy_label_discovery_e2e.rs` (the bollard variant; wisp coverage in `wisp_labels_and_discovery`).
- Delete the `#[cfg(test)] tests` `mock_backend_satisfies_dyn_trait` test from `runtime/mod.rs`? No, keep it; the mock is generic.
- Remove the `Docker` arm from `runtime/select.rs::select_backend`:
  - `BackendChoice::Docker` now returns `RuntimeError::UnknownBackend("docker is removed; set ISENGARD_RUNTIME=wisp")`.
  - Or simpler: drop `Docker` from the `BackendChoice` enum entirely; `from_env_value` returns `Unknown("docker")` for the legacy value, which `select_backend` then turns into the appropriate error.
- Remove `fn as_bollard(...)` from `RuntimeBackend` trait in `runtime/mod.rs`. Drop the default impl; drop the mock impl in tests.
- Remove `pub mod bollard_backend;` from `runtime/mod.rs`.
- Remove `let docker = backend.as_ref().and_then(|b| b.as_bollard());` from `lib.rs`. Verify nothing else in `lib.rs` references the `docker` binding (Steps 2 to 5 should have removed every caller).
- Update every docstring in `runtime/mod.rs`, `compose_apply.rs`, `labels.rs`, `proxy/discovery.rs`, `proxy/mod.rs`, `container_snapshot.rs` to drop "bollard" mentions. Replace with "the runtime" or "the trait" as appropriate. Keep references where the comment is genuinely historical ("Phase 0.4: rewritten to drive off RuntimeBackend instead of bollard directly" stays as legitimate history).

Verify:
- `cargo build --workspace`
- `cargo test --workspace`
- `grep -rn "BollardBackend\|use bollard\|as_bollard\(\)" crates/` returns zero hits in src/ (test fixtures / historical comments OK).

Commit: `refactor(agent): delete BollardBackend + as_bollard escape hatch (v0.6 release boundary)`

### Step 8: drop the workspace bollard dep + cargo-deny entry

The cleanup commit. No code changes beyond `Cargo.toml` / `Cargo.lock` / `deny.toml`.

- Remove `bollard = { ... }` from workspace `Cargo.toml`.
- Remove `bollard.workspace = true` from `crates/isengard-agent/Cargo.toml`.
- Remove `bollard.workspace = true` from `crates/isengard-plugins/updater/Cargo.toml` (if step 5 missed it).
- Remove the `RUSTSEC-2025-0134` ignore entry from `deny.toml` (the rustls-pemfile transitive that exists only because of bollard).
- Run `cargo update` to drop unused transitive crates from `Cargo.lock`.
- New `docs/RELEASE_NOTES_v0_6_0_BOLLARD_REMOVAL.md` summarising the removal: what got dropped, what stayed (compose.yaml + install-docker.sh as legacy escape hatches), the binary-size delta, the cargo-deny clean-up.

Verify:
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo deny check` -- must be green WITHOUT the RUSTSEC-2025-0134 line.
- `cargo tree -e features | grep bollard` -- must return no results.
- `cargo bloat --release` or `nm` on the resulting binary -- no `bollard` symbols, no `hyperlocal` symbols.
- Binary size before vs after: target a 5 to 10% reduction (Phase 0.7 binaries are ~6.9 MB; expect ~6.3 MB after).

Commit: `chore(workspace): drop bollard dep + cargo-deny ignore (Phase 0.16 final)`

## Branch + PR shape

- Steps 1 to 5 land as one stacked PR series on `phase/0-16-bollard-removal` against `next`. Each commit is a single PR if reviews are slow; otherwise group steps 1+3 (small + isolated) and 2+5 (load-bearing refactors) into two larger PRs.
- Step 6 is its own PR; lands as part of the v0.5.0 release boundary. Tag v0.5.0 after merge.
- Steps 7 and 8 land together as one PR against `next`, gated on the calendar window (one release cycle, typically 4 to 6 weeks after v0.5.0). Tag v0.6.0 after merge.

## Reviewer checkpoints (operator-gated)

Four explicit review checkpoints; each is a place to pause and confirm direction before continuing.

1. **After step 2 lands:** wisp-backed blue-green driver works end-to-end in the wisp VM against a real registry. Operator runs the test manually + sights the green container handoff.
2. **After step 5 lands:** updater plugin cycle runs against a wisp host and detects a digest drift on a private-registry image. Operator confirms registry auth still works.
3. **Before step 6 lands:** review the deprecation warning text + the migration doc copy. The wording in this commit is operator-visible.
4. **Before step 7 lands:** confirm the deprecation window has elapsed AND no live operator install is known to still be running `ISENGARD_RUNTIME=docker`. This is the point of no return for that env var.

## Non-goals

Same as the spec's non-goals; mirrored here for clarity:

- compose.yaml + install-docker.sh stay in the tree as documented escape hatches.
- `ISENGARD_RUNTIME` env var stays in the API surface (with reduced accepted values).
- No proto changes, no dashboard changes, no wisp feature work inside this arc.
- No squashing across the release boundary: step 6 must be a distinct commit + release tag so the deprecation message ships before the deletion ships.
