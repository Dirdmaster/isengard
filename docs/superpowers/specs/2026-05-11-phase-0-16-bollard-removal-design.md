# Phase 0.16: Bollard Removal Design (SUPERSEDED)

**Status:** SUPERSEDED 2026-05-11 by operator decision. Bollard stays first-class. Do not implement the plan below.
**Phase:** 0.16 (obsolete)
**Date:** 2026-05-11

## Pivot (2026-05-11 operator decision)

Operator framing: the `RuntimeBackend` trait IS the design value. Killing one backend kills the abstraction's point. Bollard stays first-class as the docker adapter; wisp is the default for new (Phase 0.8+) installs; future backends (podman, containerd, k8s) get the same treatment under the trait. No removal. No deprecation. The trait is permanent.

The 8-step plan in the companion plan doc is OBSOLETE. Do not dispatch its implementation.

If the operator changes their mind later (e.g., bollard's maintenance burden grows beyond the trait's value), revisit this spec then.

## Original intent (pre-pivot, kept for historical context)

A staged removal of the `bollard` Rust docker SDK from every crate in the workspace. The agent's `RuntimeBackend` trait (Phase 0.4) and the wisp backend (Phases 0.1 to 0.6) already cover every container concern. Phase 0.8 (#135) shipped systemd-native install: a fresh Phase 0.8 host has no docker installed, so `BollardBackend` is dead code on those hosts. This phase was the cleanup arc: delete `BollardBackend`, the bollard-direct call sites that still bypass the trait, and the deny.toml exceptions that exist only because of bollard's transitive deps.

This phase was **scoping only**. Implementation has been cancelled.

## Goal + done bar

**Goal:** make wisp the only container runtime the agent knows how to drive. Drop the `BollardBackend` impl, the `as_bollard()` trait escape hatch, the bollard-direct deployment driver, and the bollard-direct updater plugin paths.

**Done bar:**
- `grep -rn "use bollard" crates/` returns zero hits in source code (test fixtures may keep bollard type names in comments; that is documentation noise, not a dep).
- `grep -rn "bollard" Cargo.toml crates/*/Cargo.toml` returns zero hits.
- `cargo deny check` passes without the `RUSTSEC-2025-0134` allow-list entry (currently kept only for bollard 0.18 -> rustls-pemfile).
- `ISENGARD_RUNTIME` env var either gone or accepts only `wisp` (with documented deprecation on any other value).
- A fresh `cargo build --workspace` produces a binary that has no transitive dependency on `bollard`, `hyperlocal`, `hyper 0.14`, or any other bollard-only crate.
- `cargo test --workspace` green.
- Wisp-mode e2e (`wisp_compose_e2e`, `wisp_labels_and_discovery`, `wisp_backend_smoke`) still green; bollard-mode tests deleted along with the bollard backend they exercised.

## Current state: full audit

Total bollard touch-points across the workspace (grouped by what removal would take). Counts are call sites, not lines; the LOC delta is mostly in `runtime/bollard_backend.rs` (991 lines) and the updater's recreate path (416 lines).

### Category A: dead code today (delete outright)

These are bollard-typed surfaces with no live callers. They exist only because the Phase 0.4 to 0.6 trait migration kept legacy entry points alongside the new ones.

| Site | Description |
|---|---|
| `crates/isengard-agent/src/compose_reconciler.rs::RunningService::from_inspect` | Builds a RunningService from `bollard::secret::ContainerInspectResponse`. The live diff path uses `from_snapshot` over `runtime::ContainerSnapshot`. No callers. |
| `crates/isengard-agent/src/container_snapshot.rs::list_container_snapshots()` | Legacy bollard-direct snapshotter. The agent's heartbeat uses `list_container_snapshots_via(backend)`. The bollard fallback is only called when backend selection fails at boot, which on a Phase 0.8 host means docker isn't installed and the fallback will fail anyway. |
| `crates/isengard-agent/src/secret_fetch.rs::MaterialisedSecret::as_bollard_bind` | Renders a host:container:ro string. Used only by its own unit test; live compose_apply now hands `SecretMount` structs to the backend. |
| `crates/isengard-agent/src/proxy/discovery.rs::pick_container_ip` | Pure picker over `bollard::secret::EndpointSettings`. Live path uses the snapshot-typed `pick_snapshot_ip`. Tests still exercise it. |

