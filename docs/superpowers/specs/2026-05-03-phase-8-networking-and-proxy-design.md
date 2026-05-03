# Phase 8: Networking & Proxy Design Spec

**Status:** Final, 2026-05-03
**Parent spec:** `2026-04-29-platform-pivot-design.md` §15 (long-term own-the-stack arc, proxy slot)
**Vault doc:** `1 Projects/Isengard/Networking & Proxy.md` (ideation, 2026-05-02)
**Related vault docs:** `Cloudflare Integration.md`, `Stack Deployment.md`, `Blue-Green Deployment.md`
**Pencil source:** `design/concepts/settings-networking/` (v1, adapter-cf-tunnel-v1, adapter-tailscale-v1, routing-rule-edit-v1, service-expose-v1)
**Author:** AI partner (Opus 4.7), in dialogue with engineer

---

## 1. Why this exists

Phases 0-7 made Isengard a fleet-aware container monitor: agents enroll, the controller orchestrates updates, the dashboard renders the journal, the wizard onboards. The product is *inward-facing*: it watches your containers and reports.

Phase 8 makes Isengard *outward-facing*. After Phase 8, users can stop running Caddy, Traefik, or Nginx Proxy Manager for their own services and let Isengard handle ingress, TLS, hostname routing, and healthcheck-aware load balancing as a first-class platform feature. This is the technical underpinning of the "Swarm-on-steroids" positioning: an orchestrator with a proxy is materially different from an orchestrator that punts to one.

