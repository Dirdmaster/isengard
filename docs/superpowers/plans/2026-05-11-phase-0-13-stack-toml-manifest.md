# Phase 0.13 stack.toml Manifest + `isd deploy --all`: Implementation Plan

> Spec: [`2026-05-11-phase-0-13-stack-toml-manifest-design.md`](../specs/2026-05-11-phase-0-13-stack-toml-manifest-design.md). Branch: `phase/0-13-stack-toml`.

## Scope

Land the `stack.toml` orchestration manifest. New crate `crates/isengard-manifest/` parses and validates it. The isd CLI grows `isd deploy --all`, manifest-aware no-args `isd deploy`, `--overlay`, `--strategy`, `--diff`, `--fail-fast`. The dashboard's `POST /api/v1/stacks` and `PUT /api/v1/stacks/<id>/compose` endpoints gain a JSON content-type that carries the manifest body, secret name list, and hook list. Three new storage migrations (one combined file): `manifest_toml`/`manifest_sha256`/`manifest_imported_at`/`deploy_strategy`/`manifest_fleet` columns on `stacks`, plus `stack_secrets` and `stack_hooks` tables. The agent proto gets four new optional fields on `WriteCompose` (`manifest_toml`, `secrets`, `hooks`, `deployment_id`); the agent persists the manifest at `/etc/isengard/stacks/<name>/stack.toml` and executes hooks on the host.

Out of scope: `isd manifest cat / edit / export` (deferred), global-scope secrets (phase 0.15), rollback UI changes, dashboard frontend for manifest viewing, parallel `--all`, hook script syncing, manifest-driven fleet routing (the field is captured but not consumed).

## Dev environment

Same as the wisp phase plans: edit in `/Users/dirdmaster/Projects/isengard/.worktrees/next/`. Workspace + dashboard tests run on Mac (`cargo test -p isd -p isengard-manifest -p isengard-plugin-dashboard`). Agent integration tests run on the OrbStack `wisp` VM (`orb -m wisp bash -lc "cd $WISP && cargo test -p isengard-agent --test phase_0_13_manifest"`). End-to-end test via the existing `iso-test` VM (Ubuntu 24.04, kernel 6.19, cgroup v2, arm64, systemd 255, no docker).

## Files touched

| File | Change |
| --- | --- |
| `Cargo.toml` (workspace) | add `crates/isengard-manifest` to members |
| `crates/isengard-manifest/Cargo.toml` | new; deps: `serde`, `toml`, `thiserror`, `tracing` |
| `crates/isengard-manifest/src/lib.rs` | new; `StackManifest`, `FleetManifest`, `Strategy`, `HookSpec`, `HookEvent`, `HookErrorPolicy` |
| `crates/isengard-manifest/src/parse.rs` | new; TOML deserialization + validation |
| `crates/isengard-manifest/src/overlay.rs` | new; compose merge with list-append / scalar-overwrite rules |
| `crates/isengard-manifest/tests/parse_tests.rs` | new; manifest parsing test suite |
| `crates/isengard-manifest/tests/overlay_tests.rs` | new; merge semantics test suite |
| `crates/isengard-storage/migrations/0027_stack_manifest.sql` | new; columns + `stack_secrets` + `stack_hooks` |
| `crates/isengard-storage/src/inventory.rs` | new helpers: `update_stack_manifest`, `set_stack_secrets`, `set_stack_hooks`, `get_stack_manifest_bundle` |
| `crates/isengard-storage/src/stack.rs` | extend `Stack` + `InsertStack` with manifest fields |
| `crates/isengard-proto/proto/isengard.v1.proto` | add `manifest_toml`, `secrets`, `hooks`, `deployment_id`, `LifecycleHook` to `WriteCompose` |
| `crates/isengard-plugins/dashboard/src/api.rs` | extend `create_stack`, `put_stack_compose`; add JSON content-type branch; extend `get_stack` response |
| `crates/isengard-plugins/dashboard/tests/manifest_api.rs` | new; POST + PUT + GET round-trip |
| `crates/isd/src/compose_cmd.rs` | refactor `run_deploy` to fork on `DeployPlan`; add `run_manifest_deploy` and `run_all_deploy` |
| `crates/isd/src/main.rs` | extend `DeployArgs` with `--all`, `--root`, `--overlay`, `--strategy`, `--fail-fast` |
| `crates/isd/tests/cli_deploy.rs` | new; `resolve_deploy_plan` branch coverage |
| `crates/isengard-agent/src/compose_apply.rs` | accept the new `WriteCompose` fields; persist `stack.toml`; thread secrets list into the existing secret-mount path |
| `crates/isengard-agent/src/lifecycle_hooks.rs` | new; spawn hooks with the spec'd env / cwd / timeout |
| `crates/isengard-agent/tests/lifecycle_hooks.rs` | new; hook execution + timeout + on_error coverage |
| `crates/isengard-agent/tests/phase_0_13_manifest.rs` | new; end-to-end manifest persistence + secret + hook flow (VM-gated) |
| `docs/RELEASE_NOTES_PHASE_0_13.md` | new; user-facing release notes |