**Estimated delta:** ~150 LOC removed, no behavior change, no risk.

### Category B: live callers using `as_bollard()` escape hatch

These reach for `RuntimeBackend::as_bollard()` to get an `Arc<bollard::Docker>` and then drive docker directly. Each one needs a trait-driven replacement before bollard can go.

| Site | Native concept | Replacement strategy |
|---|---|---|
| `crates/isengard-agent/src/lib.rs:256` | Top-level plumbing: `let docker = backend.as_ref().and_then(\|b\| b.as_bollard());` | Stop plumbing the bollard handle once every downstream stops asking for one. |
| `crates/isengard-agent/src/deployment/mod.rs::DeploymentSupervisor::new` and `driver.rs::RealDriverDeps` | Entire blue-green driver: `inspect_container`, `create_image` (pull), `create_container`, `connect_network`, `start_container`, `stop_container`, `remove_container`. | Refactor `RealDriverDeps` to hold `Arc<dyn RuntimeBackend>`. The trait already covers every primitive; the only nuance is that green-container creation copies blue's bollard `Config<String>` verbatim today. After the refactor green is built by copying blue's `ContainerSnapshot` into a `ContainerCreateSpec`. The bollard `Config` -> wire-shape mapping in `BollardBackend::spec_to_config` already proves the round-trip is lossless for the fields the blue-green driver touches. |
| `crates/isengard-agent/src/compose_import.rs::spawn` and `run_once` | Sweeps `Docker::list_containers` + `inspect_container` to reverse-engineer a `compose.yaml` from running containers (the v0.3c migration path for dockerd-installed homelabs). | Two options. (1) Drop the feature: a fresh wisp install has nothing to import, and the migration use case dies with bollard. (2) Reimplement over `RuntimeBackend::list_containers` + `inspect_container`, then rewrite `compose_export.rs` to consume `ContainerSnapshot` instead of `ContainerInspectResponse`. Option (1) is the smaller diff and matches the docker-mode-stays-as-an-escape-hatch story: operators who still run docker can use the v0.4.x binary to import, then upgrade. **Recommend (1).** |
| `crates/isengard-agent/src/logs.rs::BollardLogSource` | Tails container logs via `bollard::Docker::logs`. | The `RuntimeBackend` trait already has `stream_logs`. Replace `BollardLogSource` with a `BackendLogSource` that wraps `Arc<dyn RuntimeBackend>`. WispBackend's `stream_logs` already exists from Phase 0.4. |

**Estimated delta:** deployment driver refactor is the load-bearing one. compose_import deletion saves ~500 LOC across compose_import + compose_export. logs swap is mechanical. Net: ~1000 LOC removed, ~400 LOC added.

### Category C: independent bollard surface (updater plugin)

The updater plugin (`crates/isengard-plugins/updater/`) constructs its own `bollard::Docker` and bypasses `RuntimeBackend` entirely. Phase 0.4 did not refactor it. Today it has two responsibilities:

1. **Cycle:** list `isengard.enable=true` containers, compare local digest vs registry, decide. The list + inspect calls are bollard-direct.
2. **Recreate:** in-place container recreate at the new digest (the `dispatcher_outcome::Pass` path).
3. **Self-update:** the updater recreates the agent container itself. **Already superseded** by `crates/isengard-agent/src/self_update.rs` (Phase 0.8, `20356d2`): the systemd path is binary-atomic-rename + `systemctl restart`. The plugin's `self_update.rs` is dead code on Phase 0.8+ installs.

