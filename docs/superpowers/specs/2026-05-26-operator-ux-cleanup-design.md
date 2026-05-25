# Operator UX Cleanup Design

## Problem

Isengard's core workflow is starting to work in live use, but the operator-facing surface still carries drift from older product eras.

The most damaging drift is routing vocabulary. The website says to use `isengard.route.public=true`, some CLI comments mention `expose.host`, while the actual product now uses `isengard.expose=<hostname>` and optional `isengard.expose.*` labels. A new operator can copy the wrong label from the docs and fail before reaching the good parts of the product.

The second gap is the onboarding spine. The getting-started docs for install, first stack, and adding a route are placeholders. The README still describes older login and credential flows. Root `isd` help is grouped and tidy, but it does not tell a new operator what to run first.

The third gap is recovery ergonomics. `isd stack doctor` already knows important expose-label fixes, but the docs do not teach it as the operator memory layer, and the fixer path is not script-friendly enough for automation.

## Goals

- Make `isengard.expose=<hostname>` the single public routing vocabulary.
- Remove stale `isengard.route.public`, `expose.host`, `isd login`, and `credentials.toml` guidance from public docs and operator-facing comments.
- Fill the onboarding spine with a verified, copyable path: install `isd`, run `isd init`, deploy a first stack, expose a service, run doctor, verify routes.
- Add a short `Start here` block to root `isd` help.
- Position `isd stack doctor` as the memory layer for label routing.
- Use public, generic examples only: popular containers and `*.isengard.app` route hostnames.
- Define follow-up behavior slices for host-aware `isd route add` and scriptable doctor fixes.

## Non-Goals

- Do not redesign routing internals in this cleanup.
- Do not change label parsing semantics in the docs PR.
- Do not introduce a new route label alias.
- Do not fill every reference page in the docs site.
- Do not build the Matt Pocock repo setup in the same PR.

## Public Vocabulary

The canonical route label is:

```yaml
labels:
  isengard.expose: jellyfin.isengard.app
```

Meaning: expose this service at `jellyfin.isengard.app`. When the agent can infer one upstream web port, this single label is enough. When the port is ambiguous, doctor asks for the correct container port and writes:

```yaml
labels:
  isengard.expose: qbittorrent.isengard.app
  isengard.expose.port: "8069"
```

Optional labels stay advanced:

```yaml
labels:
  isengard.expose.tls: acme
  isengard.expose.adapter: none
  isengard.expose.health: /healthz
  isengard.expose.auth: none
```

Named rules stay an escape hatch:

```yaml
labels:
  isengard.expose.web: grafana.isengard.app
  isengard.expose.web.port: "8080"
  isengard.expose.admin: admin.isengard.app
  isengard.expose.admin.port: "9090"
```

Examples must avoid personal domains, private hostnames, and real home-service names. Use popular containers such as `nginx`, `whoami`, `jellyfin`, `grafana`, `prometheus`, and `qbittorrent`. Every example route hostname should live under `isengard.app`, such as `whoami.isengard.app`.

## Phase A: Docs And Help Strike

Phase A is one tight PR with no routing or controller behavior changes. It may change docs, CLI help output, and test guardrails.

### Website

Update `website/content/index.md` so the routing feature card shows the real label and the current promise:

- `isengard.expose=<hostname>` is enough for the common case.
- The agent infers the upstream port when it is safe.
- Doctor handles the cases worth asking about.

### README

Rewrite the operator CLI section to match the current model:

- Docker context selects the host.
- `isd init` discovers or creates the controller on that context.
- Controller-backed commands discover the controller container by `io.isengard.role=controller`.
- SSH contexts use the existing Docker SSH transport and a local forward.
- No `isd login` or `credentials.toml` flow is presented as current behavior.

### Getting Started

Fill these files:

- `docs/getting-started/install.md`
- `docs/getting-started/first-stack.md`
- `docs/getting-started/adding-a-route.md`

The path should be short and executable:

