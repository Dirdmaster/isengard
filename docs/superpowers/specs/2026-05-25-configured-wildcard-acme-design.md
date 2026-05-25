# Configured Wildcard ACME Design

## Context

`isd configure` is the operator surface for routing zones, Cloudflare credentials, and ACME settings. Live testing showed that setting `routing.zones=[{ name: "vallee.casa", wildcard: true }]` does not start wildcard certificate issuance. The current controller starts the DNS-01 wildcard scheduler only from boot-time env and CLI flags.

That breaks the Isengard contract: configure changes should be live controller state, not a request to recreate containers.

## Decision

Make controller wildcard ACME reconcile from `isd configure` at runtime.

A controller background task reads these keys on a short interval and on boot:

- `routing.zones`: desired managed zones, using rows with `wildcard: true`
- `cloudflare.api_token`: DNS-01 Cloudflare token
- `acme.contact_email`: ACME account email
- `acme.directory`: ACME directory, including `prod` and `staging` aliases

For backward compatibility, existing boot flags and env remain fallback values when configure keys are missing.

## Behavior

- Changing `routing.zones` to enable wildcard should be enough.
- No controller recreate is required after configure writes.
- The reconciler issues `*.zone` plus `zone` as one order, matching the existing static parser.
- Existing scheduler backoff and metadata keep repeated polls from hammering Let's Encrypt.
- When issuance succeeds, the reconciler persists the cert and asks the routing pusher to fan out a fresh `ProxyConfig` to connected agents.
- The agent installs wildcard cert material through the existing `ProxyConfig.wildcard_certs` path.

## Architecture

Add `crates/isengard-controller/src/acme/configured.rs`.

Responsibilities:

- Resolve the current desired wildcard ACME config from `ConfigDispatcher` plus fallback `AcmeConfig`.
- Convert wildcard zones to domain identifiers: `*.vallee.casa,vallee.casa`.
- Spawn a loop that calls the existing `scheduler::tick` with a fresh Cloudflare DNS provider.
- Keep the existing env-driven `AcmeConfig` as fallback only, not as a separate scheduler.

`run_controller` will create the configured scheduler after the routing pusher exists.

## Testing

Unit tests cover the pure resolver:

- configured wildcard zones produce `*.zone` plus apex identifiers
- disabled wildcard zones do not produce groups
- configure values override env fallback values
- env fallback still works when configure is unset

A controller ACME test also verifies the reconciler uses the existing `scheduler::tick` contract with fake provider/client boundaries where practical.

## Operator impact

After this ships, the correct flow is:

```sh
isd configure
# enable wildcard on vallee.casa
```

Then watch:

```sh
docker logs -f isd-controller | grep -i 'acme\|wildcard\|cloudflare'
```

No restart should be needed.
