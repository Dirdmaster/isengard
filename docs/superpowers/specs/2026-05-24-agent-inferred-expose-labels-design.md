# Agent-Inferred Expose Labels Design

## Problem

Isengard already supports `isengard.expose*` labels, but the practical user experience still asks operators to remember too much. The current label parser accepts a hostname label plus optional labels for port, TLS, health, adapter, and auth. The controller currently treats a missing port as `80` during label ingestion.

That is too close to the painful part of Traefik: it works well once labels are correct, but the operator has to remember which labels are required and which defaults apply.

The desired shape is Traefik-like in the good way:

```yaml
labels:
  isengard.expose: plex.vallee.casa
```

One label should be enough for the common case. Doctor should remember the rest.

## Goals

- Make `isengard.expose=<hostname>` the canonical happy path.
- Infer the upstream port on the agent that runs the container.
- Keep optional labels for explicit overrides and ambiguous cases.
- Make `isd stack doctor` validate and repair the label contract.
- Add discoverable docs through CLI help so users do not memorize labels.
- Preserve existing explicit labels and UI routes.

## Non-Goals

- Do not import Traefik labels automatically in this change.
- Do not require host-published ports for reverse proxy routing.
- Do not remove support for `isengard.expose.port` or named expose rules.
- Do not make the controller reach into remote Docker daemons.

## Label Contract

### Canonical Rule

The minimum label is:

```yaml
labels:
  isengard.expose: plex.vallee.casa
```

Meaning: expose this container or compose service at `plex.vallee.casa`. If no port override exists, the agent infers the upstream container port from local Docker inspect data.

The controller must not silently convert missing port to `80`. Missing port means unresolved until the agent supplies an inferred port or doctor writes an explicit override.

### Optional Overrides

Optional labels remain supported:

```yaml
labels:
  isengard.expose: plex.vallee.casa
  isengard.expose.port: "32400"
  isengard.expose.tls: acme
  isengard.expose.health: /identity
  isengard.expose.adapter: none
  isengard.expose.auth: none
```

Defaults:

| Label | Default |
| --- | --- |
| `isengard.expose.tls` | `acme` |
| `isengard.expose.adapter` | `none` |
| `isengard.expose.auth` | `none` |
| `isengard.expose.health` | unset |

### Named Rules

Named rules remain supported for services with multiple hostnames or ports:

```yaml
labels:
  isengard.expose.web: app.vallee.casa
  isengard.expose.web.port: "8080"
  isengard.expose.admin: admin.vallee.casa
  isengard.expose.admin.port: "9090"
```

The unnamed rule is the normal path. Named rules are an advanced escape hatch.

## Runtime Flow

The agent owns port inference because it is the data-plane process on the actual Docker host.

```text
container starts on lausanne
agent inspects container labels and ports
agent converts hostname-only labels into route intents with inferred ports
controller stores label-source routing rules
controller pushes proxy config back to the same host
agent applies proxy route to the local container
```

The controller remains the control plane. It stores routing rules, resolves host ownership, and distributes config. It does not infer ports by guessing defaults.

The agent remains the data plane. DNS points traffic to the agent host. The local reverse proxy handles the request and routes to the local container port.

## Port Inference

Port inference uses local Docker inspect data for the selected container.

Agent inference is conservative. It emits a complete route only when exactly one candidate remains after deterministic inspection. If multiple candidates remain, it reports an unresolved route intent and doctor asks the operator to choose a port.

Rules:

| Case | Behavior |
| --- | --- |
| Exactly one TCP candidate | Use it automatically |
| Multiple candidates with one common web port | Agent reports unresolved; doctor recommends the common web port and asks before writing |
| Multiple web-like candidates | Ask explicitly and write `isengard.expose.port` |
| No ports from inspect or compose | Report an incomplete hostname-only label |
| Host network with image `EXPOSE` only | Use image-declared exposed port when unambiguous |
| Invalid `.port` value | Doctor warns and offers to rewrite it |

The key regression target is Plex: `isengard.expose=plex.vallee.casa` should resolve to port `32400`, not `80`, and should not require the user to remember `isengard.expose.port`.

## Doctor UX

Doctor becomes the label memory layer.

