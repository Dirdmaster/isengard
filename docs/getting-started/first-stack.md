---
title: First Stack
description: Deploy a small tech stack and expose it with one label.
---

# First Stack

Deploy a small stack first. Replace it after the workflow makes sense.

## Create `compose.yaml`

```yaml
services:
  whoami:
    image: traefik/whoami:v1.10
    labels:
      isengard.expose: whoami.isengard.app
```

The single `isengard.expose` label asks Isengard to route `whoami.isengard.app` to the service. The agent infers the upstream port when the container has one clear web port.

## Check the file

```sh
isd stack doctor ./compose.yaml
```

If doctor reports a missing or ambiguous label, run the fixer:

```sh
isd stack doctor ./compose.yaml --fix
```

## Deploy it

```sh
isd stack deploy ./compose.yaml
```

Inspect the running stack:

```sh
isd stack ls
isd ps
isd route ls
```

Point DNS for `whoami.isengard.app` at the Docker host running the agent, then verify the public route:

```sh
curl -I https://whoami.isengard.app
```

Next: learn route options in [Adding A Route](/getting-started/adding-a-route).
