# Phase 9b.1: Container-Label Policy Discovery

Builds on the Phase 9a-9d policy foundation and the Phase 9e-9f approval flow.
Closes the container-scope hole left in Plan A: the resolver already honored
container-scope rows but no producer ever wrote one. Now operators can pin,
gate, snooze, route-by-channel, or set failure handling directly from a
service's compose labels.

## What's new

- New label family `isengard.policy.*` parsed at ingest time:

  | Label                              | Field             | Values                                |
  |------------------------------------|-------------------|---------------------------------------|
  | `isengard.policy.strategy`         | strategy          | `pinned` / `tag-only` / `minor` / `any` |
  | `isengard.policy.gate`             | gate              | `auto` / `approval` / `never`         |
  | `isengard.policy.paused_until`     | paused_until      | RFC 3339 timestamp                    |
  | `isengard.policy.on_failure`       | on_failure        | `rollback` / `keep` / `notify`        |
  | `isengard.policy.approver_channel` | approver_channel  | notifier channel id                   |

  Enum values accept both kebab-case (`tag-only`) and snake_case (`tag_only`).

- The agent's existing label watcher (extended in Phase 9b.1) now reports
  containers that carry any `isengard.policy.*` key, even if they don't
  expose any HTTP route via `isengard.expose*`.

- Each report upserts a container-scope row at
  `(scope_type=container, scope_key=<host_id>/<container_name>)`. The Plan A
  resolver already prefers these rows over fleet, stack, and service rows.

- Cleanup is event-driven plus periodic. When the container exits
  (`stop` / `die` / `destroy`) the row is deleted immediately. A 1h-interval
  reaper sweeps any container-scope row whose `updated_at` is older than
  24h, covering missed events (e.g. agent crashed during destroy).

- Settings to Policies: container-scope rows are listed read-only with a
  "from labels" pill. The editor's container radio is disabled with a
  tooltip pointing at the compose file as the source of truth.

## Breaking changes

None. Migration 0016 is reused. No proto changes.

## How to use

Add labels under any service in your compose file. Example:

```yaml
services:
  cache:
    image: redis:7
    labels:
      isengard.enable: "true"
      isengard.policy.strategy: "any"
      isengard.policy.gate: "auto"
  api:
    image: ghcr.io/acme/api:1.4.2
    labels:
      isengard.enable: "true"
      isengard.policy.strategy: "pinned"
      isengard.policy.paused_until: "2026-06-15T00:00:00Z"
      isengard.policy.approver_channel: "ops"
  web:
    image: ghcr.io/acme/web:2.0.0
    labels:
      isengard.enable: "true"
      isengard.policy.strategy: "tag-only"
      isengard.policy.gate: "approval"
      isengard.policy.on_failure: "notify"
```

After redeploy, visit Settings to Policies. New container-scope rows for
`api` and `web` appear at the bottom, marked "from labels". Effective
policy on the Stack detail will show `provenance.strategy = Container`.

## How parse errors surface

If a label value is malformed (e.g. `isengard.policy.strategy = pinneded`),
the controller logs a warning containing the offending label key + value
and skips the upsert for that container. Any pre-existing row for that
container is left intact, so a typo doesn't accidentally clear an existing
policy.

## Cleanup behavior

- Removing the labels from compose and redeploying drops the
  container-scope row on the next agent report (the parsed body is empty,
  which the ingest treats as the delete signal).
- Stopping or destroying the container drops the row via the
  `ContainerLabelsRemoved` event.
- Long downtime: the 24h reaper covers missed events; rows older than that
  with no fresh report are deleted on the next hourly sweep.

## Follow-ups (deferred)

| Phase | Summary |
|-------|---------|
| 9g | Discord interactive callbacks (same pattern as Telegram). |
| 9h | Maintenance windows (cron-like grammar for the `window` field; not a label yet). |
| 9i | `Minor` strategy: semver-aware tag bumping. |
| 9j | `Rollback` failure handler (couples with Phase 10 deploy story). |

## Notes

- Container-scope policy rows are intentionally not authorable from the UI.
  This keeps compose the single source of truth for them and avoids the
  conflict resolution headache of two writers fighting over the same key.
  If a use case needs an exception, file an issue with the scenario.
- The 1h reaper interval and 24h max-age are not yet operator-tunable. If
  you have a reason to lower the max-age (faster GC for ephemeral hosts),
  open an issue with the use case.
