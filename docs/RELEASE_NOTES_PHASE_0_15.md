# Phase 0.15: Global secrets via sync-on-put

> Phase 0.15 of the v0.3 arc. Implements the brainstorm decision locked on 2026-05-10: per-fleet secrets stay the default, and a new `--scope global` flag fans put/rm/list out across every saved context. No new service, no operator-machine-as-keystore; every fleet keeps its own encryption under its own master.key.

## What this is

A `--scope` flag on `isd secret put`, `isd secret rm`, and `isd secret list`. Default stays `fleet` (single-context, identical to v0.3.6 behaviour). `--scope global` walks every entry in `~/.config/isengard/credentials.toml` and applies the operation per-context.

Plus `--sync-secrets` + `--sync-from` flags on `isd context create` so the operator gets a backfill checklist when adding a new fleet.

## CLI surface

```text
isd secret put <NAME> [--from-file PATH] [--scope fleet|global]
isd secret rm  <NAME>                    [--scope fleet|global]
isd secret list                          [--scope fleet|global]

isd context create <NAME> --ssh <TARGET> [--sync-secrets] [--sync-from <SOURCE>]
isd context create <NAME> --http <URL>   [--sync-secrets] [--sync-from <SOURCE>]
```

### Put

`--scope global`:

- Reads the value once (stdin or `--from-file`), opens a session against every saved context, and PUTs `/api/v1/secrets/<NAME>` on each.
- Best-effort: per-context failures do NOT abort the run. The remaining contexts still receive the value.
- Exits non-zero iff any context failed, so scripts can detect partial application.
- Prints a one-line summary:

```text
Stored secret "cf_dns_token" globally: synced to 2/3 contexts (alice, carol); failed: bob (timeout: connection refused)
```

### Rm

`--scope global`:

- DELETEs the secret in every saved context.
- Per-context 404s are treated as "already gone" (success, but noted separately so the operator can see whether anything actually changed).
- Same best-effort + summary line shape as put. Exits non-zero only on real failures, not on 404s.

### List

`--scope global`:

- Walks every saved context, fetches `/api/v1/secrets` from each, and renders a coverage table:

```text
NAME            SCOPE    FLEETS
cf_dns_token    global   alice, bob, carol
github_token    fleet    alice
prod_db_pw      partial  bob, carol
```

Scope classification works against the count of *reachable* contexts, so one offline fleet doesn't flip every otherwise-global secret to `partial`. Unreachable contexts are reported on stderr; the run errors only when every context failed.

`partial` is name-only divergence: the global listing does NOT verify that values match across fleets. The operator-side CLI has no way to read plaintext (see "Why no auto-copy" below), so divergent values in two fleets with the same name read as `global`.

### Context create with sync-secrets

`--sync-secrets` (with optional `--sync-from <SOURCE>`):

- After upserting the new context into the credentials file, opens a session against the source context (defaults to the file's current `default_context`) and against the newly-added context.
- Lists every secret name in the source that is missing from the new fleet and prints them as a to-do list.
- Does NOT copy values. Re-running `isd secret put <name> --scope global` for each is the supported backfill path.

Sample output:

```text
Saved context "carol".

Sync check: 2 secret(s) in "alice" not yet in "carol":
  - cf_dns_token
  - github_token

To backfill, re-run `isd secret put <name> --scope global` for each (the operator-side CLI can't read plaintext, so we list names only).
```

## Why no auto-copy

The brainstorm asked: when an operator joins a new fleet, can `--sync-secrets` walk every global secret in an existing context and put it into the new one? In practice, no.

The dashboard's `/api/v1/secrets` surface is write-only. GET returns metadata (`name`, `created_at`, `updated_at`); no endpoint anywhere on that surface returns plaintext. Only the agent's `FetchSecret` mTLS gRPC call gets plaintext back, and that's gated by per-agent mTLS identity and host_id; the operator-side CLI cannot impersonate an agent.

This is by design: keeping plaintext out of the operator's machine is the whole point of master.key + encrypted SQLite. Adding a "give me everything back" operator endpoint would defeat that. So the flag prints a backfill checklist and the operator re-puts each value from their own source of truth (password manager, env file, etc.). That's also why Phase 0.15 is "sync-on-put": the operator's `put` carries the plaintext; there is no value sync between controllers.

## Best-effort semantics: rationale

Put and rm could be all-or-nothing (any failure rolls back every context) or best-effort (a failure in one context doesn't block the others). Phase 0.15 picks best-effort.

Rationale: rolling back means writing N more requests at risk of further failure, and the natural recovery path is just "fix the broken fleet and re-run `secret put --scope global`", which is idempotent. The summary line tells the operator exactly which fleets to retry against. All-or-nothing semantics would also force a value to be held in memory longer (to replay), which is the opposite of what the security stance wants.

The exit code still flips non-zero on any failure, so CI / scripts detect the partial state and the operator decides.

## Encryption model: no change

Each controller continues to encrypt under its own `master.key`. There is NO shared keystore, NO cross-fleet master key exchange, NO control-plane secret-sync service. Global scope is a fan-out at the operator CLI layer only. If fleet A and fleet B both hold a secret named `cf_dns_token`, the bytes on disk are different (different keys, different nonces) even when the plaintext is identical.

## Tests

- 11 new unit tests covering `Scope` parsing on put/rm/list, `--sync-secrets`/`--sync-from` parsing on context create, `classify_coverage` truth table, and `ScopeReport` summary buckets.
- One ignored `#[tokio::test]` integration test (`secret::tests::global_put_fans_out_to_every_context`) that spins up two `wiremock::MockServer` instances, points a temp credentials file at them, calls `run_put` with `Scope::Global`, and verifies each stub received exactly one PUT. Opt-in via `cargo test -p isd -- --ignored secret::tests::global_put_fans_out`.

## What's NOT in 0.15

- A control-plane secret-sync service. Operator's CLI is the only fan-out point. Confirmed by the 2026-05-10 brainstorm ("no extra service").
- Cross-fleet conflict resolution. If `cf_dns_token` exists in two fleets with different values, `isd secret ls --scope global` shows `global` (name coverage only); the operator decides whether that's intentional.
- Plaintext readback from the operator CLI. As above: keeping plaintext on the operator's machine is out of scope and would defeat the master.key model.
- `--scope global` on any other subcommand (route, ps, deploy). Phase 0.15 is secrets-only. If routing rules grow a similar need they'll get their own scope flag.

## Operator-facing change summary

| Before                                  | After                                                          |
|-----------------------------------------|----------------------------------------------------------------|
| `isd secret put NAME`                   | unchanged (defaults to `--scope fleet`)                        |
| (no fan-out)                            | `isd secret put NAME --scope global` walks every saved context |
| (no aggregation)                        | `isd secret ls --scope global` shows coverage per name         |
| (no backfill helper)                    | `isd context create <name> ... --sync-secrets` lists missing   |

Default behaviour for existing scripts is unchanged. Adopters opt in by passing `--scope global` explicitly.
