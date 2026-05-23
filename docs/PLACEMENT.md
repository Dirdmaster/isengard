# Placement (Phase 0.14)

Native placement verbs let a compose file express "where to run this
service" without per-host hostname pinning. The controller's scheduler
takes the parsed intent, walks the enrolled fleet, and decides which
host(s) should run each replica. When a host's eligibility changes
(enroll, heartbeat label change, disconnect) the scheduler re-evaluates
the affected services and emits `placement.*` events.

This page is the operator-facing reference. The design spec is
[`docs/superpowers/specs/2026-05-11-phase-0-14-placement-verbs-and-scheduler-design.md`](superpowers/specs/2026-05-11-phase-0-14-placement-verbs-and-scheduler-design.md).

## Scope cut for 0.14

The scheduler is the **planner**: it computes which host should run a
replica and persists the decision to the `placements` table. The
existing per-host compose apply path remains the **actuator**. The
`placements` table is the integration seam: a follow-up commit
(0.14.1 / 0.15) will teach the apply path to consult `placements`
rather than rely on host-pinned semantics. Until then, `isd placement
show` is the readback for "what does the scheduler think should run
where."

## The four verbs

In a compose service, exactly zero or one of `spread`, `global`, `on`
may appear. `where:` is a modifier that combines with any of the
three OR appears alone (acts as "singleton with eligibility filter").

| Verb | Type | Meaning |
| --- | --- | --- |
| _(none)_ | n/a | Singleton. One replica, scheduler picks any eligible host. |
| `spread: N` | int >= 1 | N replicas. Prefer one-per-host; round-robin onto already-used hosts when fleet < N. |
| `global: true` | bool | One replica per eligible host. New enrollees get a replica automatically. |
| `on: <hostname>` | string | Pinned. Singleton; only ever placed on this host. |
| `where: "<selector>"` | string | Label selector. Combines with any of the above. Default: all healthy hosts eligible. |

`spread: 1` is normalized to singleton at parse time. `global: false`
errors at parse time (same meaning as no verb; remove instead).

## Selector grammar

Subset of k8s label-selector syntax, comma-separated AND:

```
selector = expr ("," expr)*
expr     = key (op value)?
op       = "==" | "!=" | "in (" v ("," v)* ")" | "notin (" v ("," v)* ")"
```

Examples:

- `where: "role==worker"` (single equality)
- `where: "role==worker, zone!=eu-west"` (AND)
- `where: "tier in (gpu, fast)"`
- `where: "preempt"` (key exists, value ignored)

`!=` and `notin` follow k8s semantics: the clause matches when the
key is **absent**. Combine with a positive clause when this is not
what you want.

## TOML and YAML examples

### `spread`

```toml
# compose.toml (flat shape; every top-level table is a service)
[web]
image = "nginx:alpine"
spread = 3
```

```yaml
# compose.yaml (compose-compat shape; services wrapper)
services:
  web:
    image: nginx:alpine
    spread: 3
```

### `global`

```toml
[node-exporter]
image = "prom/node-exporter:latest"
global = true
```

```yaml
services:
  node-exporter:
    image: prom/node-exporter:latest
    global: true
```

### `on`

```toml
[postgres]
image = "postgres:16"
on = "alice"
```

```yaml
services:
  postgres:
    image: postgres:16
    on: alice
```

### `where` (modifier)

```toml
[gpu-worker]
image = "myorg/inference:v3"
spread = 4
where = "tier==gpu, role!=control"

[telegraf]
image = "telegraf:1.30"
global = true
where = "role==worker"

[prometheus]
image = "prom/prometheus:latest"
where = "role==monitoring"   # alone = singleton on a monitoring host
```

```yaml
services:
  gpu-worker:
    image: myorg/inference:v3
    spread: 4
    where: "tier==gpu, role!=control"
  telegraf:
    image: telegraf:1.30
    global: true
    where: "role==worker"
  prometheus:
    image: prom/prometheus:latest
    where: "role==monitoring"
```

## Swarm-compat translation

Existing `deploy:` blocks keep working; the parser converts them to
the same internal model. Mixing a native verb and `deploy:` in the
**same service** is a parse error.

| Swarm input | Translates to |
| --- | --- |
| `deploy.replicas: N` | `spread: N` (with `where: None`) |
| `deploy.mode: global` | `global: true` |
| `deploy.placement.constraints: ["node.role == worker"]` | `where: "role==worker"` |
| `deploy.placement.constraints: ["node.hostname == alice"]` | `on: "alice"` |
| `deploy.placement.constraints: ["node.labels.tier == gpu"]` | `where: "tier==gpu"` |
| `deploy.placement.constraints: ["engine.labels.X == Y"]` | rejected (no Isengard equivalent) |

## Agent labels

The agent loads its labels at boot from two sources, merged in
order (env wins):

