# Phase 8 Plan B: TLS + tailscale + cf-tunnel Adapters Design Spec

**Status:** Final, 2026-05-03 (evening)
**Parent spec:** `2026-05-03-phase-8-networking-and-proxy-design.md` — Plan B picks up at §8 sub-phases 8e, 8f, 8g
**Predecessor:** Plan A (2026-05-03-phase-8a-8d-networking-core.md, executed; PR #18)
**Pencil source:** `design/concepts/settings-networking/{adapter-cf-tunnel-v1,adapter-tailscale-v1}.html`
**Author:** AI partner (Opus 4.7), in dialogue with engineer

---

## 1. Why this exists

Plan A made Isengard a per-host reverse proxy: routing rules in storage, Pingora running, label discovery, healthcheck eviction. But all of it speaks plain HTTP — useless for any real production use.

Plan B makes the proxy *speak HTTPS*, and adds the two adapters that bring real public traffic to the door:

- **8e — TLS via Let's Encrypt.** ACME integration so the proxy can serve HTTPS for any hostname pointed at the host. Works with the `none` adapter for users on public IPs.
- **8f — `tailscale` adapter.** Wires Tailscale Funnel as the public transport for users in tailnets. Adapter-provided certs (no ACME).
- **8g — `cf-tunnel` adapter.** Wires Cloudflare Tunnel as the public transport for users on CF DNS. Edge TLS termination (no Pingora TLS).

After Plan B, an Isengard install can route real HTTPS traffic to a container behind any of three transports, with the appropriate cert strategy for each.

---

## 2. Architecture (delta from Phase 8 spec)

The Phase 8 spec already locks the architecture: `TlsStrategy` enum (`EdgeTermination`, `PassThrough`, `AdapterProvided`), Pingora handles TLS termination when `PassThrough`/`AdapterProvided`, certs sourced from ACME / adapter callback / manual upload. Plan B implements that.

### Implementation-level decisions made in this spec round (deltas from the parent spec)

| Area | Parent spec said | Plan B decision | Why |
|---|---|---|---|
| ACME crate | `acme-rs or equivalent` | **`instant-acme`** | Modern, async, Apache-2.0, active maintenance. `acme-rs` is older + sync API. |
| Tailscale binding | `tsnet` (Go FFI) | **Shell out to `tailscale` CLI** | Avoids first Go-toolchain dep in the Rust workspace. Users with the tailscale adapter overwhelmingly have the CLI installed already (it's their daily driver). FFI cost > convenience benefit. |
| CF Tunnel data plane | cloudflared subprocess + CF API | Unchanged | Spec already correct. |
| Plan slicing | Not addressed | **One plan B for 8e + 8f + 8g** | Adapters share ACME plumbing for the `PassThrough` cert path, and the file structure is similar. Three plans = more overhead than it saves. |

### TLS request flow (consolidated)

```
                                                           ┌─────────────────────────┐
                                                           │   Public client         │
                                                           └────────────┬────────────┘
                                                                        │  HTTPS
                       ┌────────────────────────────────────────────────┴──────────────┐
                       │                Adapter (transport)                            │
                       │                                                               │
                       │   none                tailscale (CLI)         cf-tunnel       │
                       │   PassThrough         AdapterProvided         EdgeTermination │
                       │                                                               │
                       │   forwards TLS        terminates TLS at        terminates TLS │
                       │   bytes to Pingora    funnel edge using        at CF edge,    │
                       │   :8443               cert from `tailscale     forwards plain │
                       │                       cert <host>`             HTTP via       │
                       │                                                cloudflared    │
                       └────────────────────────────────────────────────┬──────────────┘
                                                                        │
                                                ┌───────────────────────┴──────┐
                                                │   Pingora (per host)         │
                                                │                              │
                                                │  :8443 listener (PassThrough │
                                                │  + AdapterProvided)          │
                                                │  :8080 listener              │
                                                │  (EdgeTermination + ACME     │
                                                │  HTTP-01 challenges)         │
                                                │                              │
                                                │  Cert callback at SNI: read  │
                                                │  /var/lib/isengard/tls/      │
                                                │  <hostname>.{crt,key}        │
                                                └───────────────────────┬──────┘
                                                                        │  plain HTTP
                                                                        ▼
                                                              ┌──────────────────┐
                                                              │ Container        │
                                                              └──────────────────┘
```

The `:8443` listener uses Pingora's TLS callback API: on each new TLS connection, the SNI is looked up in `tls_certs` storage + filesystem, and the cert is served. Cert is selected fresh each connection so renewals take effect without restart.

The `:8080` listener serves HTTP for `EdgeTermination` traffic AND handles ACME HTTP-01 challenges at `/.well-known/acme-challenge/<token>`.

### `:8443` HTTPS listener — design notes

Pingora 0.8 supports TLS via the `pingora-rustls` or `pingora-boringssl` feature. Plan B uses **`rustls`** (already in our workspace via reqwest, no extra build deps; no boringssl C compile chain). Adapter type for cert lookup: a `CertResolver` that calls into the agent's `cert_store` module on each TLS handshake.

Cert resolver pseudocode:
```rust
impl ResolvesServerCert for IsengardCertResolver {
    fn resolve(&self, hello: &ClientHello) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?;
        self.cert_store.lookup(sni)
    }
}
```

`cert_store::lookup` is a sync read from a `RwLock<HashMap<hostname, Arc<CertifiedKey>>>` populated/updated by the ACME renewal task and the adapter cert callback. Filesystem is the source of truth; the cache is invalidated when the renewal task writes new files.

---

## 3. ACME integration (8e)

### Crate: `instant-acme`

`instant-acme` 0.7+. Async, returns `Account`, `Order`, `Authorization`, `Challenge` types matching the ACME-v2 protocol. Apache-2.0.

### HTTP-01 challenge handler

When `instant-acme` produces a challenge token, the agent's ACME task writes the `key_authorization` to a shared `HashMap<token, key_auth>`. Pingora's `:8080` listener has a special path handler for `/.well-known/acme-challenge/<token>` that reads this map and responds. After challenge validation succeeds, the entry is removed.

This means **the `:8080` listener is required** even for adapters with `EdgeTermination` — it's the ACME challenge surface. CF Tunnel users can choose between:
- Letting Isengard ACME for them (CF passes `/.well-known/acme-challenge/*` through to the origin) — works but pointless since CF already terminates TLS at the edge.
- Using CF's edge cert (the default `EdgeTermination` path) — no ACME needed.

For the `none` adapter, ACME is the primary cert path. For `tailscale`, ACME is unused (adapter callback). For `cf-tunnel`, ACME is bypassed by edge termination.

### Cert lifecycle

Stored at `/var/lib/isengard/tls/<hostname>.{crt,key}`, mode 0600 (already specified in parent spec §4). Metadata in `tls_certs` SQLite table (already created in Plan A, finally populated for real here).

Renewal scheduler: a tokio task that wakes every hour, queries `tls_certs` for any cert with `next_renewal_at <= NOW()`, and triggers a new ACME order. `next_renewal_at` is set to `not_after - 30 days` on issuance. On renewal failure, retries with exponential backoff (1h, 2h, 4h, 8h, max 24h) and emits `tls.acme.failed` events.

### LE rate limit handling

Let's Encrypt rate limits: 50 certificates per registered domain per week, 5 duplicate certs per week, 5 failures per account/host/hostname per hour. The renewal task respects these via:
- A `last_attempt_at` column on `tls_certs` (added in this round's storage migration)
- Refusing to retry within the failure backoff window
- Logging the limit type when a `429 Too Many Requests` comes back

For local development / CI, we use **Let's Encrypt staging** by default unless the user sets `acme.production = true` in settings. Staging has 30k cert/week limit and untrusted certs in browsers — fine for testing.

---

## 4. Tailscale adapter (8f)

### CLI invocation surface

The adapter shells out to `tailscale` for everything. Required state:
1. `tailscale` CLI installed on the host (verified via `which tailscale` in `join()`)
2. Host is logged in to a tailnet (verified via `tailscale status --json`)

### Lifecycle

**`join(ctx)`:**
1. Verify CLI present and tailnet up. Fail with clear error if either is missing.
2. No-op for the tailnet itself — user logs in via `tailscale up` separately.

**`expose(ctx, spec)`:**
1. Resolve the local pingora port from `ctx.settings`
2. Run `tailscale serve --bg --https=443 --set-path=/ http://localhost:<port>` — this exposes the local Pingora at `<machine-name>.<tailnet>.ts.net:443`
3. If `spec.adapter_specific.funnel == true`: also run `tailscale funnel --bg --https=443 on` to make it public
4. For the cert: run `tailscale cert <hostname>` which writes `<hostname>.crt` and `<hostname>.key` to the current dir. Move them to `/var/lib/isengard/tls/` and update `tls_certs`.
5. Return `ExposedEndpoint { id: "tailscale:<spec.public_hostname>", url, adapter_data: {...} }`

**`unexpose(ctx, endpoint_id)`:**
1. Run `tailscale serve --https=443 --set-path=/ off`
2. If funnel was on: `tailscale funnel --https=443 off`
3. Delete the cert files (they're tailscale's, not ours; tailscale will regenerate on next `cert` call)

**`tls_strategy = AdapterProvided`:** Pingora's cert resolver checks the cert store; adapter populates it via the `tailscale cert` flow. Cert renewal: tailscale handles it automatically as long as the host stays online; Isengard re-runs `tailscale cert` on a 30-day cron just to refresh our local copy.

### Hostname pattern

Tailscale assigns `<machine-name>.<tailnet>.ts.net` for tailnet-only access. For Funnel, a subset of these is public. Users who want a custom domain (e.g. `blog.dirdmaster.com`) instead of `<machine>.<tailnet>.ts.net` need to:
1. CNAME their custom domain to the tailnet name
2. Tell tailscale about the custom hostname (does it support that? — verify, document)

For v1 of the tailscale adapter, the hostname is whatever tailscale assigns. Custom domain support is a v1.x follow-up if it requires anything beyond DNS.

### Crate structure

`crates/isengard-plugins/networking-tailscale/`:
- `Cargo.toml`: deps on `isengard-core`, `tokio` (for async subprocess), `serde_json` (for parsing `tailscale status --json`)
- `src/lib.rs`: `TailscaleAdapter` struct + `NetworkingAdapter` impl
- `src/cli.rs`: thin wrappers around `tokio::process::Command::new("tailscale")` calls
- `tests/loads.rs`: registration test
- `tests/cli_smoke.rs`: `#[ignore]`'d test that requires real tailscale install

---

## 5. Cloudflare Tunnel adapter (8g)

### Two-layer design

**Data plane** (the actual tunnel): `cloudflared` subprocess. Spawned in `join()` with `cloudflared tunnel --no-autoupdate run --token <token>`. Supervised by the agent like Pingora itself (restart on crash, max-restarts policy). Logs piped through tracing.

**Control plane** (tunnel mgmt): direct calls to the CF v4 API via `reqwest`. Used for:
- Tunnel CRUD (create, list, delete) — `/zones/<zone_id>/tunnels`
- Ingress rules — set via the tunnel's config endpoint
- DNS CNAME records — `/zones/<zone_id>/dns_records`

### Settings the adapter needs

Stored in `adapter_config.config_json` (storage already exists from Plan A):
```json
{
  "api_token": "<CF API token with Zone:Read, Zone:Edit, DNS:Edit permissions>",
  "account_id": "<CF account ID>",
  "zone_id": "<CF zone ID for the user's domain>",
  "tunnel_id": "<UUID, created on first join() if absent>",
  "tunnel_name": "isengard-<hostname>",
  "tunnel_token": "<auth token from CF, retrieved after tunnel create>"
}
```

### Lifecycle

**`join(ctx)`:**
1. Read settings from `ctx.settings`
2. If `tunnel_id` absent: create a new tunnel via CF API → store `tunnel_id` + `tunnel_token`
3. Spawn cloudflared subprocess with `--token <tunnel_token>` (this opens the persistent tunnel connection from this host to CF edge)
4. Supervise the subprocess; emit `networking.adapter.subprocess_crash` on unexpected exits
5. Return `Ok(())`

**`expose(ctx, spec)`:**
1. CF API call to PATCH the tunnel's ingress rules: add `{ hostname: <spec.public_hostname>, service: "http://localhost:<spec.local_listener_port>" }` to the rule list (preserve other rules)
2. CF API call to UPSERT a DNS CNAME `<spec.public_hostname>` → `<tunnel_id>.cfargotunnel.com` (proxied=true)
3. Return `ExposedEndpoint { id: "cf-tunnel:<spec.public_hostname>", url: "https://<spec.public_hostname>", adapter_data: { dns_record_id, ingress_index } }`

**`unexpose(ctx, endpoint_id)`:**
1. Parse `endpoint_id` to get hostname
2. Remove the ingress rule (PATCH tunnel config minus the matching entry)
3. Delete the DNS CNAME (DELETE `/dns_records/<dns_record_id>`)

**`tls_strategy = EdgeTermination`:** CF terminates TLS at the edge, traffic arrives at cloudflared as plain HTTP. Pingora listens on `:8080` and serves the rule normally.

### Failure modes & UX hooks

- CF API rate limits: 1200 requests/5min. Adapter caches tunnel/zone/dns IDs aggressively to avoid hitting it.
- cloudflared subprocess exit: if exit code is auth-related, emit `networking.adapter.config_invalid` and stop trying to restart. Otherwise normal restart-with-backoff.
- Tunnel token rotation: not in scope for v1; user re-runs `join()` after rotating the token in CF.

### Crate structure

`crates/isengard-plugins/networking-cf-tunnel/`:
- `Cargo.toml`: deps on `isengard-core`, `tokio`, `reqwest` (rustls), `serde`, `serde_json`
- `src/lib.rs`: `CfTunnelAdapter` struct + `NetworkingAdapter` impl
- `src/api.rs`: CF v4 API client (zones, tunnels, dns_records)
- `src/cloudflared.rs`: subprocess supervisor (mirrors `proxy::supervise` pattern from Plan A)
- `tests/api_units.rs`: unit tests with mocked HTTP responses (`wiremock` crate)
- `tests/loads.rs`: registration test

---

## 6. Settings additions

This spec does NOT include the Settings UI for adapter config (Plan C / 8h owns that). For Plan B, adapter config is provisioned via:
- `adapter_config` SQLite table (writable via existing CRUD) — pre-seed with API tokens etc. via SQL or future CLI subcommand
- Environment variable fallback for the most common settings:
  - `ISENGARD_CF_API_TOKEN` → stored on first `join()`
  - `ISENGARD_TS_HOSTNAME` → optional override of the tailnet-assigned hostname

The Settings UI lands in Plan C and reads/writes the same `adapter_config` rows.

---

## 7. Storage additions

Migration `0010_acme.sql`:

```sql
-- Phase 8e: extend tls_certs for ACME state.
ALTER TABLE tls_certs ADD COLUMN last_attempt_at TEXT;          -- last ACME attempt (for rate-limit tracking)
ALTER TABLE tls_certs ADD COLUMN last_error TEXT;               -- last ACME error message; cleared on success
ALTER TABLE tls_certs ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;

-- ACME account is per-controller, kept here so we don't re-register on every restart.
CREATE TABLE acme_account (
    id              INTEGER PRIMARY KEY CHECK (id = 1),         -- singleton row
    contact_email   TEXT NOT NULL,
    directory_url   TEXT NOT NULL,                              -- 'https://acme-v02.api.letsencrypt.org/directory' or staging
    account_key_pem TEXT NOT NULL,                              -- private key for ACME-v2 account
    kid             TEXT,                                       -- ACME account URL after registration
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

Three new entity methods on `Inventory`:
- `get_acme_account() -> Result<Option<AcmeAccount>>`
- `upsert_acme_account(AcmeAccount)`
- `record_tls_attempt(hostname, success: bool, error: Option<String>)`

---

## 8. Implementation phasing

| Sub-phase | Scope | Why this order |
|---|---|---|
| **8e-1** | Migration `0010_acme.sql` + `AcmeAccount` entity + Pingora `:8443` listener with cert resolver reading from filesystem (no ACME yet — uses pre-placed certs for testing) | TLS plumbing without the ACME complexity |
| **8e-2** | `instant-acme` integration: account registration, HTTP-01 challenge handler on `:8080`, order → cert → store on disk + in `tls_certs` | First real cert |
| **8e-3** | Renewal scheduler + rate-limit guard + `tls.acme.failed` events | Production-ready ACME |
| **8f-1** | `networking-tailscale` crate skeleton + CLI wrappers (`status`, `serve`, `funnel`, `cert`) + `join()` / `unexpose()` no-op | Crate scaffolded, no exposure yet |
| **8f-2** | `expose()` full path: `tailscale serve` + `tailscale cert` + cert installed in `cert_store` | First real adapter exposure |
| **8g-1** | `networking-cf-tunnel` crate skeleton + CF v4 API client (zones, tunnels, dns) + cloudflared subprocess supervisor | Crate scaffolded, tunnels manageable |
| **8g-2** | `expose()` / `unexpose()` full path: ingress rule + DNS CNAME + edge cert verification | First real cf-tunnel exposure |
| **8b-9** (back-port) | Wire HTTPS listener into `proxy::server::run` for production | Production agent serves HTTPS by default after Plan B lands |

Each sub-phase is shippable. After 8e-3 alone, an Isengard install with `none` adapter + DNS on a public IP can serve real LE certs. After 8f-2, the user's tailnet works. After 8g-2, public cloudflared works.

---

## 9. Edge cases

| Scenario | Behavior |
|---|---|
| `instant-acme` order rejected (LE outage, rate limit) | Backoff + retry; emit `tls.acme.failed` with HTTP status; surface in UI as yellow on the rule |
| HTTP-01 challenge fails (port 80 blocked, DNS not propagated) | LE returns specific error; we log + retry on backoff |
| Pingora restarts mid-renewal | The renewal task is in the agent process, not Pingora's. Agent restart restores from disk + retries any pending. |
| `tailscale` CLI not installed | `join()` fails fast with "tailscale CLI not found in PATH; install from tailscale.com" |
| Tailnet not logged in | `join()` fails: "this host is not logged in to a tailnet (tailscale up first)" |
| `tailscale cert <host>` rate-limited | Tailscale's own rate limits apply (similar to LE). Adapter respects them via cooldown table. |
| cloudflared subprocess crashes | Restart with backoff (max 5 in 5min, same policy as Pingora supervisor); emit `networking.adapter.subprocess_crash` |
| CF API token revoked / insufficient permissions | First call returns 401/403; emit `networking.adapter.config_invalid`; stop adapter; surface in UI as red |
| User changes API token mid-run | Adapter detects via heartbeat-time API-call failure; restart with new token from settings |
| User has both adapters configured + same hostname | Already covered by Plan A's per-rule `adapter` field. Each rule has one adapter; no per-hostname conflicts. |
| Disk full → cert can't be written | `Error::IoError`, ACME order rolled back via the `tls_certs.last_error` column. UI shows red. |

---

## 10. Out of scope (Plan C / later)

- Settings UI for adapter config (Plan C / 8h)
- Atomic upstream swap API for blue-green (Plan C / 8i)
- DNS-01 ACME challenge (deferred — HTTP-01 covers v1.x; needed only for wildcards which we don't support yet)
- `headscale`, `raw-wireguard`, `custom` adapters (separate spec rounds)
- Tailscale custom-domain support (CNAME-to-tailnet-name) — v1.x
- Mutual TLS (mTLS) between Pingora and the upstream container — v1.x
- HTTP/3 — defaults to HTTP/2, no QUIC
- Bulk ACME provisioning (e.g. importing existing certs) — v1.x

---

## 11. Success criteria

After Plan B ships:

1. ✅ Single-host install + `none` adapter + DNS A record pointed at the host → `https://blog.example.com` returns the container response with a valid LE cert
2. ✅ Single-host install + `tailscale` adapter + Funnel toggle → `https://<host>.<tailnet>.ts.net` works with adapter-provided cert
3. ✅ Single-host install + `cf-tunnel` adapter + CF API token → `https://blog.example.com` via cloudflared tunnel + edge TLS
4. ✅ Cert renewal triggers automatically at 30 days remaining; renewal failure emits a journal event
5. ✅ Cert and tunnel state survive agent restarts (no re-register needed)
6. ✅ `cargo test --workspace` green (including new integration tests for ACME + adapter unit tests with mocked HTTP)
7. ✅ Real-machine smoke tests for each adapter (manual checklist in PR description)

---

## 12. Testing strategy

- **Unit tests** per crate: ACME state machine (mocked LE responses via wiremock), CF API client (mocked HTTP), tailscale CLI parsing
- **Integration tests** in `crates/isengard-agent/tests/`:
  - `acme_local_e2e.rs` — uses [Pebble](https://github.com/letsencrypt/pebble) (a local LE-compatible CA) for end-to-end ACME without hitting LE production. `#[ignore]`'d unless Pebble is available.
  - `cf_tunnel_api_mock.rs` — wiremock against expected CF API call shapes
- **Real-machine smoke** — manual checklist in the PR description for each of the 7 success criteria above. Real LE staging account, real tailnet, real CF Tunnel.

---

## 13. Dependencies on other crates / phases

- **Plan A** (Phase 8a-8d) — provides the `NetworkingAdapter` trait, routing tables, `tls_certs` table skeleton, Pingora supervisor pattern
- **`instant-acme` crate** (external) — Apache-2.0
- **`tailscale` CLI** (external runtime dep, not a build dep) — user installs separately
- **`cloudflared` binary** (external runtime dep, not a build dep) — user installs separately
- **`reqwest` with `rustls-tls`** — already a workspace dep
- **`pingora-rustls` or `pingora-boringssl` feature on `pingora-core`** — pick `pingora-rustls` for build simplicity (no boringssl C compile)

No phase blocks Plan B from starting. Plan C (Settings UI) consumes adapter config we write in Plan B but is independently slotted.
