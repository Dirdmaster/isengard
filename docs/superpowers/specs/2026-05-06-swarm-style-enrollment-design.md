# Swarm-style enrollment join command (design)

Status: proposed
Phase: post-14 polish
Author: 2026-05-06

## Problem

`isengard controller token mint --role agent` prints a bare token on stdout.
The operator then has to assemble the rest of the agent invocation by hand:
controller URL, CA pinning, env vars, image tag, platform flag, state-dir
volume, docker.sock mount. Phase 14's mTLS pivot made it worse: the agent
now needs the controller's CA root cert inline (or by path) before the very
first RPC.

In compose-driven flows that means four commands: start the controller,
`docker exec controller ca export > ca.pem`, `docker exec controller token
mint`, then `docker run` with `-v ./ca.pem:/etc/...:ro -e
ISENGARD_CONTROLLER_CA_PEM_PATH=...` plus everything else.

Docker Swarm solved exactly this with `docker swarm join`: `swarm init`
prints a copy-pasteable `docker swarm join --token ... addr:port` command.
That UX is the bar.

## Goal

`controller token mint --role agent` returns a complete, copy-pasteable
`docker run` command. The operator pastes it on the agent host, the agent
boots, enrolls, and is online. No side files, no extra commands.

## Design

### CA pinning: base64 in env var

Today the agent supports two trust sources: `ISENGARD_CONTROLLER_CA_PEM_PATH`
(file path) and `ISENGARD_CONTROLLER_CA_PEM` (inline PEM). The path form
forces a side-file; the inline form is multiline PEM, which docker `-e`
flags pass through but most shells mangle.

Add a third: `ISENGARD_CONTROLLER_CA_PEM_BASE64`. Single-line, no escaping
needed. The agent decodes it at startup and feeds the resulting bytes to
the same `Certificate::from_pem` path as the other two variants. Resolution
order becomes: `_PATH` env > `_BASE64` env > `_PEM` env > caller-supplied
path > caller-supplied inline > native roots.

Backwards compat: existing `_PEM` and `_PEM_PATH` flows keep working
unchanged.

### Join-command output

`controller token mint --role agent` emits, on stdout:

```
Token minted (expires in 15m).

To enroll an agent, run on the host where you want it to live:

    docker run -d \
      --name iso-agent \
      --restart unless-stopped \
      --platform linux/amd64 \
      -v iso-agent-state:/var/lib/isengard \
      -v /var/run/docker.sock:/var/run/docker.sock \
      -e ISENGARD_ENROLL_TOKEN=01HX... \
      -e ISENGARD_CONTROLLER_CA_PEM_BASE64=LS0t... \
      ghcr.io/dirdmaster/isengard:next \
      agent --controller https://controller.local:9417 --state-dir /var/lib/isengard

Token expires at 2026-05-07T11:30:00Z. Mint a new one with:
    isengard controller token mint --role agent
```

Plain ASCII (no boxes), backslash line continuations so the operator can
paste the whole block. Token + base64 are both single-line. The "expires"
footer doubles as a stale-token reminder.

### Controller URL detection

Three sources, first non-empty wins:

1. `--public-addr <host:port>` CLI flag.
2. `ISENGARD_PUBLIC_ADDR` env var.
3. Default: `controller.local:9417` (matches Bonjour/mDNS norms; works
   on most LAN setups out of the box).

The listen address (`ISENGARD_LISTEN`) is intentionally *not* the
fallback: a controller bound to `0.0.0.0:9417` cannot be dialed at that
address, and a controller bound to `127.0.0.1` certainly cannot from
another host. Better to make the operator declare the public addr
explicitly than to print a broken command.

Output always uses `https://` (Phase 14 controllers serve TLS).

### Image tag

The mint output bakes in `ghcr.io/dirdmaster/isengard:next` to match
README quick start. Override with `--image <ref>` when needed (custom
registries, pinned tags, internal mirrors).

### Format flag

`--format text` (default) prints the join block. `--format token` prints
just the bare token (current behavior, kept for scripts and CI).

### Compose flow

`docker/compose.yaml` ships a controller + agent stack. The agent service
reads `ISENGARD_CONTROLLER_CA_PEM_BASE64` from a `.env` file populated by
a one-shot `controller-export` job (or by hand from `docker exec controller
isengard controller ca export | base64`). No more bind-mounted ca.pem.

For the same-compose-network case the controller advertises
`https://controller:9417` via `--public-addr controller:9417` baked into
the controller service command.

### Backwards compat

- `_PEM` and `_PEM_PATH` env vars: unchanged.
- `controller ca export`: unchanged; still useful for advanced flows
  (e.g. baking the CA into a custom image, distributing via config
  management).
- `--format token` reproduces the pre-Phase-14 mint output exactly.

## Non-goals

- Auto-detecting the controller's reachable address from outside (DNS,
  reverse proxy, Tailscale). The `--public-addr` flag is the operator's
  declared answer.
- Encoding the join command for non-Docker runtimes (Kubernetes,
  systemd-nspawn). Docker is the only supported agent runtime today.
- mDNS or DNS-SD service advertisement. `controller.local` is just a
  default string; it works if the operator sets up `.local` routing,
  but Isengard does not publish it.

## Open questions

- Should the join block include `--memory` / `--cpus` resource limits?
  Deferred: the agent itself is light, and prescribing limits would
  encode environment-specific guesses. Operators can append flags.
- Should we emit a JSON variant for tooling? Yes, as `--format json`,
  in a follow-up. Today only text + token are shipped.

## Files touched

- `crates/isengard/src/main.rs`: new flags, new mint output path.
- `crates/isengard-agent/src/enroll.rs`: `_BASE64` env var support.
- `docker/compose.yaml`: rewritten to use base64 env var.
- `docker/README.md`: new compose quick start.
- `README.md`: replace manual recipe with join-command flow.
- `docs/RELEASE_NOTES_PHASE_14.md`: cross-link the new flow.
- `docs/RELEASE_NOTES_ENROLLMENT_UX.md`: this change.
