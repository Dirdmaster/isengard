# Isengard

> **Note (2026-04-29):** Isengard is being rewritten from the ground up as a container management platform: single binary with controller and agent modes, plugin model, multi-host support, web dashboard. The Rust rewrite is in progress on the `feat/platform-rewrite` branch. The Go implementation below remains the current stable release; it stays in [`legacy-go/`](./legacy-go/) on the rewrite branch as a reference. See [`docs/superpowers/specs/2026-04-29-platform-pivot-design.md`](./docs/superpowers/specs/2026-04-29-platform-pivot-design.md) for the design.
>
> **Phase 14 (2026-05-05) — BREAKING:** the shared `ISENGARD_TOKEN` bearer secret has been replaced with an internal CA + per-agent mTLS + short-lived enrollment tokens. See the [Rust rewrite quick start](#rust-rewrite-quick-start-controller--agent) below and [`docs/RELEASE_NOTES_PHASE_14.md`](./docs/RELEASE_NOTES_PHASE_14.md) for the migration recipe.

## Rust rewrite quick start (controller + agent)

The rewrite splits Isengard into a **controller** (one per fleet, exposes a dashboard + gRPC) and **agents** (one per Docker host, talks to the controller over mTLS). All auth is bootstrapped from short-lived enrollment tokens minted by the controller; there is no long-lived shared secret.

Enrollment uses a Docker Swarm-style join command: `controller token mint` prints a copy-pasteable `docker run` block with the token and the controller's CA root pre-baked in. No manual `ca export`, no side files.

```bash
# 1. Start the controller. State (CA, sqlite, certs) lives in /var/lib/isengard.
docker run -d --name iso-controller \
  -p 9417:9417 \
  -p 8080:8080 \
  -e ISENGARD_PUBLIC_ADDR=controller.example.com:9417 \
  -v iso-controller-state:/var/lib/isengard \
  ghcr.io/dirdmaster/isengard:next controller

# 2. Mint an enrollment token. The output is a complete `docker run`
#    command for the agent, with the token and CA pre-baked in.
docker exec iso-controller isengard controller token mint --role agent
```

The output looks like this; paste it on the agent host:

```text
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
      agent --controller https://controller.example.com:9417 --state-dir /var/lib/isengard

Token expires at 2026-05-07T11:30:00Z. Mint a new one with:
    isengard controller token mint --role agent
```

For a single-host setup with both services in the same compose stack, see [`docker/`](./docker/) for a ready-to-go `compose.yaml`.

The agent enrolls on first boot (exchanges the token for an mTLS cert bundle, persists it to `state-dir/certs/`), then uses mTLS for every subsequent RPC. Certs auto-renew at 50% TTL (default 30-day TTL = renews every ~15 days). The token is consumed immediately and cannot be reused.

To remove an agent:

```bash
# Revoke its cert (immediately rejects further RPCs from that host).
docker exec iso-controller isengard controller agent revoke <host_id>

# List agents to find the host_id.
docker exec iso-controller isengard controller agent list
```

You can also mint tokens and revoke hosts from the dashboard at `http://controller:8080` (Settings → Enrollment, and per-host Revoke buttons on the inspector).

For CI / scripts that need just the bare token (no join block), pass `--format token`:

```bash
docker exec iso-controller isengard controller token mint --role agent --format token
```

This reproduces the pre-Phase-15 behaviour. You'll need to handle CA distribution yourself in that path; see [`docker/README.md`](./docker/README.md#advanced-bare-token-output).

## Operator CLI (`isd`)

`isd` is the terminal companion to the dashboard. After the controller is up, run `cargo build -p isd --release` (or `just isd-build`) and `isd login https://controller.local:9417` once: the CLI captures the controller's TLS fingerprint, stores it alongside an API token in `~/.config/isengard/credentials.toml`, and pins both for subsequent calls. Day-to-day commands: `isd ps` (list stacks + services, `--json` for scripts), `isd open <stack>` (launch the stack's primary host in your default browser), and `isd logs <stack>/<service> -f` (stream logs over the controller WebSocket). More subcommands (`apply`, `forward`, `shell`) ship in v0.3c/d once the compose-store lands.

---

# Isengard (legacy Go)