1. Install `isd`.
2. Confirm Docker context.
3. Run `isd init`.
4. Deploy a small compose stack.
5. Add `isengard.expose=<hostname>`.
6. Run `isd stack doctor`.
7. Check `isd route ls` and `curl -I https://<hostname>`.

### Reference Docs

Fill the first useful version of:

- `docs/reference/cli/route.md`
- The `doctor` section of `docs/reference/cli/stack.md`

These pages should not become exhaustive clap dumps. They should explain the mental model, list the high-value commands, and link to the getting-started pages.

### Root Help

Add a compact `Start here` block to `isd` root help above the grouped command list:

```text
Start here
  isd init              bootstrap controller and first agent
  isd stack deploy      deploy a compose stack
  isd stack doctor      check expose labels before routing
  isd route ls          inspect installed public routes
```

The help renderer already has tests that assert command grouping. Extend them to assert the new block.

### Stale Vocabulary Guard

Add a minimal docs guard that fails when public docs reintroduce known stale vocabulary:

- `isengard.route.public`
- `expose.host`
- `isd login`
- `credentials.toml`
- personal route domains in public examples

The guard should allow historical specs and release notes to keep old terms when needed. Scope it to current public docs, website content, README, and operator-facing reference pages.

## Phase B: Host-Aware Route Wizard

Phase B is a follow-up behavior PR.

Current `isd route add` lists containers from the Docker context before resolving the target route host. In multi-host fleets, that is confusing because the prompt can show the wrong host's containers.

The wizard should resolve host first:

1. Open a controller session for the selected Docker context.
2. Resolve `--host-id`, `--host`, or single-host default before showing containers.
3. List candidate containers for that host from controller inventory when available.
4. Use rich container ports from `containers_rich` when available.
5. Fall back to local Docker inspection only when the selected host is the current context host and inventory lacks port data.
6. Print the selected host in the prompt so the operator knows what they are routing.

This makes the imperative route path align with the live fixes we just made: select the correct host, select the correct service ref, select the correct port.

## Phase C: Scriptable Doctor Fixes

Phase C is another behavior PR.

`isd stack doctor --fix` should remain pleasant in a TTY, but automation needs a noninteractive path.

Proposed shape:

```sh
isd stack doctor media --fix --set qbittorrent=qbittorrent.isengard.app:8069 --yes
```

Rules:

- `--set service=hostname` writes `isengard.expose` only.
- `--set service=hostname:port` writes `isengard.expose` and `isengard.expose.port`.
- Multiple `--set` values are allowed.
- `--yes` may skip final confirmation only when all required fix inputs are provided.
- Missing required inputs still error in non-TTY mode.

Add doctor checks for common routing mistakes:

- invalid hostname syntax
- duplicate expose hostnames in one compose document
- invalid `isengard.expose.tls`
- invalid `isengard.expose.adapter`

## Testing

Phase A:

- Root help unit tests cover the `Start here` block.
- Docs stale-vocabulary guard fails on current public docs only.
- `cargo test -p isd help_render::tests`
- The docs guard command added by the PR.

Phase B:

- Unit tests for host-first route wizard resolution helpers.
- Tests for inventory port selection with rich port rows.
- Regression covering a two-host fleet where the wizard does not show containers from the wrong host.

Phase C:

- Doctor parser tests for `--set service=hostname[:port]`.
- Fixer tests for hostname-only and hostname-plus-port writes.
- Non-TTY tests proving missing input errors instead of prompting.
- New doctor check tests for invalid hostnames, duplicate hosts, invalid TLS, and invalid adapter values.

## Rollout

Ship Phase A first. It fixes the public story and gives the product a coherent first-run path.

Ship Phase B second. It removes a real multi-host footgun from `isd route add`.

Ship Phase C third. It makes doctor a reliable automation target and tightens routing validation.

After these three phases, set up the Matt Pocock repo context files so `to-issues`, `triage`, `improve-codebase-architecture`, and `diagnose` can operate with shared domain language.