| Site | Replacement strategy |
|---|---|
| `updater/src/lib.rs::cycle` (`docker.list_containers`) | Inject `Arc<dyn RuntimeBackend>` via `PluginContext`. Today the updater builds its own Docker handle; we'd add a `runtime: Arc<dyn RuntimeBackend>` field to `PluginContext` (or expose via existing accessor) and drop the bollard-direct construction. |
| `updater/src/recreate.rs::update_container` | Refactor to drive the trait: pull -> stop+remove blue -> create green -> connect networks -> start. Behavior matches the existing `compose_apply::start_service` flow. Note: the updater's recreate must preserve every label on the existing container; the trait's `inspect_container -> ContainerSnapshot` already carries labels, so the round-trip is symmetric. |
| `updater/src/dispatch_helpers.rs` | Walks `ContainerInspectResponse` / `ContainerSummary` to build an `UpdateTriggerInfo`. Refactor to consume `ContainerSnapshot`. |
| `updater/src/self_update.rs` | **Delete.** Phase 0.8 superseded it. |
| `updater/Cargo.toml::bollard` | Drop after the other items land. |
| Tests: `cycle_e2e`, `recreate_e2e`, `plugin_loads`, `dispatch_helpers` unit tests | Refactor to drive a wisp-backed `RuntimeBackend` (or a mock impl of the trait), same as the agent's existing wisp e2e suite. |

**Estimated delta:** ~600 LOC removed from updater, ~300 LOC added back. Net negative.

### Category D: agent tests and examples (mechanical)

| Site | Action |
|---|---|
| `crates/isengard-agent/tests/bollard_backend_regression.rs` | **Delete.** This test exists solely to prove BollardBackend lifecycles a busybox correctly. Removing bollard removes the thing under test. |
| `crates/isengard-agent/tests/proxy_label_discovery_e2e.rs` | Uses `BollardBackend::from_env` to drive the watcher against a live docker daemon. Two choices: (1) delete, redundant with `wisp_labels_and_discovery`; (2) rewrite to use `WispBackend::from_env` plus wisp test setup. Recommend (1): the wisp e2e already proves the watcher works end-to-end. |
| `crates/isengard-agent/tests/deployment_blue_green_{happy,rollback,aborts_on_healthcheck}.rs` | Each spins up bollard against the live daemon. Rewrite to use wisp once the deployment driver is on the trait. |
| `crates/isengard-agent/examples/compose_smoke.rs` | Uses `Docker::connect_with_local_defaults` + `build_compose`. Delete along with compose_import / compose_export. |

### Category E: workspace + deny config

| Site | Action |
|---|---|
| `Cargo.toml:123` (`bollard = { ... }` workspace dep) | Drop in the final commit. |
| `crates/isengard-agent/Cargo.toml::bollard` | Drop. |
| `crates/isengard-plugins/updater/Cargo.toml::bollard` | Drop. |
| `deny.toml:9` (RUSTSEC-2025-0134 rustls-pemfile ignore) | Drop. |
| Comments in `compose_apply.rs`, `labels.rs`, `runtime/mod.rs`, etc. that say "bollard" | Either delete the docstring (if the comment was just orienting the reader to history) or replace "bollard" with "docker" where the comment is documenting an external concept. Cosmetic; do not block the cargo-deny green-light. |

## Migration sequence

The order matters: each step must compile + pass tests at HEAD. The plan deliverable enumerates the commits.

1. **Delete dead code (Category A).** No behavior change. Smallest safe step; lands first to shrink the diff that follows.
2. **Move the deployment driver to the trait (Category B, the load-bearer).** This is the biggest single refactor. Done before anything below because the entire Category B + C cascade depends on the trait covering blue-green.
3. **Replace `BollardLogSource` with `BackendLogSource` driving the trait's `stream_logs`.** Pure refactor; logs are well-isolated.
4. **Delete compose_import + compose_export + the example.** The compose import migration path retires with bollard; documented in the release notes.
5. **Refactor the updater plugin to drive `RuntimeBackend`.** Cycle list + dispatch_helpers + recreate. Delete `updater/src/self_update.rs` (superseded by `crates/isengard-agent/src/self_update.rs`).
6. **Delete `BollardBackend`, `as_bollard()`, `bollard_backend_regression`, the bollard arm of `select.rs`.** Once nothing calls the escape hatch this is mechanical.
7. **Drop the workspace bollard dep + deny.toml entry.** Run `cargo deny check`; verify green.
8. **Release notes + final sweep:** scrub `bollard` mentions from docstrings where the word leaks user-visible terminology.