## Steps

Each step ends in a commit. Branch is `phase/0-13-stack-toml`. Subagent dispatch: Opus implementer per step (per `feedback_implementer_opus`). Sonnet code-reviewer at the marked checkpoints.

### 1. Manifest crate skeleton + TOML parsing

Land `crates/isengard-manifest/` with `StackManifest::load` + `FleetManifest::load_from_walk`. No overlays yet; just the v1 manifest schema. Strategy / HookEvent / HookErrorPolicy enums with `serde(rename_all = "kebab-case")`. Use `toml = "0.8"`; align version with the workspace's existing pin (`crates/isd/Cargo.toml`).

Tests (`crates/isengard-manifest/tests/parse_tests.rs`):
- minimal manifest (name only) parses
- full manifest (name + fleet + compose + strategy + secrets + hooks) parses
- missing `name` errors with `MissingField("name")`
- unknown `strategy` value errors
- unknown `hooks[].on` errors
- `hooks[].timeout` parses both "120s" and "60000ms" via humantime
- `FleetManifest::load_from_walk` finds isengard.toml in parent dir; stops at git root marker (or filesystem root); returns None when absent
- absolute paths in `compose = [...]` rejected; only relative paths allowed

Commit: `feat(manifest): stack.toml + isengard.toml parsing`

### 2. Overlay merge logic

`crates/isengard-manifest/src/overlay.rs`. Implements `StackManifest::resolved_compose_paths(overlay: Option<&str>) -> Result<Vec<PathBuf>>` plus a `merge_compose_yaml(base: &str, overlays: &[String]) -> Result<String>` helper that produces a single merged YAML string the controller-side path consumes.

