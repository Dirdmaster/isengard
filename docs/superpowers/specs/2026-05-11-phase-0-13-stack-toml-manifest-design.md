# Phase 0.13: `stack.toml` manifest + `isd deploy --all` (design)

Status: proposed
Phase: v0.4 foundation, phase 0.13
Author: 2026-05-11
Depends on: phase 0.12 (installer rebuild on cliclack, shipped), `POST /api/v1/stacks` (already in dashboard at `crates/isengard-plugins/dashboard/src/api.rs:716`)

## Problem

Today, `isd deploy <path>` ships a single compose file. The CLI infers the stack name from the parent directory, accepts `--stack` / `--host-id` flags, and round-trips through `POST /api/v1/stacks` (first time) or `PUT /api/v1/stacks/<id>/compose` (subsequent). That works for one stack at a time, but every interesting homelab layout has 5-20 stacks (servarr, monitoring, homepage, paperless, immich, ...). The operator wants:

```
my-homelab/
  servarr/{stack.toml, compose.toml}
  monitoring/{stack.toml, compose.toml, compose.staging.toml}
  homepage/{stack.toml, compose.toml}
  isengard.toml
```

```
cd my-homelab && isd deploy --all   # ships every stack
cd servarr   && isd deploy           # ships just this one (reads ./stack.toml)
```

Compose stays purely about services. The orchestration metadata (stack name, target fleet, secret bindings, deploy strategy, lifecycle hooks) lives one level up in `stack.toml`. `isd deploy` with no args looks for `./stack.toml` in cwd; `isd deploy --all` walks immediate subdirectories.

## Goal

Land `stack.toml` as the orchestration manifest. `isd deploy` resolves it in cwd, applies overlays, computes the host hint, threads the secrets list + hooks + strategy through to the controller, and writes the stack. `isd deploy --all` walks subdirs and ships every stack in repo order, with sane failure semantics. Two new storage tables (`stack_secrets`, `stack_hooks`) and one new column (`stacks.manifest_toml`) carry the manifest server-side. The dashboard API gains a manifest body slot on `POST /api/v1/stacks` (and a sibling `PUT /api/v1/stacks/<id>/manifest`) so subsequent edits round-trip.

Done bar:

```
$ cd ~/homelabs/main && isd deploy --all
Walking ./ for stack.toml files...
  servarr        (compose.toml, blue-green, 3 secrets)  ✓ deployed (sha 7f3a..)
  monitoring     (compose.toml + compose.prod.toml, rolling, 2 secrets) ✓ deployed
  homepage       (compose.toml, recreate, 0 secrets)    ✓ deployed
Deployed 3 stacks in 12.4s.
```

A second run is idempotent: same `stack.toml` + compose body = "no change" plans across the board.

## Non-goals (Phase 0.13)

- Global secrets sync-on-put. Phase 0.15 ships `--scope global`; 0.13 only carries the per-fleet secret name list.
- Rollback UI. `isd rollback <stack>` belongs to the existing deployment-history surface; 0.13 doesn't touch it.
- Scheduled deploys / cron triggers. Out.
- Webhook / git-tag-triggered deploys. Out.
- Cross-fleet deploy from a single repo (`isd deploy --all --fleet prod`). The manifest's `fleet` field is captured + persisted, but resolution against the operator's context list is deferred to phase 0.14+ alongside placement.
- Stack delete via `isd deploy --all` (e.g. dropping a directory removes the stack on the controller). Explicitly NOT a sync; missing dirs are ignored. Operator runs `isd stack rm` for deletes.
- Templating / variable substitution in `stack.toml` or compose. Out. The manifest is plain TOML; if operators want templating, that's `cue` / `dhall` / `jsonnet` territory and a 0.20+ conversation.
- Dashboard UI for editing manifests. Terminal-first per the brainstorm; dashboard HELD.

## Design

### File layout

```
my-homelab/
├── isengard.toml                 # optional fleet config (default fleet name, default context)
├── servarr/
│   ├── stack.toml                # required: orchestration manifest
│   └── compose.toml              # required: service definitions (compose.yml also accepted)
├── monitoring/
│   ├── stack.toml
│   ├── compose.toml              # base
│   └── compose.staging.toml      # overlay (selected via `--overlay staging` or manifest)
└── homepage/
    ├── stack.toml
    └── compose.toml
```