## Compatibility window

Three options for operators still running `ISENGARD_RUNTIME=docker`:

| Option | Description | Recommendation |
|---|---|---|
| Hard cutover | Phase 0.16 removes BollardBackend in one release. Operators who haven't switched off docker are blocked from upgrading until they `isd stack down` everything, migrate to wisp, redeploy. | NOT recommended. |
| Soft deprecation | Phase 0.16 splits across two releases. Release N: log a deprecation warning when `ISENGARD_RUNTIME=docker` is selected; release notes carry the migration script. Release N+1: BollardBackend deleted; `ISENGARD_RUNTIME=docker` becomes an `UnknownBackend` error. | **RECOMMEND.** |
| Wisp-only docker shim | A tiny adapter that translates `docker-cli`-shaped calls to wisp. | Not worth it. Operators with deep docker integration should stay on the docker-mode v0.4.x branch; Phase 0.16 is the green-field wisp-only line. |

**Recommended compatibility window:** soft deprecation across two releases.

- **Release N (v0.5.0):** ship the deprecation warning. BollardBackend still works. Release notes give the migration script (`isd stack down <stack>` for every stack, `ISENGARD_RUNTIME=wisp` in the env file, restart the agent, redeploy). No code deletion yet. Optional: a `--keep-bollard` feature flag on the agent crate so distros can ship a transitional build; default-off.
- **Release N+1 (v0.6.0):** the bollard arc removal lands. The agent refuses to start if `ISENGARD_RUNTIME=docker`. `cargo deny check` clean. Bollard gone.

This gives operators one release cycle (typically 4 to 6 weeks for Isengard) to migrate, with a clear deprecation warning every time the agent starts. Operators on long-lived docker installs can pin v0.5.x indefinitely; they just don't get new features after v0.6.

## Specific risks

These are the bollard surfaces that aren't trivially replaceable. Each one is a place where wisp may need an extension before bollard can go.

### R1: Blue-green driver green-container provenance

The blue-green driver clones blue's `bollard::Config<String>` verbatim and edits only the image tag. Today the round-trip is byte-identical to whatever docker created blue with. After the refactor green is built from a `ContainerSnapshot` round-tripped through `ContainerCreateSpec` -> `WispBackend::create_container`. The `BollardBackend::spec_to_config` mapper proves the round-trip is lossless for the fields the blue-green driver touches; but `Config<String>` carries fields the trait does not (e.g. `OpenStdin`, `Tty`, raw `HostConfig.PidMode`). The driver doesn't touch those today, but the verbatim-clone hides the gap. **Mitigation:** before the deployment refactor lands, audit `RealDriverDeps::start_green`'s `Config` field set against the trait's `ContainerCreateSpec`; add any missing fields to `ContainerCreateSpec` and to both backend impls. Same audit for `pull_and_recreate_at_digest` (rollback path).

### R2: Image pull authentication

`bollard::Docker::create_image` uses the daemon's stored registry credentials (typically from `~/.docker/config.json` mounted into the daemon). The wisp image-pull path (`wisp-image::pull`) has its own auth surface; verify it consumes the same `DockerConfig` shape the updater builds (`crates/isengard-plugins/updater/src/auth.rs`). **Mitigation:** before the updater refactor, audit `wisp-image::pull`'s auth handling and confirm it can pull from private registries with credentials the updater already loads. If gap exists, file as a wisp-image task before this phase implementation.

### R3: Healthcheck semantics

