---
title: Run the local showcase demo
description: Start a local Isengard control plane, deploy whoami, and verify routing through the local proxy.
---

# Run the local showcase demo

Use this recipe when you want a repeatable local path for screenshots, videos, or a first hands-on review. It starts a temporary controller and agent, deploys a routed `whoami` stack, and verifies the route through `127.0.0.1` with a Host header.

This is a local demo, not production guidance. Run it against a disposable Docker context, not a production host.

## Prerequisites

- Docker with access to the host Docker socket.
- `isd` installed and on `PATH`.
- `just` installed and on `PATH`.
- Free ports on the Docker context: `80`, `443`, `19417`, and `19418`.

The demo mounts `/var/run/docker.sock` into the agent by default. For a nonstandard Docker host socket, export it first:

```sh
export DOCKER_SOCK="$HOME/.orbstack/run/docker.sock"
```

## Run it

```sh
just demo
```

The recipe cleans prior demo state, pulls the published controller and agent images, starts the local control plane, deploys `examples/showcase/compose.yaml`, waits for `whoami.isengard.app` to appear in `isd route ls`, and curls the local proxy.

## Verify manually

```sh
isd stack ls
isd route ls
curl -H 'Host: whoami.isengard.app' http://127.0.0.1/
```

The curl should return the `traefik/whoami` response.

## Clean up

```sh
just demo-clean
```

`demo-clean` removes the demo controller, agent, state volumes, extracted CA file, and the `showcase-whoami` container.

## Troubleshooting

If Docker is unavailable, start Docker Desktop or your Docker runtime and rerun `just demo`.

If the Docker socket is not mounted at `/var/run/docker.sock`, set `DOCKER_SOCK` before running the demo.

If a required port is already in use, stop the service using it or run the demo on a Docker context with free ports.

If route verification fails, inspect the controller and agent state:

```sh
isd route ls
docker logs isengard-agent
curl -H 'Host: whoami.isengard.app' http://127.0.0.1/
```
