---
title: isd stack
description: Reference for the `isd stack` subcommand.
---

# `isd stack`

Deploy, inspect, audit, and edit compose-backed stacks.

## List stacks

```sh
isd stack ls
```

Shows one row per stack with service count, host count, aggregate state, and source.

## Deploy a stack

```sh
isd stack deploy ./compose.yaml
```

`deploy` sends the compose file to the controller. The controller stores it, reconciles it through the selected agent, and pushes route config when labels produce routes.

Run a dry plan first:

```sh
isd stack diff observability ./compose.yaml
```

## Check a stack with doctor

```sh
isd stack doctor ./compose.yaml
```

Doctor audits a local compose file or a named controller stack. It catches missing `isengard.expose` labels, invalid expose ports, and ambiguous upstream ports before they become broken routes.

Print the route label reference:

```sh
isd stack doctor labels
```

Apply interactive fixes:

```sh
isd stack doctor ./compose.yaml --fix
```

## Inspect services in a stack

```sh
isd stack ps observability
```

Use `isd ps` for the fleet-wide container view.
