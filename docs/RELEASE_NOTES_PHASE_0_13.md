# Phase 0.13: `stack.toml` manifest + `isd deploy --all`

> Phase 0.13 of the v0.4 foundation arc. Spec:
> `docs/superpowers/specs/2026-05-11-phase-0-13-stack-toml-manifest-design.md`.
> Plan: `docs/superpowers/plans/2026-05-11-phase-0-13-stack-toml-manifest.md`.

## What this is

`stack.toml` is the orchestration manifest that lives next to a stack's
compose file(s). Compose stays purely about services; the orchestration
metadata (stack name, fleet binding, deploy strategy, secret bindings,
lifecycle hooks) moves up one level. `isd deploy --all` walks immediate
subdirs and ships every directory that has a `stack.toml`.

Result: an operator with five stacks (servarr, monitoring, homepage,
paperless, immich) runs one command from the repo root and every stack
ships in one round-trip per stack.

## File layout

```
my-homelab/
├── isengard.toml                 # optional fleet config
├── servarr/
│   ├── stack.toml                # required: orchestration manifest
│   └── compose.toml              # required: compose body
├── monitoring/
│   ├── stack.toml
│   ├── compose.toml              # base
│   └── compose.staging.toml      # overlay
└── homepage/
    ├── stack.toml
    └── compose.toml
```

## `stack.toml` schema (v1)

```toml
# Required.
name = "servarr"

# Optional fleet binding. Captured + persisted; resolution against
# saved contexts is deferred to phase 0.14+.
fleet = "homelab"

# Required, non-empty. Earlier entries are the base; later entries
# overlay. Paths are relative to stack.toml's directory.
compose = ["compose.toml"]

# Optional. Named overlays selected via `isd deploy --overlay <name>`.
[overlays.staging]
compose = ["compose.staging.toml"]

# Optional. Default: "auto". Allowed: auto, blue-green, rolling, recreate.
strategy = "blue-green"

# Optional. Per-fleet secret names. Phase 0.13 ships per-fleet only;
# phase 0.15 introduces `--scope global`.
secrets = ["SONARR_API_KEY", "PLEX_TOKEN"]

# Optional. Hooks run on the host as the agent user (today: root).
# cwd: /etc/isengard/stacks/<name>/. timeout default 60s.
[[hooks]]
on = "pre-deploy"
cmd = ["./scripts/backup-config.sh"]
timeout = "120s"

[[hooks]]
on = "post-deploy"
cmd = ["./scripts/notify-discord.sh", "deployed"]
on_error = "continue"
```

## Operator quickstart

Create a stack from scratch in five lines:

```
mkdir my-homelab && cd my-homelab
mkdir homepage && cd homepage
printf 'name = "homepage"\ncompose = ["compose.toml"]\n' > stack.toml
printf 'services:\n  homepage:\n    image: ghcr.io/gethomepage/homepage:latest\n' > compose.toml
cd .. && isd deploy --all
```

Subsequent runs (no compose change) are a no-op. Edit `homepage/compose.toml`,
rerun `isd deploy --all`, see one redeploy.

## Migrating existing stacks

Stacks deployed via the pre-0.13 single-compose path keep working
unchanged. To start using `stack.toml` against them:

1. `cd` into the stack's directory (or create one).
2. Drop a minimal manifest: `printf 'name = "<stack>"\ncompose = ["compose.toml"]\n' > stack.toml`.
3. `isd deploy` from that directory. The manifest persists on the
   controller; the next `isd deploy --all` from the parent picks it up.

## CLI surface

```
isd deploy                                # deploy ./stack.toml
isd deploy <subdir>                       # deploy <subdir>/stack.toml
isd deploy <compose.yaml>                 # legacy single-file path (unchanged)
isd deploy --all                          # walk cwd's subdirs, deploy each
isd deploy --all --root <path>            # walk <path>'s subdirs
isd deploy --overlay staging              # apply [overlays.staging] from manifest
isd deploy --strategy rolling             # override manifest's strategy for this run
isd deploy --diff                         # dry-run; print plan for each
isd deploy --fail-fast                    # with --all, stop at first failure
```