Merge rules (from the spec §"Parsing + merging"):
- Parse each file into `serde_yaml::Value`
- Walk `services` map: new keys insert; existing keys recurse
- Scalar fields: last-write-wins
- List fields by name (`volumes`, `environment`, `ports`, `depends_on`, `cap_add`, `cap_drop`, `networks`, `expose`, `labels`): append + dedupe
- Dedupe key for "k:v" strings: first colon-separated component (`SONARR_API_KEY=foo` -> `SONARR_API_KEY`; `./data:/data` -> `./data`)
- Nested maps (`deploy`, `healthcheck`, `logging`): recurse with same rules
- Unknown field: shallow last-write-wins (don't fight the compose surface)

Tests (`crates/isengard-manifest/tests/overlay_tests.rs`):
- Two files differing only in image: image overwritten
- Two files: base has env `[A=1, B=2]`, overlay `[A=9, C=3]`: result has `[A=9, B=2, C=3]`
- Volumes append-dedupe by source path
- New service in overlay inserts; existing service merges
- Base + named overlay flow: `resolved_compose_paths("staging")` returns base + the `overlays.staging.compose` list
- Unknown overlay name errors

Commit: `feat(manifest): overlay merging for compose files`

**Checkpoint 1 (review):** end of step 2, before any storage / API touches. Reviewer (Sonnet) confirms TOML schema, naming, overlay rules. Operator hands-on optional.

### 3. Storage migration + inventory helpers

`crates/isengard-storage/migrations/0027_stack_manifest.sql` exactly as spec §"Storage schema". Drop in. Apply order is automatic (next-number lexical).

`crates/isengard-storage/src/inventory.rs`:
- `update_stack_manifest(stack_id, manifest_toml, manifest_sha256, deploy_strategy, manifest_fleet) -> Result<()>`: UPDATE with the four new columns
- `set_stack_secrets(stack_id, secret_names: &[&str]) -> Result<()>`: DELETE-then-INSERT in a transaction; verifies every name exists in `secrets` (returns missing-name list in the error)
- `set_stack_hooks(stack_id, hooks: &[StoredHook]) -> Result<()>`: DELETE-then-INSERT in a transaction; preserves manifest order via `ordinal`
- `get_stack_manifest_bundle(stack_id) -> Result<ManifestBundle>`: JOIN across stacks + stack_secrets + stack_hooks; returns a single struct ready for the dashboard

`crates/isengard-storage/src/stack.rs`: extend `Stack` with `manifest_toml`, `manifest_sha256`, `manifest_imported_at`, `deploy_strategy`, `manifest_fleet`. `InsertStack` stays unchanged (manifest writes happen separately).

Tests (existing `crates/isengard-storage/src/inventory.rs::tests`):
- `update_stack_manifest` round-trips
- `set_stack_secrets` rejects unknown name with the missing-name list
- `set_stack_secrets` is idempotent (rerunning with same list = no rows added)
- `set_stack_hooks` preserves order
- `get_stack_manifest_bundle` returns NULL columns when manifest absent

Commit: `feat(storage): manifest_toml + stack_secrets + stack_hooks (migration 0027)`

**Checkpoint 2 (review):** end of step 3, schema is permanent once merged. Reviewer (Sonnet) checks column types, constraints, FK behavior, index choices.

### 4. Proto extension

`crates/isengard-proto/proto/isengard.v1.proto`: add the four new fields to `WriteCompose` (field numbers 6-9) and the new `LifecycleHook` message. Regenerate via the existing build.rs invocation (`cargo build -p isengard-proto`).

Update `crates/isengard-proto/src/lib.rs` if any explicit re-exports are needed (probably not: the proto crate exports the whole module).

Tests:
- Build succeeds (`cargo build -p isengard-proto`)
- An existing serializing-test (in `crates/isengard-proto/tests/` if present) round-trips a `WriteCompose` with the new fields populated and with them omitted; both decode

Commit: `feat(proto): WriteCompose manifest + secrets + hooks + deployment_id`

### 5. Agent: manifest persistence + hook execution

`crates/isengard-agent/src/compose_apply.rs`: extend the `WriteCompose` handler to:
- Persist `manifest_toml` (if non-empty) at `/etc/isengard/stacks/<name>/stack.toml` (atomic write via tempfile).
- Thread the `secrets` list into the existing v0.3.6 secret-mount path (existing `crates/isengard-agent/src/secret_mounts.rs`: extend it to accept stack-scoped lists in addition to compose-level `secrets:` blocks).
- Run pre-deploy hooks BEFORE the compose reconciliation step.
- Run post-deploy hooks AFTER reconciliation reports all services `Running`.
- Run failure hooks if any step errors.

New module `crates/isengard-agent/src/lifecycle_hooks.rs`:

```rust
pub struct HookEnv {
    pub stack: String,
    pub host_id: String,
    pub deployment_id: String,
}

pub async fn run_hooks(
    hooks: &[LifecycleHookProto],
    event: HookEvent,
    stack_dir: &Path,
    env: &HookEnv,
    failure_detail: Option<&str>,
) -> HookOutcome;

pub enum HookOutcome { AllOk, Aborted { hook_idx: usize, reason: String } }
```

Spawn via `tokio::process::Command`. cwd = `stack_dir`. env layered per spec §"Hook execution semantics". Timeout via `tokio::time::timeout`; on expiry, SIGTERM then SIGKILL after 5s. Write `events` row per hook run (controller-side; agent reports via the existing event stream).

Tests:
- `crates/isengard-agent/tests/lifecycle_hooks.rs`:
  - hook with `cmd = ["true"]` succeeds
  - hook with `cmd = ["false"]` and `on_error = "abort"` aborts; `HookOutcome::Aborted` with reason
  - hook with `cmd = ["false"]` and `on_error = "continue"` passes; warning logged
  - hook with `cmd = ["sleep", "10"]`, timeout 500ms: SIGTERM observed
  - env vars set correctly (test via a hook that writes `env > $TMP/dump.txt`)
- `crates/isengard-agent/tests/phase_0_13_manifest.rs` (VM-gated, `#[cfg(target_os = "linux")]`):
  - simulate WriteCompose with manifest + 2 secrets + pre + post hooks
  - assert `/etc/isengard/stacks/<name>/stack.toml` written
  - assert pre-deploy ran before compose apply
  - assert secrets mounted under `/run/secrets/<name>` in container

Commit: `feat(agent): manifest persistence + lifecycle hook execution`

**Checkpoint 3 (review):** end of step 5. Hooks-as-root on the host is a security surface. Reviewer (Sonnet) checks process spawn options, signal handling, env-var leakage, timeout correctness.

### 6. Dashboard API: extended POST + PUT + GET

`crates/isengard-plugins/dashboard/src/api.rs`:

- `create_stack` (POST /api/v1/stacks): accept the new optional fields. After `insert_stack`, before `WriteCompose`:
  - if `manifest_toml` present: parse via `isengard_manifest::StackManifest::from_str`; assert manifest.name == body.name (400 mismatch); call `inventory.update_stack_manifest`
  - if `secrets` present: call `inventory.set_stack_secrets`; on missing-name, return 422 with the list
  - if `hooks` present: validate via the manifest crate's HookSpec validator; call `inventory.set_stack_hooks`
  - thread `manifest_toml`, `secrets`, `hooks`, `deployment_id` into the `WriteCompose` protobuf
- `put_stack_compose` (PUT /api/v1/stacks/<id>/compose): add a JSON content-type branch. JSON body shape mirrors `create_stack`'s body except `host_id` is omitted (host already bound). When the request comes in as `application/yaml`, behavior is unchanged (compose-only update; manifest preserved). When `application/json`, manifest fields update in the same transaction.
- `get_stack` (GET /api/v1/stacks/<id>): include `manifest_toml`, `manifest_sha256`, `manifest_imported_at`, `deploy_strategy`, `manifest_fleet`, `secrets`, `hooks` in the response (nullable / empty when absent).

Validation:
- `secrets` field with unknown names -> 422 `{ error: "unknown secrets", missing: ["FOO", "BAR"] }`
- `manifest_toml` with name mismatch -> 400 `{ error: "manifest name 'X' != body name 'Y'" }`
- `hooks[].on` outside enum -> 400

Tests (`crates/isengard-plugins/dashboard/tests/manifest_api.rs`):
- POST with full manifest body -> 201, `get_stack` shows all fields
- POST with `secrets` referencing missing name -> 422 with `missing`
- POST with `manifest_toml` whose name mismatches body.name -> 400
- PUT with JSON content-type, manifest changes only (compose identical) -> 200, manifest updated
- PUT with YAML content-type -> 200, compose updated, manifest preserved unchanged
- GET after a deploy: manifest fields populated; for a legacy compose-only stack: manifest fields null

Commit: `feat(dashboard): POST/PUT stacks accepts manifest + secrets + hooks`

**Checkpoint 4 (review):** end of step 6. The wire surface is now expanded. Reviewer (Sonnet) confirms status code semantics, JSON shape, content-type negotiation, backward-compat.

### 7. isd CLI: deploy plan resolver + manifest deploy path

`crates/isd/src/main.rs`: extend `DeployArgs` with the new flags (`--all`, `--root`, `--overlay`, `--strategy`, `--fail-fast`). Update the long help text.

`crates/isd/src/compose_cmd.rs`: introduce `DeployPlan` and `resolve_deploy_plan`. Refactor `run_deploy` to dispatch:

- `DeployPlan::Single { compose_path, .. }`: existing behavior verbatim; backward-compat.
- `DeployPlan::Manifest { manifest_path, overlay, strategy_override, .. }`: new path. Load `StackManifest`, resolve compose paths, merge YAML, build the JSON body (manifest_toml, compose_yaml, secrets, hooks), POST or PUT.
- `DeployPlan::All { root, .. }`: new path. Walk subdirs, build a list of `DeployPlan::Manifest`, run each in lexical order, collect results, print summary.

`resolve_deploy_plan` precedence (matches spec §"isd CLI surface"):
1. `--all` set: `All { root: args.root.unwrap_or_else(cwd) }`
2. `args.path` is a file with `.yml` / `.yaml` / `.toml` suffix and exists as file: `Single`
3. `args.path` is a directory containing `stack.toml`: `Manifest`
4. `args.path` is None and `./stack.toml` exists: `Manifest { manifest_path: cwd/stack.toml }`
5. `args.path` is None and no `./stack.toml`: error 2 (manifest parse) with: "no stack.toml in cwd"

Tests (`crates/isd/tests/cli_deploy.rs`):
- Each `resolve_deploy_plan` branch with a tempdir fixture
- Plan resolver respects `--root`
- `--strategy` overrides manifest's strategy in the resolved plan
- `--overlay` selects the named overlay

Commit: `feat(isd): manifest-aware deploy + deploy --all`

### 8. isd CLI: `--all` summary output + exit codes

Polish the `run_all_deploy` output and exit-code behavior to match spec §"isd CLI surface":
- Per-stack status glyph + name + short summary (`deployed`, `no-change`, `failed`, `skipped`)
- After-the-run totals line
- Per-failure error block at the end
- Exit codes: 0 all-clean, 1 some-failed, 2 manifest parse, 3 controller unreachable
- `--diff` mode: print the plan for each stack without writing; exit 0 even if there are pending changes

Tests:
- Smoke test via a mock HTTP server (`mockito` or `wiremock`) that simulates the dashboard endpoints; assert exit codes and stdout content for each branch
- `--fail-fast` aborts on first failure; without it, continues

Commit: `feat(isd): --all summary output + exit code matrix`

### 9. Backfill helpers + manifest-from-compose

`crates/isengard-manifest/src/lib.rs`:

```rust
impl StackManifest {
    /// Produce a minimal manifest for a legacy compose-only stack.
    /// Used by operators to `isd deploy --all` against a repo they're
    /// migrating: drop a stack.toml with `name = "..."` and they're done.
    pub fn minimal_for(name: &str) -> Self;

    /// Serialize back to TOML for `isd manifest export` (deferred command;
    /// helper lives here so the round-trip is testable now).
    pub fn to_toml_string(&self) -> Result<String, ManifestError>;
}
```

Tests:
- `minimal_for("foo").to_toml_string()` round-trips back to `StackManifest::from_str`
- `to_toml_string` preserves hook order

Commit: `feat(manifest): minimal manifest helper + TOML round-trip`

### 10. End-to-end integration on iso-test VM

Fixture repo under `tests/fixtures/phase_0_13/` (committed):

```
phase_0_13/
├── isengard.toml          # fleet = "iso-test"
├── alpha/
│   ├── stack.toml         # name = "alpha", recreate, 1 hook
│   └── compose.toml       # one nginx service
├── beta/
│   ├── stack.toml         # name = "beta", auto, no hooks
│   ├── compose.toml
│   └── compose.staging.toml
└── gamma/
    ├── stack.toml         # name = "gamma", blue-green, pre-deploy hook
    └── compose.toml
```

Test script (Justfile recipe `just e2e-phase-0-13`):

```bash
cd tests/fixtures/phase_0_13
isd deploy --all
isd ps                                  # expect alpha, beta, gamma all running
isd deploy --all --diff                 # expect 3x no-change
# touch alpha/compose.toml's image tag
isd deploy --all                        # expect 1 deployed, 2 no-change
isd deploy --all --overlay staging      # expect beta redeploys with the overlay
```

Run from iso-test VM. The test is shell-driven (not cargo-test-driven) because the iso-test workflow already has SSH+ssh-tunnel scaffolding; treating this as a Justfile recipe keeps it out of the cargo-test critical path. Manual confirmation that the report matches expectations is the gate.

Commit: `test(phase_0_13): e2e fixture + Justfile recipe for iso-test`

### 11. Release notes

`docs/RELEASE_NOTES_PHASE_0_13.md`. Match the shape of `docs/RELEASE_NOTES_WISP_PHASE_0_8.md`. Sections:

- What's new (stack.toml, deploy --all, hooks, manifest-bound secrets)
- Operator quickstart (5-line walkthrough creating a manifest from scratch)
- Migrating existing stacks (drop a `stack.toml` with `name` only, redeploy)
- Wire surface changes (proto field additions, dashboard JSON content-type)
- Known limitations (global secrets in 0.15, hook script sync deferred, sequential --all only)
- Compatibility (agents older than 0.13 ignore the new fields; controllers older than 0.13 send empty fields)

Commit: `docs(phase_0_13): release notes`

### 12. PR

Open `phase/0-13-stack-toml` -> `next`. Title: `feat(phase_0_13): stack.toml manifest + isd deploy --all`. Body: spec link + scope + non-goals + screenshots of `isd deploy --all` summary output + the manifest fixture + the new dashboard JSON contract. Mark draft; flip ready for review after CI green.

## Validation

Per-step `cargo test -p <crate>` runs locally. Full gate before PR:

- `cargo build --workspace --release` clean
- `cargo test --workspace` green (Mac); `orb -m wisp bash -lc "cd $WISP && cargo test --workspace"` green (VM)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo fmt --check` clean
- `cargo deny check` clean (no new licenses)
- `just e2e-phase-0-13` against iso-test produces the expected report
- `isd deploy --all` on a manually-crafted three-stack fixture deploys, redeploys (no-change), and overlays cleanly

## Risks (and mitigations baked into the plan)

- **Hook execution security.** Step 5 hooks as root. Mitigation: Sonnet review at checkpoint 3; document in release notes that hooks run as the agent user (today: root) and operators should audit their `cmd` values. A dedicated `isengard-hooks` user is a phase 0.16 follow-up.
- **Migration breakage on existing stacks.** Five new columns on `stacks` + two new tables. SQLite ALTER TABLE in a transaction. All new columns nullable. Existing stacks: manifest_toml = NULL, behavior unchanged. Tested in step 3.
- **Proto wire compat.** New fields are protobuf-optional (proto3 default). Old agents ignore them. Old controllers don't send them. Tested by step 4's round-trip.
- **`--all` ordering surprises.** Lexical. Documented in release notes. If operators want topological, they prefix with `00-` / `10-`.
- **Manifest-vs-compose drift.** Dual-hash semantics (manifest_sha256 + compose_sha256) on PUT. Conflict on either fails. Documented in the API doc step.
- **`isengard.toml` upward walk.** Stop at `.git` dir or filesystem root. Tested in step 1.

## Subagent dispatch

Per `feedback_implementer_opus`, implementers run on Opus. Reviewers on Sonnet at the four explicit checkpoints. Sequence:

1. Steps 1 + 2 in one Opus dispatch (the manifest crate is small + self-contained). Checkpoint 1 review at the end.
2. Step 3 standalone (storage migration is permanent once merged). Checkpoint 2 review.
3. Step 4 standalone (proto bump is a small, isolated change).
4. Step 5 standalone (agent + hooks; security-sensitive). Checkpoint 3 review.
5. Step 6 standalone (dashboard API). Checkpoint 4 review.
6. Steps 7 + 8 in one Opus dispatch (CLI surface; the two steps share machinery).
7. Step 9 standalone (small helper additions).
8. Steps 10 + 11 + 12 in one Opus dispatch (fixtures + docs + PR).

Reviews at the end of: step 2, step 3, step 5, step 6, before step 12.

## Open questions during implementation

- **Hook env leakage.** Should the agent's own env (which may contain `ISENGARD_SECRETS_PASSPHRASE`) be inherited by hooks, or do we whitelist? Spec says "starts from the agent's own environment then layered": that's permissive. Reviewer call at checkpoint 3; lean toward whitelist-by-default with an opt-in `inherit_env = true` per hook.
- **`PUT /stacks/<id>/manifest`.** Spec reserves it but step 6 doesn't implement it ("deferred"). Skip in 0.13 or land for completeness? Recommendation: skip; one fewer surface to maintain until the `isd manifest edit` command lands.
- **`deployment_id` source.** Today's `WriteCompose` doesn't carry one. We generate a fresh ULID on the controller per-request and thread it through. Confirm in step 4 that the existing deployment-tracking code (phase 10g rolling state machine) is happy with the new ID source or needs adjustment.
- **JSON content-type for `PUT /stacks/<id>/compose`.** If the operator's curl-using scripts default to text/plain or application/yaml, they keep working. Document in the release notes; verify a curl-from-shell example works in the e2e step.