1. `/etc/isengard/agent.toml` `[labels]` table.
2. Environment variables matching `ISENGARD_LABEL_<KEY>=<VALUE>`.

See [`install/agent.toml.example`](../install/agent.toml.example) for
the file format.

Reserved keys (`host`, `hostname`, `id`) are dropped with a warning;
the controller derives those from the host identity. Invalid keys
(non-ASCII, over 63 chars, characters outside `[a-z0-9._-]+`) or
values containing `=` / `,` are dropped with a warning, not errored:
the agent keeps booting even with a typo.

Re-read on SIGHUP is future work. Today a change to labels means
`systemctl restart isd-agent`.

## Operator decisions (locked 2026-05-11)

The brainstorm flagged five places where the spec defaulted but the
operator could flip the call. All five are now locked:

| # | Decision | Locked behavior |
| --- | --- | --- |
| 1 | `spread: 1` | Normalized to `Singleton` at parse time. |
| 2 | Placement backfill at migration | Backfill rows for every existing service (state='active', replica_index=0). |
| 3 | `where:` zero match | **Auto-place** on the first eligible host (enroll OR label-change event). Overrides the earlier "stay Pending until manual redeploy" draft. |
| 4 | Disconnect grace tie-break | Drain the original replica when the host returns; keep the freshly-placed one. |
| 5 | Per-service grace period | Fleet-wide only in 0.14; per-service deferred to 0.15+. |

Decision #3 is the highest-impact: a service with a `where:` selector
that no host satisfies will stay Pending until a host enrolls or a
heartbeat brings the right label. At that moment the scheduler
auto-places the service. No operator action required.

## Failure modes

| Mode | Behavior |
| --- | --- |
| `spread: N` but eligible hosts < N | Place onto all available; emit `placement.degraded`. (No degradation emitted when round-robin still satisfies count.) |
| `where:` matches zero hosts | Stay Pending. Emit `placement.no_eligible_hosts`. Auto-place when a host becomes eligible (decision #3). |
| Pinned `on:` host disappears | Stay Pending. Emit `placement.host_gone` (or `placement.unknown_host` if it never enrolled). No relocation. |
| Mixed `spread:` + `on:` in same service | Hard parse error at `isd stack deploy`. Pick one. |
| Mixed native verb + `deploy:` in same service | Hard parse error. |
| Selector grammar error | Hard parse error at `isd stack deploy`. |

## CLI

`isd placement show` prints the controller's placement grid:

```
$ isd placement show
STACK  SERVICE         REPLICA  HOST   STATE    ASSIGNED
12     prometheus      0        alice  active   2026-05-11T09:14:02Z
12     node-exporter   0        alice  active   2026-05-11T09:14:02Z
12     node-exporter   1        bob    active   2026-05-11T09:14:02Z
12     gpu-worker      0        bob    active   2026-05-11T09:14:02Z
12     gpu-worker      1        carol  active   2026-05-11T09:14:02Z
```

Flags: `--stack-id <N>` filters to one stack; `--json` emits machine-
readable rows.

`isd placement explain` is deferred to 0.14.1: it needs a per-host
eligibility walk on the controller side that depends on the
compose -> Placement resolver wiring landing alongside the dispatch
path.

## Events

The scheduler publishes on the controller's `EventBus`. Subscribers
(journal, dashboard banner, notifiers) match on the `kind`:

| Kind | Meaning |
| --- | --- |
| `placement.created` | A new placement row landed (creation or relocation). |
| `placement.removed` | A placement row is being drained. |
| `placement.degraded` | `spread: N` couldn't be fully satisfied. |
| `placement.no_eligible_hosts` | `where:` matched zero hosts. |
| `placement.unknown_host` | `on: <name>` references a host that never enrolled. |
| `placement.host_gone` | Pinned host disconnected. |
| `placement.duplicate` | (reserved, decision-#4 surface) |
| `placement.auto_placed` | (reserved, decision-#3 audit surface) |

## End-to-end walkthrough (two-host)

Spin up `iso-alice` and `iso-bob` per the install guide. Label one
of them at agent boot:

```
# /etc/isengard/agent.toml on bob
[labels]
role = "worker"
```

Deploy a stack with `where: "role==worker"`:

```
# compose.toml
[web]
image = "nginx:alpine"
where = "role==worker"
```

```
$ isd stack deploy ./compose.toml
$ isd placement show
STACK  SERVICE  REPLICA  HOST  STATE   ASSIGNED
1      web      0        bob   active  ...
```

Relabel alice (`role=worker` env var or agent.toml edit + restart):

```
$ ssh alice systemctl restart isd-agent
$ isd placement show
STACK  SERVICE  REPLICA  HOST  STATE   ASSIGNED
1      web      0        bob   active  ...
```

Note alice doesn't pick up `web` because it's a Singleton; the
existing active placement on bob keeps it.

Switch to `global: true` and redeploy: alice now also receives a
replica via the on-enroll trigger.