## Wire surface changes

### `POST /api/v1/stacks` (extended)

Optional body fields: `manifest_toml`, `secrets`, `hooks`.

New status code: **422** when one or more entries in `secrets` reference
a name not in the controller's `secrets` table. Body:

```
{ "error": "unknown secrets", "missing": ["FOO_KEY", "BAR_KEY"] }
```

### `GET /api/v1/stacks/{id}` (extended)

Response now includes `manifest_toml`, `manifest_sha256`,
`manifest_imported_at`, `deploy_strategy`, `manifest_fleet`, `secrets`,
`hooks`. Pre-0.13 stacks return null/empty values.

### `WriteCompose` proto (extended)

Four new optional fields (numbers 6..=9): `manifest_toml`, `secrets`,
`hooks` (repeated `LifecycleHook`), `deployment_id` (controller-minted
ULID). Pre-0.13 agents ignore the new fields (proto3 default);
pre-0.13 controllers don't set them.

### Storage migration 0028

- Five nullable columns on `stacks`: `manifest_toml`, `manifest_sha256`,
  `manifest_imported_at`, `deploy_strategy`, `manifest_fleet`.
- New `stack_secrets` table. FK on the secrets side is `ON DELETE
  RESTRICT`: operators must unbind a secret before they can drop it.
- New `stack_hooks` table storing per-stack lifecycle hooks
  (`on_event`, `cmd_json`, `timeout_ms`, `on_error`, `ordinal`).

## Known limitations (Phase 0.13)

- **Hook execution on the host is NOT yet implemented in the agent.**
  The proto carries the hook list and the dashboard persists it; the
  agent-side `lifecycle_hooks` module that runs pre/post/failure hooks
  is deferred to a follow-up. Manifest persistence on the agent works
  (`stack.toml` lands at `/etc/isengard/stacks/<name>/stack.toml`).
- **Stack-level secret mounting from the manifest is also deferred.**
  Per-service compose `secrets:` blocks remain the canonical surface
  in 0.13; the stack-level mount layered from `secrets = [...]` is a
  small follow-up that piggybacks on the existing secret-mount path.
- **`isd manifest cat / export / edit`** are deferred.
- **JSON content-type variant of `PUT /api/v1/stacks/{id}/compose`**
  is reserved for follow-up. Today's PUT stays YAML-only and
  preserves manifest state on the agent (the agent treats an empty
  `manifest_toml` field as "no update"). Re-deploying via `POST
  /stacks` is the supported path for manifest changes in 0.13.
- **Global-scope secrets** ship in Phase 0.15 (separate from this
  phase). The 0.13 manifest's `secrets = [...]` field is per-fleet.
- **`--all` is sequential.** Parallel `--all` introduces ordering
  questions for hooks; deferred.
- **No topological sort** for `--all`. Lexical order; operators who
  want priority prefix dirs with `00-`, `10-`, `20-`.
- **Hook scripts must already exist on the host.** Phase 0.13 does
  not ship scripts alongside the compose; operators reference
  absolute paths to system tooling or scripts they pre-deploy via
  configmap-like means.
- **Hooks run as the agent user (today: root).** Operators with
  malicious `cmd` values can wreck the host. A dedicated
  `isengard-hooks` user with `NoNewPrivileges` + `ReadOnlyPaths` is
  a Phase 0.16+ tightening.

## Compatibility

- Agents older than 0.13 receive `WriteCompose` with the new fields
  absent on the wire (proto3 default); they ignore them.
- Controllers older than 0.13 don't set the new fields; new agents
  decode empty values and behave exactly as before.
- Legacy stacks (deployed via `isd deploy <compose.yaml>`) keep
  working. Their `manifest_*` columns are NULL; redeploying via a
  `stack.toml` populates them in place.