Walk rule for `isd deploy --all`: scan immediate subdirectories of cwd; a subdir is a stack if and only if it contains `stack.toml`. Nested layouts (`apps/servarr/stack.toml`) are NOT recursively discovered; operators run `isd deploy --all` from each `apps/` parent or pass `--root <path>`. Keeps the rule one level deep and unsurprising.

### `stack.toml` schema

```toml
# stack.toml v1

# Stack identity. Required.
name = "servarr"

# Fleet binding. Optional. When omitted, the stack is deployed against the
# operator's current `isd context`. Resolution against the saved context list
# is fleet-name -> context match; ambiguity errors out.
fleet = "homelab"

# Compose file list. Required, non-empty. Earlier entries are the base;
# later entries overlay (merged services-wise, last-write-wins on scalar
# fields, list-append on `volumes` + `environment` + `ports`). Paths are
# relative to stack.toml's directory.
compose = ["compose.toml"]

# Overlay groups. Optional. Named overlays selected at deploy time via
# `isd deploy --overlay <group>`; the named overlays are appended after
# `compose` (in declaration order). Unknown overlay names error out.
[overlays.staging]
compose = ["compose.staging.toml"]

# Deploy strategy. Optional; defaults to "auto" (the per-service
# heuristic from phase 10g lives in storage at services.deploy_strategy_override).
# Manifest-level override applies to every service in the stack unless the
# compose file pins per-service via the existing 10g label.
# Allowed: "auto" | "blue-green" | "rolling" | "recreate"
strategy = "blue-green"

# Per-fleet secrets to mount into this stack. Names refer to entries in
# the controller's `secrets` table (managed via `isd secret put`). Each
# name is bound to the stack at deploy time; the agent receives the
# decrypted ciphertext per the existing v0.3.6 secrets-mount path and
# mounts it under /run/secrets/<NAME> in the container.
#
# Stack-level: every service in the stack sees each secret. For
# service-scoped binding, the existing compose `secrets:` block stays
# canonical and overrides this list.
#
# Phase 0.13 ships per-fleet only. Phase 0.15 introduces a "scope"
# discriminator and `--scope global`; the manifest field gains a
# `[[secrets]]` array-of-tables shape at that time. For 0.13 we keep
# the simpler `secrets = [...]` list to avoid premature complexity.
secrets = ["SONARR_API_KEY", "PROWLARR_API_KEY", "PLEX_TOKEN"]

# Lifecycle hooks. Optional. Each hook is a shell command the agent
# executes on the host (not inside any container) at a specific point.
# cwd: the stack's compose dir on the host (`/etc/isengard/stacks/<name>/`).
# env: passes `ISENGARD_STACK`, `ISENGARD_HOST_ID`, `ISENGARD_DEPLOYMENT_ID`.
# timeout: default 60s, per-hook overridable. Hook failure aborts the
# deploy unless `on_error = "continue"`.
#
# Pre-deploy fires after the controller validates the manifest but before
# `WriteCompose` reaches the agent. Post-deploy fires after the agent
# acks the WriteCompose AND the reconciler reports all services
# `Running`. Failure runs after deploy aborts for any reason (timeout,
# image pull failure, hook itself failing).
[[hooks]]
on = "pre-deploy"
cmd = ["./scripts/backup-config.sh"]
timeout = "120s"

[[hooks]]
on = "post-deploy"
cmd = ["./scripts/notify-discord.sh", "stack deployed"]
on_error = "continue"

[[hooks]]
on = "failure"
cmd = ["./scripts/rollback-and-page.sh"]
timeout = "180s"
```

### `isengard.toml` schema (fleet-level, optional)

```toml
# isengard.toml v1

# Default fleet for any stack in this repo that doesn't pin a fleet in its
# own stack.toml. Optional. Overrides the operator's `isd context use`
# default for this directory tree.
fleet = "homelab"

# Default context binding. Optional. Operator can opt to override at the
# command line via `--context <name>`.
context = "homelab-tailnet"
```

### Parsing + merging

The manifest parser lives in a new crate, `crates/isengard-manifest/`:

```rust
// crates/isengard-manifest/src/lib.rs

pub struct StackManifest {
    pub name: String,
    pub fleet: Option<String>,
    pub compose: Vec<PathBuf>,
    pub overlays: BTreeMap<String, OverlaySpec>,
    pub strategy: Strategy,
    pub secrets: Vec<String>,
    pub hooks: Vec<HookSpec>,

    /// Absolute path to the directory holding `stack.toml`. Used by
    /// hooks (cwd resolution) and by overlay merging.
    pub root: PathBuf,
}

#[derive(Clone, Copy)]
pub enum Strategy { Auto, BlueGreen, Rolling, Recreate }

pub struct HookSpec {
    pub on: HookEvent,
    pub cmd: Vec<String>,
    pub timeout: Duration,
    pub on_error: HookErrorPolicy,
}

pub enum HookEvent { PreDeploy, PostDeploy, Failure }
pub enum HookErrorPolicy { Abort, Continue }

pub struct OverlaySpec { pub compose: Vec<PathBuf> }

impl StackManifest {
    pub fn load(stack_toml: &Path) -> Result<Self, ManifestError>;
    pub fn resolved_compose_paths(&self, overlay: Option<&str>) -> Result<Vec<PathBuf>, ManifestError>;
}

pub struct FleetManifest {
    pub fleet: Option<String>,
    pub context: Option<String>,
}

impl FleetManifest {
    pub fn load_from_walk(start: &Path) -> Result<Option<Self>, ManifestError>;
}
```

Overlay merge semantics (delegated to the existing compose-parser, extended slightly):
- Parse each compose file into the existing `DesiredCompose` value
- For each subsequent file, walk services
  - if a service name is new, insert
  - if it exists, scalar fields (image, restart, network_mode, etc.) are last-write-wins
  - list fields (volumes, environment, ports, depends_on, cap_add, networks) are append-and-dedupe (dedupe key: the string for primitives; the first-component for "k:v" forms like volumes / environment)
  - nested objects (deploy, healthcheck) recurse with the same rules

The merge produces a single `DesiredCompose` that goes on the wire as YAML, the same way it does today (`crates/isd/src/compose_cmd.rs:toml_compose_to_yaml`). Operators NEVER see the merged file; the agent persists it at `/etc/isengard/stacks/<name>/compose.yml`.

### Server API

#### Existing endpoint, extended

`POST /api/v1/stacks` already exists. Body today is `{ name, compose_yaml, host_id? }`. Phase 0.13 extends it with three optional fields and one new status code:

```json
{
  "name": "servarr",
  "compose_yaml": "services:\n  sonarr: ...",
  "host_id": "01J9...",                      // unchanged
  "manifest_toml": "name = \"servarr\"\n...", // NEW: verbatim stack.toml
  "secrets": ["SONARR_API_KEY", ...],         // NEW: per-fleet secret name list
  "hooks": [                                  // NEW: hook list, schema below
    {
      "on": "pre-deploy",
      "cmd": ["./scripts/backup.sh"],
      "timeout_ms": 120000,
      "on_error": "abort"
    }
  ]
}
```

Wire-side validation:
- `manifest_toml`: optional. When present, the controller parses it (using `isengard-manifest`) and asserts `manifest.name == body.name`; mismatch returns 400.
- `secrets`: optional. Each entry must reference an existing row in `secrets`; unknown names return 422 with the list of missing names.
- `hooks`: optional. Each hook validated against the same schema as the TOML manifest. Unknown `on` values return 400.

Status codes: existing 201 / 400 / 409 / 503 / 504 stay. Add 422 for "one or more secrets unknown."

#### New endpoint

