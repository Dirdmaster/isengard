---
title: Install
description: Install isd and bootstrap the first controller and agent.
---

# Install

Install `isd` on the machine you use to operate Docker hosts.

## Install the CLI

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Weavers-Engineering/Isengard/releases/latest/download/isd-installer.sh | sh
```

Other install paths:

| Channel | Command |
|---|---|
| Homebrew | `brew install weavers-engineering/isengard/isd` |
| Docker | `docker run --rm ghcr.io/weavers-engineering/isd:latest --version` |
| Cargo | `cargo install --git https://github.com/Weavers-Engineering/Isengard isd` |

## Choose a Docker context

`isd` uses Docker contexts as its target selector and credential source.

```sh
docker context ls
docker context use default
```

For a remote host, create an SSH-backed Docker context first:

```sh
docker context create prod --docker "host=ssh://deploy@example-host"
isd --context prod ps
```

## Bootstrap Isengard

Run `isd init` against the context that should host the controller and first agent.

```sh
isd init
```

The command creates the controller and first agent on that Docker host. Controller-backed commands discover the controller container by `io.isengard.role=controller`.

Verify the cluster:

```sh
isd ps
isd hosts ls
```

Next: deploy your first stack in [First Stack](/getting-started/first-stack).