Bollard runs the docker daemon's healthcheck (Docker daemon-internal). Wisp's `run_healthcheck` runs externally via HTTP probe or nsenter. The blue-green driver's `wait_until_healthy` loop polls bollard's `inspect_container().state.health.status`. Wisp's `inspect_container` returns its own `HealthState`; verify the deployment driver's polling loop is generic over the trait shape and not the docker daemon enum. **Mitigation:** trait surface already exposes `HealthState` (Phase 0.4); confirm the driver consumes it, not the bollard enum.

### R4: Event stream gaps

Phase 0.5 documented behavior deltas in `labels.rs`: `container.update` and `container.destroy` events are not mapped to a `RuntimeEventType` variant. Under bollard the watcher saw these via the raw event stream; the trait doesn't surface them. Today this is a notification redundancy loss only (the labels watcher still sees `die`). Removing bollard cements the gap. **Mitigation:** acceptable as-is; deferred to a future trait extension if a real workload needs `update` event hooks.

### R5: Cargo.lock churn

Dropping bollard removes hyper 0.14, hyperlocal, rustls-pemfile, several others from the lockfile. Downstream crates that depend on those (today: none in our workspace; verify) would need adjustment. **Mitigation:** run `cargo tree -e features` before + after; verify no removed transitive becomes an unexpected dep.

## Test plan

**Key insight:** the existing `bollard_backend_regression` test suite proves the trait wiring is equivalent across the two backends today. Deleting it leaves the wisp tests as the only proof. That is acceptable because (a) the wisp tests cover the same surface area and (b) the bollard backend is the thing being removed, so an equivalence test is moot.

| Coverage | Today | After Phase 0.16 |
|---|---|---|
| Backend lifecycle (create / start / stop / remove + list / inspect) | `bollard_backend_regression` + `wisp_backend_smoke` | `wisp_backend_smoke` |
| Compose reconcile e2e | `wisp_compose_e2e` | `wisp_compose_e2e` |
| Labels watcher + discovery | `wisp_labels_and_discovery` + `proxy_label_discovery_e2e` (bollard variant) | `wisp_labels_and_discovery` |
| Blue-green happy path | `deployment_blue_green_happy` (bollard against live daemon) | rewrite for wisp |
| Blue-green abort on healthcheck failure | `deployment_blue_green_aborts_on_healthcheck` | rewrite for wisp |
| Blue-green rollback | `deployment_blue_green_rollback` | rewrite for wisp |
| Updater plugin cycle | `cycle_e2e` (bollard) | rewrite for wisp |
| Updater plugin in-place recreate | `recreate_e2e` (bollard) | rewrite for wisp |
| Updater plugin self-update | `plugin_loads` + indirect | dead (superseded by `isengard self-update`); delete |

Verification gates per step (matches existing CI):
- `cargo fmt --check`
- `cargo clippy --workspace -D warnings`
- `cargo test --workspace` (Mac side, no docker daemon required after the refactor lands)
- `cargo deny check` (must pass without the RUSTSEC-2025-0134 entry after the final commit)
- `cargo build --workspace --release` produces a binary; manual `nm` or `cargo bloat` to verify no transitive bollard symbols leak in.
- End-to-end smoke in the existing wisp OrbStack VM: `isd deploy` a servarr-style stack, `isd stack ps`, `isd logs`, `isd stack down`. No bollard, no docker daemon.

## Explicit non-goals