Two parts, designed together because they entangle:
1. **Pingora**, a per-host reverse proxy (Cloudflare's open-source Rust library), supervised by the agent. Handles all local routing, TLS, healthchecks, atomic upstream swap.
2. **NetworkingAdapter**, a plugin trait that defines the public-facing transport (cf-tunnel, tailscale, raw, custom). Adapters sit *above* Pingora; Pingora always sits *above* the container.

Without Pingora as the local universal layer, every adapter would have to redo hostname routing, TLS, and healthchecks. With it, adapters only need to make the connection arrive at Pingora's listener.

---

## 2. Architecture

### The layered stack

```
                            ┌─────────────────────────┐
                            │  Public internet user   │
                            └────────────┬────────────┘
                                         │
              ┌──────────────────────────┴──────────────────────────┐
              │       NetworkingAdapter (transport / exposure)      │
              │                                                     │
              │   cf-tunnel-adapter   tailscale-adapter   custom    │
              │   (cloudflared)       (tsnet)             (user-    │
              │                                            provided)│
              └──────────────────────────┬──────────────────────────┘
                                         │
                            ┌────────────▼────────────┐
                            │      Pingora proxy       │
                            │      (per-host, in       │
                            │      agent process)      │
                            │                          │
                            │  • SNI/Host routing      │
                            │  • TLS termination       │
                            │  • Healthcheck-aware     │
                            │  • Atomic upstream swap  │
                            │    (blue-green hook)     │
                            └────────────┬─────────────┘
                                         │
                            ┌────────────▼────────────┐
                            │  Container upstreams     │
                            │  (Docker network or      │
                            │   exposed ports)         │
                            └──────────────────────────┘
```

Pingora is **always above the container**, **always below the adapter**. They are not interchangeable, they are stacked.

### Control plane / data plane split

- **Controller (control plane):** owns the routing rules table. Decides which hostname maps to which container, which TLS strategy applies, which healthcheck to run. Pushes `ProxyConfig` messages to each agent over the existing Phase 2 gRPC sync stream.
- **Agent (data plane host):** receives `ProxyConfig`, drives Pingora's reload, supervises Pingora's lifetime, manages adapter lifecycle, emits proxy events back through the existing event channel.
- **Pingora (data plane runtime):** routes traffic, terminates TLS, runs healthchecks, swaps upstreams. Has no direct controller connection — all configuration enters via the agent.

Same pattern as Envoy + Istio, NGINX + a config tool.

### Pingora form: library, not subprocess

Pingora is the `pingora-proxy` crate (Cloudflare-published). The agent embeds it as a tokio task inside its own process — not a separate subprocess.

Rationale:
- No IPC: routing config arrives via shared memory inside the agent
- Faster reloads: no fork/exec on every config change
- Simpler crash handling: tokio `JoinHandle.await` returns the panic, agent restarts the task
- One-binary story preserved: `isengard agent` is still a single binary

Trade-off: a Pingora panic that escapes the task boundary brings down the agent. We accept this because (a) Pingora is mature library code (it powers Cloudflare's edge at billions of req/sec), (b) tokio task panic isolation is the standard Rust async error model, (c) the agent's existing supervisor behavior (systemd or Docker restart=always) covers process-level failures.

If real-world panic-from-Pingora incidents accumulate, carving out into a subprocess is a future refactor, not a v1 concern.

### Where Pingora runs: per-host

Each enrolled host runs one agent → one Pingora. Reasons:
- Containers live on hosts; routing inside the host means traffic enters and stays local once it crosses the adapter boundary.
- A controller-side proxy would be a SPOF and a bandwidth bottleneck. Homelab user with one host = one Pingora; fleet of 10 hosts = 10 Pingoras, each handling its host's traffic.
- Healthchecks and atomic swaps are inherently local — the local Pingora knows the local container's health better than the controller does.

The **controller's own dashboard URL** (e.g. `isengard.dirdmaster.com → controller:9418`) is solved by the single-node assumption: the controller-host always also runs an agent. That agent's Pingora exposes the controller. Split deployments (controller on a separate machine without an agent) are documented as "put the controller behind your own reverse proxy" — not a first-class supported topology in v1.

---

## 3. NetworkingAdapter trait

Plugin trait, registered via `inventory` like other plugins. Sub-trait of the existing `Plugin`.

```rust
#[async_trait]
pub trait NetworkingAdapter: Plugin {
    /// Stable name used in settings, events, UI. Lowercase, no spaces.
    fn id(&self) -> &'static str;  // "cf-tunnel", "tailscale", "raw", "none"

    /// Bring up adapter resources on this host. Idempotent. Called when the
    /// host first enrolls and on adapter config changes.
    async fn join(&self, ctx: &AdapterContext) -> Result<()>;

    /// Tear down adapter resources. Called on adapter swap or host
    /// decommission.
    async fn leave(&self, ctx: &AdapterContext) -> Result<()>;

    /// Expose a routing rule. The adapter is responsible for whatever it
    /// needs to do externally (DNS, tunnel ingress config, firewall rules)
    /// to make `spec.public_hostname` reach this host's Pingora listener
    /// at `spec.local_listener_port`.
    ///
    /// The adapter does NOT do hostname routing or TLS termination — that's
    /// Pingora's job. The adapter just makes the connection arrive.
    async fn expose(&self, ctx: &AdapterContext, spec: &ExposeSpec)
        -> Result<ExposedEndpoint>;

    /// Tear down an exposure. Inverse of expose().
    async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str)
        -> Result<()>;

    /// Optional: provide a TLS cert for the hostname. Some adapters (CF
    /// Tunnel, Tailscale Funnel) handle TLS at their edge and pass plain
    /// HTTP to Pingora. Others (raw, custom) require Pingora to do its own
    /// ACME against Let's Encrypt.
    fn tls_strategy(&self) -> TlsStrategy;
}

pub enum TlsStrategy {
    /// Adapter terminates TLS at its edge. Pingora listens on plain HTTP.
    EdgeTermination,
    /// Adapter passes raw TCP through. Pingora handles TLS via its own ACME.
    PassThrough,
    /// Adapter provides a cert via callback (e.g., Tailscale's
    /// `tailscale cert <hostname>` flow).
    AdapterProvided(Box<dyn Fn(&str) -> Result<TlsCert> + Send + Sync>),
}

pub struct ExposeSpec {
    pub public_hostname: String,         // "blog.dirdmaster.com"
    pub local_listener_port: u16,        // 8443 (Pingora's HTTPS port) or 8080 (plain)
    pub protocol: Protocol,              // Http | Tcp
    pub adapter_specific: serde_json::Value,  // e.g., CF zone_id, Tailscale tags
}

pub struct ExposedEndpoint {
    pub id: String,                      // "cf-tunnel:abc123"
    pub url: String,                     // "https://blog.dirdmaster.com"
    pub adapter_data: serde_json::Value, // for unexpose() to reverse the op
}

pub struct AdapterContext {
    pub host_id: HostId,
    pub settings: Settings,              // host-scoped settings slice
    pub event_emitter: Arc<dyn EventEmitter>,
}
```

### Adapter implementations shipped in Phase 8

| Adapter | join() | expose() | tls_strategy |
|---|---|---|---|
| `none` | no-op | returns error: "no adapter configured" | n/a |
| `tailscale` | join tailnet via tsnet | (no-op for LAN) or `tailscale serve`/Funnel for public | `AdapterProvided` (tsnet fetches cert) |
| `cf-tunnel` | spawn cloudflared subprocess, register tunnel via API | add ingress rule pointing at `localhost:<port>`, create CNAME via CF API | `EdgeTermination` |

`headscale`, `raw-wireguard`, and `custom` are NOT in Phase 8 scope — they ship as separate plugins later (likely v1.x batch).

### Adapter loading

Adapters use the existing `inventory` plugin registration mechanism (Phase 1). Each adapter is its own crate under `crates/isengard-plugins/`:
- `crates/isengard-plugins/networking-none/`
- `crates/isengard-plugins/networking-tailscale/`
- `crates/isengard-plugins/networking-cf-tunnel/`

The `none` adapter is always compiled in (so an agent without explicit configuration has a well-defined baseline). `tailscale` and `cf-tunnel` are feature-gated in `crates/isengard/Cargo.toml` so a user can build a slimmer binary without them if desired. Default cargo features include both.

### Adapter selection

Per-host setting `networking.adapter` selects the active adapter for that host. Set via Settings → Networking UI or via `isengard agent --adapter <name>` flag at install time. Multiple adapters can be installed; only one is active per host. (Per-rule adapter override is supported — see §6 schema.)

---

## 4. Pingora design

### Listeners

Two listeners on every host:
- `:8080` — plain HTTP, used when `tls_strategy = EdgeTermination` (adapter handles TLS) and for ACME HTTP-01 challenges
- `:8443` — HTTPS with TLS termination, used when `tls_strategy = PassThrough` or `AdapterProvided`

Both ports are bound at Pingora startup and serve all rules multiplexed by SNI / `Host:` header. There is exactly one HTTP listener and one HTTPS listener per host — not one port per rule. Adapters configure their ingress to point at one of these two ports.

Both ports are configurable in Settings → Networking → Proxy. Defaults `8080` and `8443` are chosen to avoid conflict with system services on `80`/`443` (privileged ports require capabilities that the agent's nonroot user does not have by default; an `isengard agent --capabilities net_bind_service` flag can opt into binding to `:80`/`:443` directly for users who want it).

### Routing

On request, Pingora reads the `Host:` header (HTTP) or SNI (TLS) and looks up the routing rule by exact hostname. Wildcards are NOT supported in v1 (each `*.example.com` rule is a separate row).

If no rule matches → 404 with body `"no routing rule for <hostname>"`. If a rule matches but no upstream is healthy → 503 with body `"no healthy upstream for <hostname>"`.

### TLS termination

For rules with `tls_strategy != EdgeTermination`, Pingora terminates TLS using a cert from one of three sources:

1. **ACME via Let's Encrypt** — production default. `acme-rs` or equivalent crate, supports HTTP-01 (challenge served on `:8080` under `/.well-known/acme-challenge/`) and DNS-01 (when adapter exposes a DNS API, currently only `cf-tunnel`). Cert renewal at 30 days remaining, cron-style scheduler in the agent.
2. **Adapter callback** — `TlsStrategy::AdapterProvided`. Pingora calls back into the adapter when it needs a cert for a hostname (e.g. tailscale's tsnet auto-cert flow).
3. **Manual cert** — escape hatch. User uploads PEM cert + key via Settings UI; agent stores them on disk; Pingora reads them. Used when ACME isn't possible (private CA, wildcard cert from external source).

### TLS cert storage on disk

Filesystem path: `/var/lib/isengard/tls/<hostname>.{crt,key}`, mode `0600`, owned by the agent's nonroot user. The directory is part of the agent's data volume and is what the future Backup plugin scoops.

Why filesystem not SQLite: Pingora's TLS reload reads PEM files efficiently; storing cert blobs in SQLite means a read+decode every reload. Filesystem also makes manual debugging trivial (`openssl x509 -in /var/lib/isengard/tls/blog.example.com.crt -text`).

### Healthcheck-aware upstreams

Each routing rule can declare a healthcheck:
- `healthcheck_path = '/healthz'` → HTTP GET `/healthz`, expect 2xx
- `healthcheck_path = NULL` → TCP connect check only

Pingora runs the check on `healthcheck_interval_secs` (default 10s). Upstreams failing for 3 consecutive checks are removed from rotation. Upstreams returning to health are re-added after 1 success.

Health state is broadcast to the controller via the agent's event channel as `routing.upstream.health_changed { hostname, upstream, was, now }`.

### Atomic upstream swap (blue-green hook)

Because Pingora is a library inside the agent, the agent calls a plain Rust function (`proxy.swap_upstream(rule_id, new_upstream, grace_period)`) — no IPC, no RPC. When invoked:
1. Pingora marks the new upstream as the primary
2. New connections route to the new upstream
3. Existing connections continue against the old upstream until they close
4. After `grace_period_secs` (default 60s), any remaining old connections are forcibly closed
5. Old upstream is removed from the rule

This is the primitive Phase 10 (Blue-Green Deployment) builds on. In Phase 8 it ships unused but ready.

### Out of scope for Pingora v1

Documented as "explicitly punted, not forgotten":
- WAF / rate limiting (Pingora supports it via custom middleware; defer to v1.x)
- WebSocket session affinity (use straightforward upstream load balancing)
- HTTP/3 (default to HTTP/2)
- Proxying gRPC for non-Isengard traffic (the existing controller↔agent gRPC bypasses Pingora entirely)

---

## 5. Hybrid configuration: UI + labels

The proxy's UX positioning anchor: **NPM's UI meets Traefik's docker labels.** Both modes are first-class. Users don't have to pick — they can click-to-add in the dashboard AND declare in compose, in any combination, on the same controller.

### Two source-of-truth modes

Every routing rule has a `source` discriminator:

```rust
pub enum RuleSource {
    Ui,                              // user clicked "Add rule" in the dashboard
    Label { container_id: String },  // discovered from container labels
    Imported { from: String },       // bulk imported (NPM JSON, Traefik) — DEFERRED
}
```

**Bulk import (`Imported`) is NOT in Phase 8 scope.** It's listed in the schema for future-compat but the parser ships as a separate spec round. The variant is retained so we don't have to do a schema migration when it lands.

The data model is unified — same `routing_rules` table, same Pingora behavior. The difference is who edits the rule:

| Source | Editable in UI? | What happens if labels change? |
|---|---|---|
| `Ui` | ✅ fully | (no labels involved) |
| `Label` | ⚠ override-only — see below | label change re-imports, override rows survive |

### Label vocabulary

Deliberately concise. ~7 labels covering the common 80%; advanced cases escape to the UI.

```yaml
labels:
  isengard.expose: blog.example.com         # required — presence enables routing
  isengard.expose.port: 8080                # default: container's first EXPOSE port
  isengard.expose.tls: edge                 # edge | acme | manual (default: acme)
  isengard.expose.health: /healthz          # default: TCP-only check
  isengard.expose.adapter: cf-tunnel        # default: settings default
  isengard.expose.auth: cf-access           # cf-access | basic | none (default: none) — recognised, ENFORCEMENT DEFERRED to v1.x
  isengard.deploy.strategy: blue-green      # recognised; enforcement requires Phase 10
```

Multi-rule per service (same service, multiple hostnames), pattern-matched:

```yaml
labels:
  isengard.expose.web: blog.example.com
  isengard.expose.web.port: 8080
  isengard.expose.api: api.example.com
  isengard.expose.api.port: 8081
  isengard.expose.api.auth: cf-access
```

When the second segment after `expose.` is a known property name (`port`, `tls`, `health`, `adapter`, `auth`) → it's a property of the default unnamed rule. Otherwise it's a named rule with its own properties under `expose.<name>.<prop>`.

**Auth label is parsed and persisted in v1, enforcement is deferred to v1.x** — the same forward-compat pattern as the `Imported` source variant. Phase 8 stores the value in `routing_rules.auth` (added below in §6 schema as nullable) but Pingora ignores it. When the auth-enforcement plugin lands, it reads existing label values without a re-deploy.

### Label-discovery cadence

Agent subscribes to **Docker's event stream via bollard** — `start`, `stop`, `update` events trigger an immediate label re-read for the affected container. No polling, no per-heartbeat scan. This is event-driven; if the Docker socket reconnects after disconnect, agent does one full container scan to recover any missed events, then resumes streaming.

The agent diffs label-set-on-this-container vs label-state-known-to-controller and sends only the deltas as `routing_rule.upsert` / `routing_rule.delete` messages over the existing gRPC sync stream.

### Conflict resolution: labels win the hostname, UI wins the overrides

A user creates `blog.example.com → web:8080` via the UI. Later, a container with label `isengard.expose: blog.example.com` enrolls. What happens?

1. Hostname / target / port are **owned by source**. If the container declares the hostname via labels, the label wins.
2. Per-field UI overrides **survive label re-imports** when both rules coexist. Stored in a separate `routing_rule_overrides` table keyed by `routing_rule_id + field`.
3. **Conflict UI**: when a label-imported rule arrives matching an existing UI rule's hostname, surface a banner: *"Label discovery for blog.example.com on container `web` — your manual rule has been replaced. Overrides preserved: tls=manual."* Clear undo button to remove the label-source rule and restore UI ownership.

This means: declarative users (Traefik converts) keep their labels as source of truth. UI users (NPM converts) keep their workflow. Mixed shops can have both.

### Routing rule lifetime

A rule **survives container stop**. Stopping a service returns 503 for its hostname; restarting brings the rule back to healthy. The rule is only deleted when its parent stack is deleted (`ON DELETE CASCADE`). When a routing rule is deleted, its `routing_rule_overrides` rows cascade-delete with it (`ON DELETE CASCADE` on the override table's foreign key).

This decouples rule lifecycle from container restart cycles, which happen frequently (deploys, healthchecks, manual restarts) and shouldn't churn external DNS / CF Tunnel ingress every time.

### Per-host data model with fleet-aware UI shortcut

Underneath: every rule binds to ONE host (the host whose container has the label, or the host targeted in the UI). No cross-host forwarding, no distributed coordination.

UI surface: when the user creates a rule via "Add rule" and the underlying service runs on multiple hosts in a fleet, the rule editor offers a **"Apply to all hosts running this service in fleet `<name>`"** checkbox. Checking it generates N rows in `routing_rules`, one per host, all with the same hostname / port / TLS settings — but each binding to a different host. The UI groups them visually as one rule with N upstreams, but the data model is N independent rows.

Trade-off acknowledged: this means hostname uniqueness can't be enforced at the database level for fleet-shortcut rules (multiple rows would share `public_hostname`). The UNIQUE constraint becomes `(public_hostname, host_id)` instead of `(public_hostname)`. The application layer enforces "only one host per fleet can serve a given hostname externally" via DNS — the adapter for each host's rule will create its own CNAME / tunnel ingress, but only the first to win the DNS race serves traffic. (This is acceptable for v1 because the user asked for the shortcut; true cross-host failover is Phase 8.5 / v1.x.)

### Source surface in UI

Routing rules table gains a SOURCE column showing origin with a badge:

| Hostname | Target | Adapter | TLS | Health | Source |
|---|---|---|---|---|---|
| blog.dirdmaster.com | blog/web :80 | cf-tunnel | edge | ✓ healthy | 🏷️ label · `web` |
| grafana.dirdmaster.com | grafana/web :3000 | tailscale | adapter | ✓ healthy | 🎨 ui |

Click on a label-source row → modal explains *"this rule comes from container `web`'s `isengard.expose` label. To stop using labels, remove the label or click 'Convert to UI rule'."*

Click on a UI-source row → editable inline.

(Mocked in `design/concepts/settings-networking/v1.html`.)

---

## 6. Data model

New tables in `isengard-storage` (SQLite migration `0010_routing.sql`):

```sql
CREATE TABLE routing_rules (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    fleet           TEXT NOT NULL,
    host_id         TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    stack_id        INTEGER REFERENCES stacks(id) ON DELETE CASCADE,
    service_name    TEXT NOT NULL,
    container_port  INTEGER NOT NULL,
    public_hostname TEXT NOT NULL,
    protocol        TEXT NOT NULL CHECK (protocol IN ('http', 'tcp')),
    adapter         TEXT NOT NULL,                  -- 'cf-tunnel', 'tailscale', 'none'
    tls_mode        TEXT NOT NULL CHECK (tls_mode IN ('edge', 'acme', 'manual')),
    healthcheck_path TEXT,                          -- e.g. '/health'; NULL = TCP-only
    healthcheck_interval_secs INTEGER NOT NULL DEFAULT 10,
    auth            TEXT,                           -- 'cf-access' | 'basic' | NULL — parsed in v1, enforcement deferred to v1.x
    state           TEXT NOT NULL CHECK (state IN ('pending', 'active', 'draining', 'failed')),
    -- Hybrid mode source tracking
    source          TEXT NOT NULL CHECK (source IN ('ui', 'label', 'imported')),
    source_container_id TEXT,                       -- non-NULL when source='label'
    source_imported_from TEXT,                      -- 'npm', 'traefik' when source='imported' (DEFERRED)
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(public_hostname, host_id)
);

CREATE INDEX idx_routing_rules_host ON routing_rules(host_id);
CREATE INDEX idx_routing_rules_stack ON routing_rules(stack_id);
CREATE INDEX idx_routing_rules_hostname ON routing_rules(public_hostname);

-- Per-field UI overrides for label-source rules.
CREATE TABLE routing_rule_overrides (
    routing_rule_id INTEGER NOT NULL REFERENCES routing_rules(id) ON DELETE CASCADE,
    field           TEXT NOT NULL,    -- 'tls_mode', 'healthcheck_path', 'adapter', etc.
    value_json      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (routing_rule_id, field)
);

-- Adapter configuration, one row per host per adapter.
CREATE TABLE adapter_config (
    host_id         TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    adapter         TEXT NOT NULL,
    config_json     TEXT NOT NULL,    -- adapter-specific settings (CF token, tailnet auth key, etc.)
    enabled         BOOLEAN NOT NULL DEFAULT 0,
    PRIMARY KEY (host_id, adapter)
);

-- Cert metadata (the actual PEM lives on the agent's filesystem).
CREATE TABLE tls_certs (
    public_hostname TEXT PRIMARY KEY,
    host_id         TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    issuer          TEXT NOT NULL,    -- 'lets_encrypt', 'adapter:tailscale', 'manual'
    not_before      TEXT NOT NULL,
    not_after       TEXT NOT NULL,
    last_renewed_at TEXT,
    next_renewal_at TEXT NOT NULL,
    serial          TEXT
);
```

### `ProxyConfig` over the wire

The controller's `ProxyConfig` message pushed to each agent over the gRPC sync stream:

```protobuf
message ProxyConfig {
  string host_id = 1;
  uint64 generation = 2;            // monotonic; agent ignores stale configs
  repeated RoutingRule rules = 3;
  ProxySettings settings = 4;       // listener ports, ACME contact email, log sample rate
}

message RoutingRule {
  int64 id = 1;
  string public_hostname = 2;
  Upstream upstream = 3;
  TlsMode tls_mode = 4;
  Healthcheck healthcheck = 5;
  string adapter = 6;
}

message Upstream {
  string container_id = 1;
  string container_ip = 2;
  uint32 container_port = 3;
}
```

`ProxyConfig.generation` solves the late-arriving-message problem: agent only applies configs with a generation > last applied. Controller increments on every push.

---

## 7. Settings UX

`Settings → Networking` has two tabs (mocked in `design/concepts/settings-networking/v1.html`):

**Adapter tab:** which transport(s) for this host?
- Card per installed adapter (`none`, `tailscale`, `cf-tunnel` in Phase 8)
- Each card shows: status (green/yellow/red), config form, enable toggle, test button
- Default adapter selector at the top
- Per-host override note: "default applies to new rules; existing rules keep their adapter"

**Proxy tab:** Pingora settings + routing rules.
- Listener ports (HTTP + HTTPS), ACME contact email, log sample rate, default healthcheck interval
- Routing rules table (rows for every rule across all hosts in the active fleet) with SOURCE badge
- Inline rule editor (modal opens for create / edit)
- "Add rule" button → opens the routing-rule-edit modal (mocked: `routing-rule-edit-v1.html`)

**Stack detail integration:** the existing "Expose" button on a service card opens the same routing-rule-edit modal pre-populated with that service. Result: a row inserted into `routing_rules` with `source = 'ui'`, `host_id = <selected host>`. (Mocked: `service-expose-v1.html`.)

**Adapter cards** (mocked: `adapter-cf-tunnel-v1.html`, `adapter-tailscale-v1.html`):
- cf-tunnel: API token field, account ID, zone selector, tunnel name, status of cloudflared subprocess + last connection time, list of active ingress rules
- tailscale: tailnet name, auth key field (or OAuth flow), tags, exit-node toggle, Funnel toggle, list of nodes seen

---

## 8. Implementation phasing

| Phase | Scope | Why this order |
|---|---|---|
| **8a** | NetworkingAdapter trait + `none` no-op adapter + `routing_rules` / `routing_rule_overrides` / `adapter_config` / `tls_certs` SQLite migrations + storage CRUD | Trait shape is the thing other phases depend on; storage layer enables all subsequent work |
| **8b** | Pingora as a tokio task in the agent; binds `:8080` + `:8443`; static config from a single hardcoded `RoutingRule`; HTTP only, no TLS | Smallest demonstrable proxy. End-to-end test: docker-compose with one service, one rule, curl through Pingora succeeds |
| **8c** | `ProxyConfig` proto + controller pushes via existing sync stream; agent reloads Pingora on receipt; full per-host CRUD reflected in the store | Removes 8b's hardcoding; controller becomes the source of truth |
| **8d** | Healthcheck-aware upstream selection; `routing.upstream.health_changed` event; healthcheck status surfaced in routing rules table | Prerequisite for blue-green; standalone value |
| **8e** | ACME via Let's Encrypt; HTTP-01 challenge handler on `:8080`; cert storage on disk at `/var/lib/isengard/tls/`; `tls_certs` table populated; cron-style renewal scheduler in agent | Real HTTPS; works with `none` adapter for users with public IP + DNS pointed at the host |
| **8f** | `tailscale` adapter: tsnet bootstrap, `AdapterProvided` TLS via tsnet auto-cert, Funnel toggle for public, settings UI card | First real adapter; validates trait shape with non-trivial implementation |
| **8g** | `cf-tunnel` adapter: cloudflared subprocess management, CF API for tunnel creation + DNS, `EdgeTermination` TLS, settings UI card | Second adapter; validates `EdgeTermination` strategy |
| **8h** | Settings UI Networking tab (Adapter + Proxy sub-tabs), routing rules table with SOURCE column + filters, inline rule editor, conflict banner for label-vs-UI, fleet-shortcut UI | UX layer over the now-functional backend |
| **8i** | Atomic upstream swap API on Pingora (`swap_upstream(rule_id, new_upstream, grace_period)`); blue-green hook used by Phase 10 | Cheap to add now while Pingora internals are fresh, expensive to retrofit |

Each phase is shippable independently. Phase 8b alone unlocks "I can demo Isengard routing traffic to a container."

**Label discovery (Docker events subscription) lives in Phase 8c**, alongside the `ProxyConfig` push. Without it, `source = 'label'` rules can't enter the system.

---

## 9. Edge cases

| Scenario | Behavior |
|---|---|
| Adapter `join()` fails | Host enters degraded state. Routing rules can't be applied. Surfaces as `networking.adapter.failed` event with retry-after. UI shows red status card. |
| Pingora tokio task panics | `JoinHandle.await` returns the panic; agent restarts the task with exponential backoff (up to 5 attempts in 5 minutes). After max retries, agent emits `proxy.crashloop` event and gives up. UI shows red on the host card. |
| Routing rule references a service that doesn't exist on the host | Pingora returns 503 with body explaining "upstream not available". Controller emits `routing.upstream_missing` event, surfaced in UI. |
| Two adapters configured simultaneously | Allowed. Each routing rule carries an `adapter` field; each rule is bound to one adapter. Settings UI shows multiple adapter cards as "active". |
| ACME challenge fails (DNS not propagated, port 80 blocked, rate limit) | Pingora retries on a backoff (LE has explicit guidance: 1h, 2h, 4h, max 24h). Emits `tls.acme.failed` event with attempt count. UI shows yellow on the rule. Manual cert upload as escape hatch. |
| User changes adapter mid-rule (cf-tunnel → tailscale for the same hostname) | Routing rule's `adapter` field flips. Old adapter's `unexpose()` runs, new adapter's `expose()` runs. State machine: `active → draining → pending → active`. Failure of new adapter rolls back to old. |
| Compose label change without container restart | Docker `update` event triggers immediate label re-read. Agent diffs against known state, controller upserts rule, `ProxyConfig` flows back. No container restart required. |
| Container with `isengard.expose` label has no exposed port | Falls back to first `EXPOSE` directive in the image. If none, rule is rejected with `routing.label_invalid` event. UI surfaces a yellow banner on the container's row. |
| Fleet-shortcut rule: two hosts in fleet both come up healthy but DNS only points to one | Both adapters create their own CNAME; only the DNS-pointed one serves. The other emits `routing.shortcut_unreachable` event flagged in UI as "DNS not pointing here". User must manually adjust DNS or accept that only one host serves. |
| Blue-green swap to a healthcheck-failing upstream (Phase 10 use of 8i) | Pingora refuses the swap, keeps old upstream alive. Emits `routing.swap.aborted` event. Phase 10's orchestration retries with backoff. |
| Adapter subprocess (e.g. cloudflared) crashes | Adapter's `join()` is re-invoked under the same retry policy as Pingora. Emits `networking.adapter.subprocess_crash` event. |

---

## 10. Open questions resolved (record of decisions made in 2026-05-03 brainstorm)

These were the unresolved items from the vault `Networking & Proxy.md` doc. Decisions made:

| Question | Decision | Rationale |
|---|---|---|
| Pingora binary distribution: library vs subprocess | **Library** (tokio task in agent) | No IPC, simpler reload, panic isolation via tokio. Subprocess carve-out is a future refactor only if real panic incidents accumulate. |
| TLS cert storage | **Filesystem** at `/var/lib/isengard/tls/<hostname>.{crt,key}` mode 0600 | Pingora reload reads PEM efficiently; debugging is trivial; future Backup plugin scoops the directory. |
| Routing rule lifetime when container stops | **Survives stop, returns 503**; deleted only when parent stack is deleted | Container restarts are frequent and shouldn't churn external DNS / tunnel ingress. |
| Per-fleet vs per-host rules | **Per-host data model + fleet-aware UI shortcut** | Underneath, every rule is per-host. UI offers "apply to all hosts in fleet" which generates N rows. No cross-host forwarding, no distributed coordination. True fleet-scoped routing is Phase 8.5 / v1.x. |
| Controller dashboard URL bootstrap | **Single-node assumption: controller-host always runs an agent** | That agent's Pingora exposes the controller. Split deployments document "use your own external proxy". |
| Bulk import (NPM JSON, Traefik dynamic.yml) | **Deferred to a later spec round** | Schema retains the `Imported` source variant for forward compat, but the parser ships separately. |
| Label-discovery cadence | **Docker event stream via bollard** (start/stop/update) | Zero polling, instant reaction. Reconnect triggers a full scan to recover missed events. |
| Pingora listener architecture | **One HTTP (`:8080`) + one HTTPS (`:8443`) listener, SNI/Host-routed** | Standard reverse-proxy pattern. Adapters point at one of these two ports. |

---

## 11. Out of scope (explicitly punted)

- WAF, rate limiting, request shaping → Pingora supports it via middleware; deferred to v1.x
- WebSocket session affinity (sticky sessions) → use straightforward LB
- HTTP/3 → defaults to HTTP/2
- gRPC proxying for non-Isengard traffic
- `headscale`, `raw-wireguard`, `custom` adapters → separate plugin specs in the v1.x batch
- True cross-host load balancing / fleet-scoped routing → Phase 8.5 / v1.x
- Bulk import from NPM/Traefik → separate spec round, schema is forward-compat
- Privileged-port (`:80`/`:443`) binding by default → opt-in via `--capabilities net_bind_service`
- mTLS between adapter and Pingora → adapter loopback to localhost, no mTLS needed
- Per-rule middleware chains (auth, rewrite, etc.) → labels offer one auth strategy (`cf-access`); richer middleware is v1.x

---

## 12. Success criteria

Phase 8 ships when, on a single-host install:

1. ✅ User installs the `cf-tunnel` adapter, configures CF API token, enables it in Settings → Networking
2. ✅ User runs `docker compose up` on a stack whose service has `isengard.expose: blog.example.com` + `isengard.expose.port: 8080` labels
3. ✅ Within 5s, the rule appears in Settings → Networking → Proxy with SOURCE = label and STATE = active
4. ✅ `https://blog.example.com` resolves and returns the container's response, with a valid TLS cert (CF Edge in this case)
5. ✅ Stopping the container surfaces STATE = degraded; restarting returns to active; deleting the stack removes the rule
6. ✅ Equivalent flow with `tailscale` adapter and `https://blog.tailnet-name.ts.net` over Tailscale Funnel
7. ✅ User adding a UI rule from Stack detail's "Expose" button generates a `source = 'ui'` row that survives container restarts
8. ✅ `cargo test --workspace` passes including new integration tests for the `none` adapter end-to-end

---

## 13. Testing strategy

- **Unit tests** per crate: trait method signatures, ProxyConfig serialization round-trips, label parser, conflict resolution logic, healthcheck state machine
- **Integration tests** in `crates/isengard-controller/tests/`:
  - `proxy_basic_routing.rs` — end-to-end with `none` adapter, real Pingora, real container (via testcontainers): controller pushes config, agent loads it, request reaches container
  - `proxy_label_discovery.rs` — start container with labels, assert rule appears, change labels, assert rule updates
  - `proxy_health_eviction.rs` — container becomes unhealthy, upstream evicted, returns to health, re-added
  - `proxy_atomic_swap.rs` — exercise `swap_upstream` API, assert grace period behavior
- **Adapter tests** are mocked at the adapter API boundary in CI (no real CF account, no real tailnet); manual smoke tests document the real-adapter flow in the spec's "Success criteria"
- **Pingora panic test** — deliberately panic inside a Pingora task, assert agent restarts it within backoff window

---

## 14. Dependencies on other crates / phases

- **Phase 1** (Plugin trait + host) — adapters are plugins, use `inventory` registration
- **Phase 2** (gRPC sync stream) — `ProxyConfig` flows over the existing stream, no new transport
- **Phase 4** (Event journal + bus) — `routing.*` and `networking.*` events emit through the existing bus
- **Phase 5** (Dashboard) — Networking settings tab + routing rules table land here, in `crates/isengard-plugins/dashboard/`
- **`pingora-proxy` crate** (external, Cloudflare-published) — version pinned in workspace `Cargo.toml`
- **`acme-rs` or `instant-acme` crate** (external) — for ACME client implementation
- **`bollard` crate** — already in use; gain Docker event stream subscription

No phase blocks Phase 8 from starting; Phase 8 unblocks Phase 10 (Blue-Green) and a real cf-tunnel ship.
