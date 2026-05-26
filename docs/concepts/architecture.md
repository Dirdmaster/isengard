---
title: Architecture
description: How the operator CLI, controller, agents, and proxy fit together.
---

# Architecture

Isengard is a Docker-native control plane. It keeps Compose as the workload format while adding fleet state, routing, backups, upgrades, and operator tooling around it.

## Pieces

`isd` is the operator CLI. It targets Docker contexts, discovers the controller, and sends stack, route, backup, restore, and upgrade requests.

The controller stores fleet state: enrolled hosts, stack definitions, route intent, secrets, certificates, and events. It coordinates agents but does not run user workloads itself.

Agents run on Docker hosts. They reconcile assigned stacks, inspect local containers, report health, and apply proxy configuration for services on that host.

The Pingora proxy runs beside the agent. It matches hostnames, terminates or forwards TLS according to route policy, and forwards traffic to local upstream containers.

## Control flow

1. The operator runs `isd stack deploy ./compose.yaml`.
2. The controller stores the desired stack.
3. The matching agent applies the Compose project on its Docker host.
4. Labels such as `isengard.expose` become route intent.
5. The agent resolves the upstream container and installs the route in Pingora.

That split keeps the controller as the source of truth while each host owns local Docker and proxy reconciliation.