- **Docker docs are NOT removed wholesale.** `docs/RELEASES.md` and `docs/RELEASE_NOTES_*.md` keep their docker-mode references for historical accuracy. `install/compose.yaml` and `install/install-docker.sh` (Phase 0.8's renamed legacy installer) stay in-tree as documented escape hatches for operators who want isengard-in-a-box. The CLAUDE.md / README.md prose may be tightened but docker stays mentioned as a "legacy / advanced" install path.
- **`ISENGARD_RUNTIME` env var stays in the API surface.** Phase 0.16 narrows the accepted values; the var itself remains so future runtimes (containerd-direct, podman, etc.) can slot in.
- **No changes to the proto surface.** All controller -> agent / agent -> controller wire messages are runtime-agnostic.
- **No changes to the dashboard.** The dashboard already only knows containers via `StackInfo` / `ServiceInfo`, neither of which carry a backend tag (though Phase 0.3+ status doc references a "Dashboard backend column (wisp/docker per host)" wisp polish item; that lives separately).
- **No wisp feature work in Phase 0.16.** If R1 (driver field gap) or R2 (image-pull auth) needs wisp extensions, those land in their own phase + PR, before this one.

## Risks of NOT doing this

| Risk | Impact |
|---|---|
| Dead code drift | `BollardBackend` (991 LOC) bit-rots silently. Every wisp-side change has to re-prove it doesn't break the bollard path even though no current operator runs it on Phase 0.8 hosts. |
| cargo-deny exceptions accumulate | The RUSTSEC-2025-0134 ignore is the canary. As bollard's transitive deps age, more advisories will fire; each one is a maintenance toll for a backend nobody uses. |
| Dependency surface bloat | bollard pulls hyper 0.14 + hyperlocal + rustls-pemfile. Removing it shrinks the supply-chain surface by ~30 crates (verify with `cargo tree`). |
| Mental tax | New contributors see two backends, two test suites, two doc paths. The wisp story is half-told as long as `RuntimeBackend::as_bollard()` exists on the public trait. |
| Release-binary size | bollard contributes ~600 KB to the static musl binary (estimate; verify before/after). Phase 0.7 binaries are ~6.9 MB; trimming 10% off the wire matters for the convenience-script install on slow links. |

## Rough effort estimate

| Step | PR count | LOC delta |
|---|---|---|
| Category A dead-code purge | 1 | -150, +0 |
| Deployment driver refactor onto trait | 1 | -800, +400 |
| Logs source onto trait | 1 | -100, +60 |
| compose_import + compose_export + example deletion | 1 | -1200, +0 |
| Updater plugin refactor | 1 to 2 | -600, +300 |
| Delete BollardBackend + as_bollard + bollard_backend_regression + select.rs Docker arm | 1 | -1100, +20 |
| Drop workspace bollard dep + deny.toml + final scrub | 1 | -30, +0 |
| **Total** | **7 to 8 PRs** | **~-3000, +800 (net ~-2200 LOC)** |

The bulk of the removed LOC is the bollard backend (991), the compose import/export pair (~980 combined), and the updater's bollard-direct paths (~600). The additions are mostly the trait-driven rewrites of the deployment driver and the updater cycle.

## Default-and-document calls (operator review)

These are calls the spec makes that the operator may want to flip during review:

1. **Compose-import retires with bollard, not earlier.** Alternative: keep compose-import behind a `RuntimeBackend::list_containers + inspect_container` rewrite + a new `ContainerSnapshot -> compose.yaml` mapper. Cost: ~600 LOC kept + a path nobody on a wisp-only install will ever exercise. Default: drop.
2. **`updater/src/self_update.rs` deleted (not refactored).** Phase 0.8's `crates/isengard-agent/src/self_update.rs` is the live path. Alternative: keep both side by side; let operators pick via plugin config. Cost: maintaining two self-update paths forever. Default: drop the plugin path.
3. **`proxy_label_discovery_e2e.rs` deleted (not rewritten for wisp).** `wisp_labels_and_discovery` covers the same surface. Alternative: keep one bollard-variant test as a smoke check during the migration window. Cost: a single test referencing bollard during the deprecation window. Default: delete in lockstep with BollardBackend; the deprecation window has its own coverage via the still-shipping v0.5.x branch.
4. **Two-release deprecation, not three.** Alternative: three releases (warn, error, delete). Cost: slower removal, longer dual-path maintenance. Default: two releases is enough given the brainstorm-locked "docker mode = bollard mode" framing.
5. **No feature flag for the transitional v0.5 build.** A `--keep-bollard` Cargo feature on the agent crate was floated above as optional; default is to NOT ship it. Operators who need docker should pin v0.5.x. Adding a feature flag couples bollard removal to a build-matrix burden.