`PUT /api/v1/stacks/{id}/manifest`: replace the stored manifest + secret list + hook list without touching compose. Body identical to the manifest portion of `POST /stacks` (no `compose_yaml`). Used by `isd manifest edit` (deferred: out of 0.13 scope; the endpoint is reserved here so the API doesn't change shape later). For 0.13, the manifest is only ever written through `POST /stacks` or `PUT /stacks/<id>/compose` (extended below).

`PUT /api/v1/stacks/{id}/compose`: existing endpoint. Phase 0.13 extends the request body to optionally carry `manifest_toml`, `secrets`, and `hooks` (same shape as `POST /stacks`). When the optional fields are present, the controller updates them in the same transaction as the compose update. When absent, manifest state is preserved unchanged (the operator deploying a compose-only change does NOT inadvertently strip secrets/hooks).

Content-type negotiation: today the endpoint accepts `application/yaml` as the raw body. To carry the extended fields, the endpoint adds an `application/json` content-type that takes the full body shape above (with `compose_yaml` as a string field). YAML-content-type requests stay supported and treat the body as compose-only (no manifest changes). The CLI sends JSON when `stack.toml` is present, YAML otherwise.

#### Read endpoint

`GET /api/v1/stacks/{id}`: today returns `{ id, name, host_id, ... }`. Phase 0.13 adds `manifest_toml`, `secrets`, `hooks` to the response (nullable; absent for stacks that pre-date the manifest). No new endpoint; reuses the existing one.

### Storage schema

New migration `0027_stack_manifest.sql`:

```sql
-- Phase 0.13: stack.toml manifest persistence.

-- Verbatim stack.toml contents. Nullable; legacy stacks (deployed via the
-- v0.3d direct-compose path) have NULL manifest until reuploaded.
ALTER TABLE stacks ADD COLUMN manifest_toml TEXT;
ALTER TABLE stacks ADD COLUMN manifest_sha256 TEXT;
ALTER TABLE stacks ADD COLUMN manifest_imported_at TEXT;

-- Per-stack secret bindings. One row per (stack, secret_name). The
-- secrets themselves stay in the `secrets` table (added in 0025);
-- this table just binds them to stacks. ON DELETE CASCADE on the stack
-- side; ON DELETE RESTRICT on the secret side (cannot drop a secret
-- still bound to any stack: the operator must unbind first).
CREATE TABLE stack_secrets (
    stack_id    INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    secret_name TEXT    NOT NULL REFERENCES secrets(name) ON DELETE RESTRICT,
    bound_at    TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (stack_id, secret_name)
);

CREATE INDEX idx_stack_secrets_secret ON stack_secrets(secret_name);

-- Per-stack lifecycle hooks. Pre-deploy / post-deploy / failure. The
-- agent receives the hook list as part of the deploy payload (NOT
-- ahead-of-time persisted on the agent host); persisted here so the
-- controller can replay on agent reconnect and so the dashboard can
-- surface "what runs on next deploy".
CREATE TABLE stack_hooks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    stack_id    INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    on_event    TEXT    NOT NULL CHECK (on_event IN ('pre-deploy','post-deploy','failure')),
    cmd_json    TEXT    NOT NULL,                      -- JSON-encoded Vec<String>
    timeout_ms  INTEGER NOT NULL DEFAULT 60000,
    on_error    TEXT    NOT NULL DEFAULT 'abort'
                CHECK (on_error IN ('abort','continue')),
    ordinal     INTEGER NOT NULL,                       -- preserves manifest order within event
    created_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_stack_hooks_stack_event
    ON stack_hooks(stack_id, on_event, ordinal);

-- Deploy-strategy override on the stack (vs the existing per-service
-- override in 0012). Stack-level applies to all services unless a
-- service has its own override. Values match 0012 plus 'rolling' and
-- 'recreate' for the manifest-driven additions.
ALTER TABLE stacks ADD COLUMN deploy_strategy TEXT
    CHECK (deploy_strategy IS NULL OR deploy_strategy IN ('auto','blue-green','rolling','recreate'));

-- Source fleet binding from the manifest. Nullable. Future phases use
-- this to filter `isd deploy --all` against the operator's contexts.
ALTER TABLE stacks ADD COLUMN manifest_fleet TEXT;
```

Three notes on schema decisions:

1. **`stack_hooks.cmd_json` is JSON rather than a separate `hook_args` table.** Hooks are read as one bundle per deploy; relational normalization would force a join with no upside. JSON-as-TEXT is the right shape for this access pattern.
2. **`deploy_strategy` lives on `stacks`, not in a new `stack_strategy` table.** It's a single nullable field; no overlap, no history.
3. **`ON DELETE RESTRICT` for `stack_secrets.secret_name`.** Forces operators to unbind before delete. Bias toward surfacing the dependency rather than silently breaking deploys.

### Agent surface

The agent receives `WriteCompose` today. Phase 0.13 extends the proto (in `crates/isengard-proto/proto/isengard.v1.proto`):

```protobuf
// Existing:
message WriteCompose {
    string request_id      = 1;
    string stack_name      = 2;
    string compose_yaml    = 3;
    string expected_sha256 = 4;
    bool   force           = 5;

    // NEW in phase 0.13:
    string manifest_toml   = 6;       // optional; persisted on the agent at stack.toml
    repeated string secrets = 7;      // secret names to mount; values arrive via the v0.3.6 secret-distribution path
    repeated LifecycleHook hooks = 8; // run on host, see semantics below
    string deployment_id   = 9;       // ULID, used for hook env + audit
}

message LifecycleHook {
    string on         = 1;  // "pre-deploy" | "post-deploy" | "failure"
    repeated string cmd = 2;  // argv form; cmd[0] is the program, rest are args
    uint64 timeout_ms = 3;
    string on_error   = 4;  // "abort" | "continue"
}
```

Wire-compat: the new fields are protobuf-optional (proto3 default behavior); an agent compiled before 0.13 ignores them. Controllers compiled before 0.13 don't send them. The minimum agent version for 0.13 features is captured in the controller's existing version-handshake.

Agent persistence: `manifest_toml` lands at `/etc/isengard/stacks/<name>/stack.toml` alongside the existing `compose.yml`. The agent does NOT parse it for behavior; it's persisted for operator inspection + `isd manifest cat` (deferred). Hook execution + secret mounting are driven by the explicit fields in `WriteCompose`, not by re-parsing the persisted manifest.

### isd CLI surface

New subcommands and flags on the existing surface:

| Command | Effect |
|---|---|
| `isd deploy` (no args) | Read `./stack.toml` from cwd. If absent, error with: "no stack.toml in cwd; pass a path or run `isd deploy <path>` to deploy a single compose file." |
| `isd deploy <stack-name>` | Treat `<stack-name>` as a subdir of cwd; read `<stack-name>/stack.toml`. Same flow as no-args. |
| `isd deploy <compose-path>` | Legacy path. Backward-compatible: `<compose-path>` ends in `.yml` / `.yaml` / `.toml` AND isn't a directory -> deploy as a single compose, no manifest. Preserves the v0.3d shape. |
| `isd deploy --all` | Walk cwd's immediate subdirs; deploy every dir with a `stack.toml`. |
| `isd deploy --all --root <path>` | Walk `<path>`'s immediate subdirs instead of cwd. |
| `isd deploy --diff` | Dry-run: print the plan for each affected stack, exit without writing. |
| `isd deploy --strategy <s>` | Override the manifest's `strategy` for this invocation. Doesn't persist to the manifest. |
| `isd deploy --overlay <name>` | Apply the named `[overlays.<name>]` block from `stack.toml`. |
| `isd deploy --fail-fast` | On `--all`, stop at the first failing stack. Default is "continue, report at the end." |
| `isd deploy --yes` | Skip interactive confirmation prompts. |
| `isd deploy --force` | Bypass sha256 optimistic-concurrency check. Inherited from v0.3d behavior. |

Exit codes:

- 0: all stacks deployed (or `--diff` produced a clean preview)
- 1: one or more stacks failed (full report still printed)
- 2: argument / manifest parse error (no deploys attempted)
- 3: controller unreachable (no deploys attempted)

`isd deploy --all` output format: one line per stack, status glyph + name + summary. After the run, a totals line and a per-failure error block:

```
$ isd deploy --all
servarr        ✓ deployed   (3 services changed, sha 7f3a..)
monitoring     ✓ deployed   (1 service changed, sha 1c44..)
homepage       ✗ failed     (image pull: nginx:latest)
paperless      ✓ no-change

Summary: 3 deployed, 1 failed, 0 skipped. (12.4s)

homepage:
  POST /api/v1/stacks/{id}/compose -> 503
    "agent for host 01J9... isn't connected"
  Rerun with `isd ps` to check host health.
```

### Multi-stack walking semantics

Order of operations for `isd deploy --all`:

1. Walk cwd's immediate subdirs in lexical order. (Deterministic for diffs; operators can prefix with `00-`, `10-`, `20-` if they want priority.)
2. For each subdir with `stack.toml`: parse the manifest. Parse failures collect into a "rejected" list and DO NOT trigger any controller calls.
3. After all manifests parse successfully (or if `--fail-fast` and any failed): for each accepted manifest, run the deploy synchronously in the same lexical order.
4. Per-stack: parse compose, merge overlays, POST or PUT through the same code path `isd deploy <path>` uses today. Hook + secret + manifest fields flow through the JSON content-type variant of the endpoint.
5. On failure of one stack: by default, continue with the rest. With `--fail-fast`, abort and skip the remaining.
6. End-of-run summary printed regardless.

Concurrency: serial in 0.13. Parallel `--all` is an obvious next step but introduces ordering questions for hooks that share state (e.g. a pre-deploy script in two stacks both writing `./.deploy.lock`); defer to a future phase with explicit `parallelism` semantics.

### Deploy strategy semantics

Each strategy maps to existing agent behavior; 0.13 doesn't ship new orchestration code (that's phase 0.10g territory already merged), it just plumbs the manifest field through.

