# Agent-Owned Ingress Network Design

**Status:** Draft, 2026-05-25
**Context:** Follow-up to Phase 8 proxy networking after runtime evidence showed routed containers can be discovered without being reachable by Pingora.

## Problem

Isengard currently treats routing rules and container reachability as separate concerns. A route can exist because the controller learned about it from labels, the dashboard, or a stack manifest, but the target container may not be attached to the network the agent's Pingora proxy can reach.

That makes the operator responsible for remembering to attach routed containers to `isengard-proxy`. When they forget, the proxy resolves no usable IP, falls back to `127.0.0.1`, healthchecks evict the upstream, and clients see a 503. The root issue is not healthchecking; it is that route creation did not reconcile the network needed to serve the route.

## Goal

Creating or discovering a route should make the container reachable by the local Isengard proxy whenever the runtime supports it.

The route lifecycle becomes:

1. Controller declares a route.
2. Agent receives the route in `ProxyConfig`.
3. Agent reconciles runtime networking for the upstream container.
4. Agent resolves the upstream IP from the Isengard ingress fabric.
5. Proxy installs the route only with a meaningful upstream endpoint, or marks it unresolved with a clear reason.

## Ingress Fabric

For Docker, the initial ingress fabric is the existing external bridge network named `isengard-proxy`.

Conceptually this is not an operator-managed Traefik-style opt-in network. It is an Isengard-owned per-host ingress network:

- The agent ensures the network exists before applying route config.
- The agent ensures routed bridge-networked containers are attached to it.
- The agent prefers the container's `isengard-proxy` IP for upstream routing.
- The operator no longer edits Compose files just to make a route reachable.

The name can remain `isengard-proxy` for compatibility, but code and docs should describe it as Isengard's ingress network, not a manual prerequisite.

## Route Sources

Auto-attach applies to every route creation path:

- Label-discovered routes, such as `isengard.expose=plex.vallee.casa`.
- Dashboard or API-created manual routes.
- Stack manifest routes for Isengard-managed stacks.
- Future imported routes, if they target a local runtime container.

The controller should not need to know which path created the route. It owns intent. The agent owns host-local reconciliation.

## Agent Reconciliation

The agent performs reconciliation while applying `ProxyConfig`, before building the new upstream registry.

For each routing rule with a runtime container upstream:

1. Inspect the container.
2. Classify its network mode.
3. Reconcile ingress-network membership when possible.
4. Re-inspect or refresh the container network settings after attach.
5. Resolve the upstream IP.
6. Install the upstream only if the resolved endpoint is valid.

The operation must be idempotent. If the network exists and the container is already attached, applying the same config again is a no-op apart from normal registry replacement.

## Docker Behavior

### Bridge or Compose-Networked Containers

If the container is running on a Docker bridge or Compose network, the agent should:

- Ensure `isengard-proxy` exists.
- Connect the container to `isengard-proxy` if it is not already connected.
- Resolve the upstream from the `isengard-proxy` attachment.

If the connect operation fails because the container disappeared, the route is unresolved until the next config push or Docker event.

### Host-Networked Containers

Docker host-networked containers cannot be attached to a bridge network. For these, the agent should use the host-network upstream path:

- Resolve the Docker host gateway from the agent container.
- Route to `host_gateway:container_port`.
- Log or expose that the route is using `host-network` mode.

This keeps Plex-style deployments working without pretending they joined the ingress network.

### None-Networked Containers

Docker `none` network containers cannot serve a route. The agent should leave the route unresolved with a clear reason. It should not fall back to `127.0.0.1`.

### Stopped or Missing Containers

If the container is stopped, missing, or cannot be inspected, the agent should leave the route unresolved and log the reason. When the container starts and label discovery or another config push runs, reconciliation can try again.

## Unresolved Routes

The current localhost fallback hides the real failure. Replace it with explicit unresolved upstream state.

An unresolved route should keep enough information for:

- Proxy responses to return a meaningful 503 reason.
- Health/events to say why the route is unavailable.
- The dashboard to show an actionable status.

Initial unresolved reasons:

- `container_missing`
- `container_stopped`
- `ingress_network_create_failed`
- `ingress_network_attach_failed`
- `no_usable_container_ip`
- `unsupported_network_mode_none`
- `invalid_container_port`

## Runtime Boundaries

The agent should expose this through the runtime abstraction, not by hard-coding all behavior in proxy config application.

Suggested trait-level operation:

```rust
async fn ensure_ingress_attachment(&self, container_ref: &str) -> Result<IngressEndpoint, RuntimeError>;
```

Where `IngressEndpoint` can represent:

- Attached container IP on the Isengard ingress network.
- Host-network gateway endpoint.
- Unresolved state with reason.

Docker implements this with network create/connect/inspect. Wisp can later implement it using its own network fabric without changing controller routing semantics.

## Safety

Auto-attaching a container to a Docker network is a runtime mutation. Isengard should treat it as part of route reconciliation, but keep it narrow:

- Only attach containers targeted by active routes.
- Never disconnect containers automatically in the first version.
- Never modify application Compose files for externally managed stacks.
- Preserve existing container network attachments.
- Avoid `127.0.0.1` as a fallback unless the runtime explicitly reports that localhost is the correct reachable endpoint.

Disconnect-on-route-delete can be considered later, but only with ownership tracking so Isengard does not remove a network attachment the operator or another tool created.

## Observability

Logs and events should make the reconciliation visible:

- Ingress network created.
- Container attached to ingress network.
- Container already attached.
- Host-network upstream selected.
- Route unresolved with reason.

The dashboard can start by surfacing unresolved reason text in the routing table. A richer route health view can follow later.

## Testing

Unit tests:

- Docker endpoint selection prefers `isengard-proxy`.
- Host-network endpoints use host gateway.
- `none` network resolves to unsupported/unresolved.
- Existing attachments are idempotent.
- Attach failure does not install localhost fallback.

Integration tests with Docker:

- Create a route for a bridge-networked container not attached to `isengard-proxy`; agent attaches it and proxy can route.
- Re-applying config does not duplicate or error.
- Host-networked container route uses host gateway.
- Stopped container route remains unresolved until start/reconcile.

## Migration

Existing deployments that already use `isengard-proxy` continue working. Deployments that forgot to attach routed containers start working automatically after the agent update, except `none` network containers, which become explicitly unresolved.

Docs should remove the operator-facing requirement to edit Compose files for `isengard-proxy` and replace it with a note that Isengard owns the ingress network for routed containers.