Read-only mode:

```text
isd stack doctor servarr
```

Expected shape:

```text
services.plex: ok
  isengard.expose=plex.vallee.casa
  inferred port: 32400 from agent inspect

services.qbittorrent: warning
  multiple candidate ports: 8080, 6881
  fix: add isengard.expose.port=8080

services.overseerr: warning
  no isengard.expose label
  fix: add hostname label
```

Fix mode asks in human terms:

```text
Expose services.overseerr through which hostname?
Which port should qbittorrent use? 8080, 6881
```

Doctor writes the minimum labels needed.

No ambiguity:

```yaml
labels:
  isengard.expose: overseerr.vallee.casa
```

Ambiguity:

```yaml
labels:
  isengard.expose: qbittorrent.vallee.casa
  isengard.expose.port: "8080"
```

Doctor should validate both compose source and runtime/control-plane state. A label that should create a route but did not should be reported as stale ingestion or unresolved inference, not as healthy.

## CLI Documentation

Add a discoverable label reference to the CLI.

Suggested command:

```text
isd stack doctor labels
```

Also include the short form in `isd stack doctor --help`.

Reference content:

```text
Required:
  isengard.expose=<hostname>

Optional:
  isengard.expose.port=<container-port>
  isengard.expose.tls=acme|edge|manual
  isengard.expose.adapter=none|tailscale|cf-tunnel
  isengard.expose.auth=none|...
  isengard.expose.health=/path

Named:
  isengard.expose.<name>=<hostname>
  isengard.expose.<name>.port=<container-port>
```

The docs should say: start with `isengard.expose=<hostname>`. Add optional labels only when doctor tells you to or when you want a non-default behavior.

## Migration And Coexistence

Existing behavior must keep working.

| Existing state | Behavior |
| --- | --- |
| Explicit `isengard.expose.port` | Keep it, validate it, do not remove it |
| Hostname-only labels | Start working once agent inference lands |
| UI routes | Continue working |
| UI route with same hostname as label route | Label-source route wins on the same host |
| Traefik labels | Do not import automatically |
| Compose with host-published ports but no expose label | Doctor offers to add hostname-only expose label |
| Generated `.port` from current doctor | Valid and still supported |

Traefik labels can become a future doctor check: "unsupported Traefik labels detected; run migration helper." That is not part of this design.

## Components

| Unit | Responsibility |
| --- | --- |
| `isengard-core::labels` | Parse hostname-only and optional labels into structured route intents without defaulting missing port to `80` too early |
| Agent label watcher | Inspect selected container, infer port, attach inferred route data to label reports |
| Controller routing ingest | Store complete label-source routes and reject unresolved missing-port intents |
| Doctor checks | Validate source compose plus runtime/controller route state against the label contract |
| Doctor fixers | Add hostname labels, repair invalid port labels, write `.port` only when ambiguity requires it |
| CLI help/docs | Show the minimal label contract and optional overrides |

## Testing

Required tests:

- Core parser: hostname-only, explicit port, named rules, invalid port.
- Agent: hostname-only Plex-like container infers `32400` from inspect.
- Agent: host-networked container uses image-declared exposed port when unambiguous.
- Controller: missing unresolved port does not become `80`.
- Controller: complete inferred route inserts a label-source routing rule.
- Doctor: missing expose label offers hostname-only fix.
- Doctor: ambiguous ports prompts and writes `.port`.
- Doctor: existing explicit `.port` is preserved and validated.
- Doctor: stale label route is reported when a label exists but no route was created.
- CLI docs: label reference renders required, optional, and named label sections.

## Acceptance Criteria

- A compose service with only `isengard.expose=plex.vallee.casa` can create a route to Plex on port `32400` through agent-side inference.
- `isd stack doctor` no longer treats hostname-only labels as incomplete when the agent can infer one port.
- `isd stack doctor --fix` writes hostname-only labels when inference is safe.
- `isd stack doctor --fix` writes `isengard.expose.port` only for ambiguity, invalid existing port, or explicit user choice.
- Missing inferred port never silently becomes `80` unless `80` is the inferred port.
- Existing explicit labels and UI routes continue to work.
- The label reference is discoverable from the CLI.