| Strategy | Agent behavior |
|---|---|
| `auto` | Per-service heuristic from phase 10g: if the service has a healthcheck AND no published-port-rebind would conflict, blue-green; else in-place recreate. Today's default. |
| `blue-green` | Force blue-green for every service in the stack: spin up new revision, healthcheck, swap, drain. The existing `deployment_groups` rolling state machine drives it. |
| `rolling` | Multi-host: one host at a time, the existing `deployment_parallelism` from migration 0018. Single-host: degenerates to in-place recreate. (Stack-level `rolling` populates `stacks.deployment_parallelism` to `'1'` if it's null.) |
| `recreate` | Stop-then-start every service in the stack. Maps to the existing in-place path; explicit-recreate behavior. |

When the manifest pins `strategy = "blue-green"` but a service has `deploy_strategy_override = 'in-place'` in 10g labels, the per-service override wins (more specific). The controller emits a warning event on the stack so operators can see the conflict in dashboard event logs.

### Hook execution semantics

The agent runs hooks. Spec:

- **cwd**: `/etc/isengard/stacks/<stack-name>/` on the host. The directory holds `compose.yml` and (post-0.13) `stack.toml`. Hooks referencing `./scripts/foo.sh` resolve relative to this dir; the operator is responsible for syncing scripts to the agent (out of scope for 0.13; see "Risks": for now, hooks can either be absolute paths to system tooling like `/usr/bin/curl` OR scripts shipped alongside the compose file via an existing volume/configmap pattern).
- **env**: starts from the agent's own environment, then layered with:
  - `ISENGARD_STACK=<name>`
  - `ISENGARD_HOST_ID=<host_id_hex>`
  - `ISENGARD_DEPLOYMENT_ID=<deployment_ulid>`
  - `ISENGARD_HOOK_EVENT=pre-deploy|post-deploy|failure`
  - `ISENGARD_MANIFEST_PATH=/etc/isengard/stacks/<name>/stack.toml`
- **timeout**: per-hook, default 60s. Enforced via `tokio::time::timeout` on the spawned process; on timeout, send SIGTERM, wait 5s, then SIGKILL.
- **on_error = "abort" (default)**: hook exit != 0 OR timeout aborts the deploy. For `pre-deploy`: the WriteCompose is NOT applied. For `post-deploy`: the compose IS applied (the deploy already happened); the failure event is logged and the dashboard reports the stack as `degraded`.
- **on_error = "continue"**: log the failure as a warning event; continue.
- **failure hook**: fires once per failed deploy. Receives the failure reason via `ISENGARD_FAILURE_REASON=<short string>` and `ISENGARD_FAILURE_DETAIL=<verbose>` env. Failure hook does NOT itself trigger another failure hook on failure (no recursion).

Audit trail: each hook execution writes a row to the existing `events` table with `kind='lifecycle_hook_run'`, `payload={ stack, event, exit_code, duration_ms }`.

### Migration: existing stacks

Stacks deployed via the pre-0.13 path have NULL `manifest_toml`. They keep working as before; the controller treats them as "compose-only" stacks. Two operator paths to manifest-ify them:

1. **Implicit**: next time the operator runs `isd deploy <stack>` with a `stack.toml` present, the new manifest fields flow through `PUT /stacks/<id>/compose` (JSON variant). The manifest is persisted; the stack now has `manifest_toml IS NOT NULL`. No special command.
2. **Explicit reverse-import**: deferred to a future `isd manifest export <stack>` command. Out of scope for 0.13.

For 0.13's done bar, we add a one-shot path to the manifest crate: `StackManifest::from_compose_and_inventory(stack: &Stack, compose_yaml: &str) -> StackManifest` that produces a minimal manifest (name + auto strategy + no secrets / hooks) so operators can `cat > stack.toml` and iterate. Lives in the manifest crate, no server-side endpoint.

### What the existing `compose_cmd.rs` becomes

`crates/isd/src/compose_cmd.rs` today implements:
- `run_deploy(DeployArgs)`: single-file deploy
- `run_diff(DiffArgs)`: preview
- `run_edit(EditArgs)`: $EDITOR round-trip

Phase 0.13 keeps all three. The deploy path forks at the top:

```rust
pub async fn run_deploy(args: DeployArgs, context: Option<&str>) -> Result<()> {
    let plan = resolve_deploy_plan(&args)?;
    match plan {
        DeployPlan::Single(SingleDeploy { compose_path, .. }) => {
            // existing v0.3d path
        }
        DeployPlan::Manifest(manifest_deploy) => {
            run_manifest_deploy(manifest_deploy, context).await
        }
        DeployPlan::All(all_deploy) => {
            run_all_deploy(all_deploy, context).await
        }
    }
}
```

`resolve_deploy_plan` inspects argv:
- `--all` set: `DeployPlan::All` (walks cwd or `--root`)
- `args.path` is a directory with `stack.toml`: `DeployPlan::Manifest`
- `args.path` is a file ending `.yml` / `.yaml` / `.toml`: `DeployPlan::Single` (legacy)
- no `args.path` and `./stack.toml` exists: `DeployPlan::Manifest(cwd)`
- no `args.path` and no `./stack.toml`: error

### Out-of-scope, explicitly

- `isd manifest cat <stack>` / `isd manifest export <stack>` / `isd manifest edit <stack>`: deferred. The endpoints exist for the round-trip; the CLI commands are a separate small phase.
- Stack-level `recreate` blocking pre-deploy hooks from racing against in-flight containers. Hook ordering vs container lifecycle is the same as today's lifecycle hooks (phase 12bc).
- Hook script syncing. Operators get hooks running on the host with whatever's already there (`curl`, `notify-send`, system scripts). A "ship this directory alongside compose" sync surface is a phase 0.17+ idea.
- Manifest-level resource quotas (`max_cpu_per_stack`, etc.). Out.
- Manifest-level network policy (`isolate_from = [...]`). Out.

## Test strategy

Unit tests in `crates/isengard-manifest/`:

- TOML parse: minimal manifest (name only) passes
- TOML parse: rejects missing `name`
- TOML parse: rejects unknown `strategy` value
- TOML parse: rejects `hooks[].on` outside the three-value enum
- Overlay merge: scalar last-write-wins
- Overlay merge: list append-and-dedupe by primary key (volume "src:dst" dedups on "src:dst", env "KEY=val" dedups on "KEY")
- `resolved_compose_paths`: base only, base + overlay, base + named overlay
- Error: overlay name not in manifest -> clear error message
- Error: compose file not found relative to manifest dir -> error includes the searched path

Unit tests in `crates/isd/src/compose_cmd.rs`:

- `resolve_deploy_plan`: each branch (single, manifest-from-cwd, manifest-from-subdir, all, all-with-root, error-when-nothing)
- TOML compose -> YAML conversion already covered (existing tests stay green)

Integration tests in `crates/isengard-plugins/dashboard/tests/`:

- `POST /api/v1/stacks` with new fields persists `manifest_toml`, `stack_secrets`, `stack_hooks` rows
- Unknown secret in `secrets` field -> 422 with the missing-name list
- `PUT /api/v1/stacks/<id>/compose` with JSON content-type: manifest fields update; YAML content-type: manifest fields preserved
- `GET /api/v1/stacks/<id>` returns the manifest fields when present, omits / nulls them when not

End-to-end test (inside the OrbStack `wisp` VM, or via the existing iso-test VM):

- Set up a fixture repo with three subdirs (servarr, monitoring, homepage); each has `stack.toml` + minimal `compose.toml`
- `isd deploy --all` succeeds, all three stacks visible in `isd ps`
- Touch one compose, rerun `isd deploy --all`: one "changed," two "no-change"
- Drop one subdir's compose file, rerun: error reported for that stack, others unaffected
- Manifest with a `pre-deploy` hook that exits 0: deploy completes, event row written
- Manifest with a `pre-deploy` hook that exits 1: deploy aborts, no compose write, failure event row written

## Risks

- **Manifest vs compose drift.** Operator edits `compose.toml` but not `stack.toml`; controller re-receives the same manifest, no behavior change. Inverse: operator edits `stack.toml` (changes secrets) but not `compose.toml`; the compose hash doesn't change but the manifest hash does. Mitigation: the JSON variant of `PUT /stacks/<id>/compose` takes a manifest_sha256 in addition to compose_sha256; conflict on either fails the PUT. `isd deploy` always re-uploads both even if the compose is unchanged, so manifest-only changes get applied. Document the dual-hash semantics in the API doc.
- **Hooks expose the host.** A `pre-deploy` hook can run arbitrary commands as the agent user. Today's agent runs as root (per the systemd unit). Operators who write malicious / typo'd hooks DELETE THINGS. Mitigation for 0.13: hooks are stored encrypted at rest (already true: the controller's `secrets` infrastructure can be reused for the `cmd_json` column with minor extension), and the dashboard surfaces "next deploy will run these N hooks as root on these N hosts" as a warning. A more principled fix (dedicated `isengard-hooks` user with NoNewPrivileges + ReadOnlyPaths) is a phase 0.16+ tightening.
- **Secrets in manifest.** The manifest references secret NAMES, never values. Names in TOML are fine (operators commit the manifest to git); values stay in the controller. Mitigated by design; flagged only because reviewers will ask.
- **`isd deploy --all` ordering surprises.** Lexical ordering is deterministic but operators who name dirs `app`, `db`, `proxy` get db third. We considered topological sort (db before app before proxy) via `depends_on` in the manifest, but rejected: too much complexity for a feature operators don't yet ask for. Document the lexical rule and let operators rename if they care.
- **Hooks running on agent reconnect.** If the agent disconnects mid-deploy after the pre-deploy hook fired but before WriteCompose, the controller has no clean replay story. Mitigation: pre-deploy hooks SHOULD be idempotent (operator responsibility, documented). The controller persists the deployment in the existing `deployments` table; on reconnect, if state is `Pending`, the deploy resumes from the WriteCompose step (hooks already ran in the previous attempt). Mark this clearly in the API doc.
- **Migration churn.** Adding `manifest_toml` / `manifest_sha256` / `manifest_imported_at` / `deploy_strategy` / `manifest_fleet` to `stacks` is five columns in one ALTER block. SQLite handles them serially; the migration runs in a transaction. No data backfill (everything nullable). Risk is bounded.
- **Backward compatibility for the dashboard route table.** The dashboard's existing `/stacks/:id` GET response gains optional fields. Existing dashboard JS that ignores unknown fields keeps working; the new fields surface only when the dashboard frontend is upgraded. No coupling that fights.

## Open questions

- **Should `isengard.toml` live in cwd or repo root?** Today's spec says "optional, sits next to the per-stack dirs." If operators put it at git root and `cd subdir && isd deploy --all` from the subdir, the file is invisible. Phase 0.13 fix: walk upward from cwd looking for `isengard.toml` until we hit a `.git` dir or the filesystem root, take the closest. If two operators have conflicting expectations, we'll hear about it.
- **Overlay selection via env var?** A `ISENGARD_OVERLAY=staging isd deploy --all` shape is convenient for CI. Defer to follow-up; the flag is enough for 0.13.
- **What happens if `stack.toml` declares a fleet that doesn't match the operator's `isd context use` selection?** Today: warning, deploy proceeds against the selected context. Stricter: error, force operator to `isd context use <fleet>`. We go with warning for 0.13 because the multi-fleet workflow is still nascent; stricter behavior can layer in once the fleet model is exercised in anger.
- **`deployment_id` in `WriteCompose`.** A ULID emitted by the controller per deploy. Used by hooks and audit. Today's `WriteCompose` lacks it; we add the field in the proto extension. Agents older than 0.13 ignore it; new agents thread it into hook env. No backward-compat risk; just flagging the proto bump.
- **Should `stack_secrets` enforce a max length?** TOML manifests with 200 secrets are operator error; the DB doesn't care. No limit at the schema level; CLI / dashboard cap at, say, 32 with a clear error if breached. Phase 0.13 ships uncapped; if abuse shows up, we add a `CHECK`.
