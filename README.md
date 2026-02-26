# Isengard

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
