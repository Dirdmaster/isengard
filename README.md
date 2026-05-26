# Isengard

Isengard is a small self-hosting control plane for deploying Docker Compose stacks, routing public hostnames, and operating a fleet from the `isd` CLI.

- Bootstrap a controller and first agent with one command.
- Keep Compose as the source of truth for stacks and routes.
- Expose services with one `isengard.expose` label.
- Use `isd stack doctor` to catch routing mistakes before deploy.
- Run across local or SSH-backed Docker contexts without a separate login flow.

## Install

Install `isd` on the machine you use to operate Docker hosts.

| Channel | Command |
|---|---|
| Shell installer | `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Weavers-Engineering/Isengard/releases/latest/download/isd-installer.sh | sh` |
| Homebrew | `brew tap weavers-engineering/isengard && brew install isd` |
| Docker | `docker run --rm ghcr.io/weavers-engineering/isd:latest --version` |
| Cargo | `cargo install --git https://github.com/Weavers-Engineering/Isengard isd` |

## Five-minute quick start

Bootstrap Isengard on your current Docker context:

```sh
isd init
```

Create `compose.yaml`:

```yaml
services:
  whoami:
    image: traefik/whoami:v1.10
    labels:
      isengard.expose: whoami.isengard.app
```

Check the stack before deploying:

```sh
isd stack doctor ./compose.yaml
```

Deploy it:

```sh
isd stack deploy ./compose.yaml
```

List the installed routes:

```sh
isd route ls
```

Verify the route through the proxy with an explicit Host header:

```sh
curl -I -H 'Host: whoami.isengard.app' http://127.0.0.1
```

For a real public route, point `whoami.isengard.app` at the host running the agent and use `curl -I https://whoami.isengard.app`.

## Compose example

This is the smallest useful stack for testing route discovery:

```yaml
services:
  whoami:
    image: traefik/whoami:v1.10
    labels:
      isengard.expose: whoami.isengard.app
```

The agent infers the upstream port when the container has one clear web port. Add `isengard.expose.port` when a service exposes multiple candidate ports.

## Architecture

The operator uses `isd` from a laptop or admin host. `isd` targets Docker contexts, discovers the controller, and sends stack, route, backup, and upgrade commands.

The controller stores fleet state, stack definitions, route intent, certificates, and events. It coordinates agents but does not run user containers itself.

Agents run on Docker hosts. They reconcile assigned stacks, inspect local containers, report health, and apply proxy configuration for services on that host.

The Pingora proxy runs beside the agent and serves public routes. It matches hostnames, terminates or forwards TLS according to route policy, and forwards traffic to local upstream containers.

## Maturity

Isengard is showcase-ready: the core operator path is intended to be demoable and understandable. It is not yet hardened for unattended production upgrades.

## Docs

- [Install](./docs/getting-started/install.md)
- [First stack](./docs/getting-started/first-stack.md)
- [Adding a route](./docs/getting-started/adding-a-route.md)
- [`isd route`](./docs/reference/cli/route.md)
- [`isd stack`](./docs/reference/cli/stack.md)
- [`isd backup`](./docs/reference/cli/backup.md)
- [`isd upgrade`](./docs/reference/cli/upgrade.md)
- [Architecture](./docs/concepts/architecture.md)
- [Stack manifest](./docs/reference/manifest/stack-toml.md)
- [Install directory notes](./install/README.md)

## License

[MIT](LICENSE)