[![CI](https://img.shields.io/github/actions/workflow/status/dirdmaster/isengard/ci.yml?branch=main&label=CI&style=flat)](https://github.com/dirdmaster/isengard/actions/workflows/ci.yml)
[![Docker](https://img.shields.io/github/actions/workflow/status/dirdmaster/isengard/docker.yml?label=Docker&style=flat)](https://github.com/dirdmaster/isengard/actions/workflows/docker.yml)
[![Go](https://img.shields.io/github/go-mod/go-version/dirdmaster/isengard?style=flat)](https://go.dev)
[![License](https://img.shields.io/github/license/dirdmaster/isengard?style=flat)](LICENSE)
[![GHCR](https://img.shields.io/badge/ghcr.io-dirdmaster%2Fisengard-blue?style=flat)](https://ghcr.io/dirdmaster/isengard)

Lightweight Docker container auto-updater. Watches running containers for newer images and recreates them in-place, preserving ports, volumes, networks, labels, and restart policies.

## Features

- **Registry-first detection** checks remote digests via HEAD requests (~50ms per image) and only pulls when an update exists
- **Zero configuration** out of the box. Mount the Docker socket and go. Every running container is watched by default
- **Faithful recreation** preserves the full container config across updates: ports, volumes, networks, env vars, labels, resource limits
- **~3 MB image** built from scratch with a static Go binary, no runtime dependencies

## Quick start

```bash
docker run -d \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/dirdmaster/isengard
```

Or with Docker Compose:

```yaml
services:
  isengard:
    image: ghcr.io/dirdmaster/isengard
    restart: unless-stopped
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
```

## Configuration

All configuration is via environment variables.

| Variable | Default | Description |
|----------|---------|-------------|
| `ISENGARD_INTERVAL` | `30m` | Check interval (Go duration format) |
| `ISENGARD_WATCH_ALL` | `true` | Watch all containers; set `false` for opt-in mode |
| `ISENGARD_RUN_ONCE` | `false` | Run a single check cycle, then exit |
| `ISENGARD_CLEANUP` | `true` | Remove old images after a successful update |
| `ISENGARD_STOP_TIMEOUT` | `30` | Seconds to wait for graceful container stop |
| `ISENGARD_LOG_LEVEL` | `info` | Minimum log level: `debug`, `info`, `warn`, `error` |
| `ISENGARD_SELF_UPDATE` | `false` | Allow Isengard to update its own container |

## Filtering containers

**Watch-all mode** (default): every running container is watched. Exclude specific containers with a label:

```yaml
labels:
  - isengard.enable=false
```

**Opt-in mode**: set `ISENGARD_WATCH_ALL=false` and label only the containers you want watched:

```yaml
labels:
  - isengard.enable=true
```

## Private registries

Isengard checks remote digests directly via the registry v2 API (~50ms per image). For private registries, mount your Docker credentials so Isengard can authenticate these requests:

```yaml
volumes:
  - /var/run/docker.sock:/var/run/docker.sock
  - ~/.docker/config.json:/root/.docker/config.json:ro
```

Without the mount, digest checks on private images will fail and Isengard falls back to pulling through the Docker daemon (which uses the host's own auth). The fallback works fine but skips the fast digest check.

Supports Docker Hub, GHCR, ECR, Quay, and self-hosted registries.

## How it works

1. Lists all running containers (filtered by mode and labels)
2. For each container, sends a HEAD request to the registry to get the remote digest (~50ms)
3. Compares the remote digest against the local image's `RepoDigests`
4. If the digest differs, pulls the new image and recreates the container with the same configuration
5. If the digest check fails (auth issues, unsupported registry), falls back to pull-and-compare by image ID

## Self-update

Set `ISENGARD_SELF_UPDATE=true` to let Isengard update its own container when a newer image is available. The self-update always runs last, after all other containers have been processed.

When a new image is detected, Isengard recreates its own container using the same stop/remove/create/start sequence it uses for every other container. Use `restart: unless-stopped` in your compose file so Docker restarts the new container if needed.

Isengard identifies its own container using multiple detection methods that work across Docker Compose, Swarm, cgroup v1, and cgroup v2 environments. No extra labels or configuration are needed beyond enabling the flag.

## Building from source

```bash
go install github.com/dirdmaster/isengard@latest
```

Or build the Docker image:

```bash
docker build -t isengard .
```

## Contributing

1. Fork and clone, then run `bun install` to set up git hooks via lefthook
2. Make sure you have Go 1.25+ installed
3. Lefthook handles `go fmt`, `go vet`, `golangci-lint`, and `go build` on pre-commit; tests run on pre-push
4. Use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `test:`

See [open issues](https://github.com/dirdmaster/isengard/issues) for things to work on.

## License

[MIT](LICENSE)
