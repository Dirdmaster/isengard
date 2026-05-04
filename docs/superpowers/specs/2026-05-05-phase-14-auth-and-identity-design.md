# Phase 14: Auth & Identity — Design

> **Status:** approved 2026-05-05. Replaces Phase 2c's shared `ISENGARD_TOKEN` bearer model.

## Goal

Replace the cluster-wide shared bearer secret with an internal-CA + per-agent mTLS model. After this phase:

- Controller boots without any operator-supplied secret.
- Agents are enrolled via short-lived, mintable, one-time-use tokens.
- Every controller↔agent gRPC call is mTLS-authenticated with per-agent certs signed by the controller's internal CA.
- Per-agent revocation works without disturbing the rest of the fleet.
- Cert rotation is automatic at 50% TTL.

## Non-goals

- Operator → dashboard HTTP auth. The dashboard stays unauthenticated; assumes trusted network (localhost / tailnet / behind cf-tunnel + Cloudflare Access). Dashboard auth is a separate phase, captured on the v1.x roadmap as Cloudflare Access integration.
- Multi-role identity (manager vs worker). Single role for v1; revisit when Stage 2 SaaS or admin/operator separation justifies.
- Cert rotation for the controller's own CA. Single CA, no expiration on the root for v1; rotation is operational (regenerate state-dir, re-enroll). Document.
- External CA / public TLS for the controller. Phase 8 already handles per-host Pingora ACME for service traffic; that's orthogonal.
- RBAC / per-agent permissions. Defer to Stage 2 multi-tenant work.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ Controller                                                       │
│                                                                  │
│   isengard-storage::ca           ┌──────────────────────┐        │
│     ├─ ca table (1 row)          │ enrollment_tokens     │       │
│     │  ├─ root_cert_pem          │  hash, role, ttl      │       │
│     │  └─ root_key_pem (chmod    │  consumed_at          │       │
│     │     600 in state-dir)      └──────────────────────┘        │
│     │                            ┌──────────────────────┐        │
│     │                            │ agent_certs           │       │
│     │                            │  host_id, serial      │       │
│     │                            │  cert_pem             │       │
│     │                            │  issued_at, expires_at│       │
│     │                            │  revoked_at           │       │
│     │                            └──────────────────────┘        │
│     │                                                            │
│   isengard-controller::ca::Authority                             │
│     ├─ load_or_init() → Authority                                │
│     ├─ sign_leaf(host_id, hostname) → (cert_pem, key_pem)        │
│     └─ root_cert_pem() → bytes                                   │
│                                                                  │
│   isengard-controller::enrollment                                │
│     ├─ mint_token(role, ttl) → opaque_token                      │
│     ├─ redeem_token(token, host_info) → IssuedCert               │
│                                                                  │
│   isengard-controller::revocation                                │
│     ├─ revoke(host_id)                                           │
│     └─ Set<serial> in-memory, refreshed on change                │
│                                                                  │
│   tonic Server                                                   │
│     ├─ TLS config: CA root for client verification               │
│     ├─ Enroll RPC: plaintext-TLS, token-in / cert-out            │
│     ├─ All other RPCs: require valid client cert                 │
│     └─ Interceptor: reject if cert serial in revocation set      │
└──────────────────────────────────────────────────────────────────┘
                              ▲
                              │ mTLS gRPC
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Agent                                                            │
│                                                                  │
│   state-dir/                                                     │
│     ├─ agent.json (host_id, controller_url)                      │
│     └─ certs/                                                    │
│         ├─ ca.pem      (controller CA root)                      │
│         ├─ agent.crt   (this agent's cert)                       │
│         └─ agent.key   (this agent's private key, chmod 600)     │
│                                                                  │
│   isengard-agent::cert_store                                     │
│     ├─ exists() → bool                                           │
│     ├─ load() → CertBundle                                       │
│     ├─ save(bundle) → atomic write                               │
│     └─ swap(new_bundle) → atomic rename                          │
│                                                                  │
│   isengard-agent::lib                                            │
│     ├─ on first boot: present token to Enroll, persist bundle    │
│     ├─ on subsequent boots: load bundle, build mTLS channel      │
│     └─ heartbeat loop checks cert TTL, calls RenewCert at 50%    │
└──────────────────────────────────────────────────────────────────┘
```

## Components

### `isengard-storage` additions

**Migration `0014_auth_and_identity.sql`:**

```sql
-- Single-row CA storage. Stored in DB (not filesystem) so backup/restore
-- captures it. The PEM blobs are *not* additionally encrypted: file
-- permissions on the SQLite file (chmod 600 in state-dir) are the only
-- protection. Document this limitation.
CREATE TABLE ca (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    root_cert_pem   TEXT NOT NULL,
    root_key_pem    TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- One-time enrollment tokens. Token plaintext is shown to the operator
-- exactly once; only the sha256 hash is stored.
CREATE TABLE enrollment_tokens (
    token_hash      BLOB PRIMARY KEY,           -- sha256(token_bytes)
    role            TEXT NOT NULL CHECK (role IN ('agent')),  -- v1: only 'agent'
    expires_at      TEXT NOT NULL,
    consumed_at     TEXT,                        -- NULL until redeemed
    consumed_by     BLOB,                        -- host_id of agent that redeemed
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_enrollment_tokens_active
    ON enrollment_tokens(expires_at)
    WHERE consumed_at IS NULL;

-- Per-agent cert ledger. One row per cert ever issued (renewals append,
-- old rows stay for audit). Active cert = (host_id, max(issued_at), revoked_at IS NULL).
CREATE TABLE agent_certs (
    serial          BLOB PRIMARY KEY,            -- cert serial number
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    cert_pem        TEXT NOT NULL,
    issued_at       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT NOT NULL,
    revoked_at      TEXT,
    revoke_reason   TEXT
);

CREATE INDEX idx_agent_certs_host_active
    ON agent_certs(host_id, issued_at DESC)
    WHERE revoked_at IS NULL;
```

**New `crates/isengard-storage/src/ca.rs`:**
- `pub struct CaRow { root_cert_pem, root_key_pem }`
- `Inventory::get_ca() -> Option<CaRow>`
- `Inventory::set_ca(row: CaRow)` — single insert, panics if already set
- All access goes through the existing pool

**New `crates/isengard-storage/src/enrollment_token.rs`:**
- `pub struct EnrollmentTokenRecord { token_hash, role, expires_at, consumed_at, consumed_by }`
- `Inventory::insert_enrollment_token(hash, role, expires_at)`
- `Inventory::find_active_token(hash) -> Option<EnrollmentTokenRecord>` — only returns if not expired AND not consumed
- `Inventory::consume_enrollment_token(hash, host_id)` — atomic UPDATE; errors if already consumed
- `Inventory::list_active_tokens() -> Vec<EnrollmentTokenRecord>` — for the dashboard tab

**New `crates/isengard-storage/src/agent_cert.rs`:**
- `pub struct AgentCert { serial, host_id, cert_pem, issued_at, expires_at, revoked_at, revoke_reason }`
- `Inventory::insert_agent_cert(cert: AgentCert)`
- `Inventory::active_cert_for_host(host_id) -> Option<AgentCert>`
- `Inventory::revoke_cert(serial, reason)` — sets revoked_at + reason
- `Inventory::revoked_serials() -> Vec<Vec<u8>>` — for in-memory revocation set bootstrap

### `isengard-controller` additions

**New `crates/isengard-controller/src/ca.rs`:**

```rust
pub struct Authority {
    cert_pem: String,
    key_pair: rcgen::KeyPair,
    cert_params: rcgen::CertificateParams,
}

impl Authority {
    /// Load CA from storage if present, otherwise generate and persist a new one.
    pub async fn load_or_init(inventory: &Inventory) -> Result<Self>;

    /// Sign a leaf cert for an agent. Returns (cert_pem, key_pem, serial, expires_at).
    pub fn sign_agent_leaf(
        &self,
        host_id: HostId,
        hostname: &str,
        ttl: Duration,
    ) -> Result<IssuedLeaf>;

    pub fn root_cert_pem(&self) -> &str;
}

pub struct IssuedLeaf {
    pub cert_pem: String,
    pub key_pem: String,
    pub serial: Vec<u8>,
    pub expires_at: DateTime<Utc>,
}
```

CA cert: ECDSA P-256, 10-year validity, CN = "Isengard Internal CA", BasicConstraints CA=true, KeyUsage = CertSign+CRLSign. Self-signed.

Leaf cert: ECDSA P-256, 30-day validity by default, CN = host UUID string, SAN = hostname (DNS), KeyUsage = DigitalSignature+KeyEncipherment, ExtendedKeyUsage = ClientAuth+ServerAuth. Serial is 16 random bytes from `rand`.

**New `crates/isengard-controller/src/enrollment.rs`:**

```rust
pub struct EnrollmentService {
    inventory: Arc<Inventory>,
    ca: Arc<Authority>,
}

impl EnrollmentService {
    /// Operator-side: mint a new token. Returns the plaintext token shown
    /// once; only the hash is persisted.
    pub async fn mint(&self, role: TokenRole, ttl: Duration) -> Result<String>;

    /// Agent-side: redeem a token for a fresh cert.
    pub async fn redeem(
        &self,
        token: &str,
        host_info: HostInfo,
    ) -> Result<EnrollResponse>;
}

pub enum TokenRole { Agent } // v1 single role

pub struct EnrollResponse {
    pub host_id: HostId,
    pub agent_cert_pem: String,
    pub agent_key_pem: String,
    pub ca_root_pem: String,
    pub heartbeat_interval_secs: u32,
}
```

Token format: 32 random bytes from `rand::rngs::OsRng`, base32-encoded (no padding), uppercase. Hash: sha256 of the raw bytes (not the base32 string). Operator sees ~52 chars.

**New `crates/isengard-controller/src/revocation.rs`:**

```rust
pub struct RevocationSet {
    inner: Arc<RwLock<HashSet<Vec<u8>>>>,
}

impl RevocationSet {
    pub async fn load_from_inventory(inventory: &Inventory) -> Result<Self>;
    pub fn contains(&self, serial: &[u8]) -> bool;
    pub fn revoke(&self, serial: Vec<u8>);
}

pub async fn revoke_agent(
    inventory: &Inventory,
    revocation: &RevocationSet,
    host_id: HostId,
    reason: &str,
) -> Result<()>;
```

The interceptor checks `revocation.contains(serial)` on every RPC. O(1).

**Tonic server changes in `crates/isengard-controller/src/service.rs`:**

- Build `ServerTlsConfig` with the CA root as the client cert verifier root.
- The `Enroll` RPC must work *without* a client cert (bootstrap chicken-and-egg). Solution: two listeners, OR a single listener with optional client cert + per-RPC enforcement in an interceptor. Pick: **single listener, optional client cert, interceptor enforces**. The `Enroll` RPC is the only one that allows missing client cert; it validates the token instead. Every other RPC: interceptor extracts the client cert from `tonic::Request::peer_certs()`, fails if missing or revoked.

### `isengard-proto` changes

- `EnrollRequest`: drop the implicit bearer header. Add `token: string` field.
- `EnrollResponse`: add `agent_cert_pem`, `agent_key_pem`, `ca_root_pem` fields.
- New RPC: `RenewCert(RenewCertRequest) returns (RenewCertResponse)`.
  - `RenewCertRequest { host_id: bytes }` — authenticated by the existing client cert; the controller cross-checks that the cert presented matches the host_id requested.
  - `RenewCertResponse { agent_cert_pem, agent_key_pem, expires_at }`.
- Drop the `bearer-token` interceptor from the agent client builder.

### `isengard-agent` additions

**New `crates/isengard-agent/src/cert_store.rs`:**

```rust
pub struct CertBundle {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn cert_dir(state_dir: &Path) -> PathBuf;
pub fn exists(state_dir: &Path) -> bool;
pub fn load(state_dir: &Path) -> Result<CertBundle>;

/// Atomic write: write each file as `<name>.new`, fsync, rename.
/// On rename failure, leave the old files in place.
pub fn save(state_dir: &Path, bundle: &CertBundle) -> Result<()>;
```

**Refactor `crates/isengard-agent/src/lib.rs`:**

- Remove the `let token = std::env::var("ISENGARD_TOKEN")...` reads (both call sites).
- Add: on first boot (no `agent.json`), require `ISENGARD_ENROLL_TOKEN` env or `--enroll-token` flag. Call `Enroll(token, host_info)` over a plain TLS channel that *trusts* a self-signed bootstrap cert (the Enroll RPC is the only place we don't have the CA yet). Persist the response into `state-dir/certs/`.
- On subsequent boots: load `cert_store::CertBundle`, build a `tonic::transport::ChannelBuilder` with `tls_config(client_tls(bundle.ca_pem, bundle.cert_pem, bundle.key_pem))`. Use this for all subsequent RPCs.
- New module `crates/isengard-agent/src/cert_renewal.rs`:
  - Spawned task on the existing sync handle. On every heartbeat (every 10s), check `now > issued_at + ttl/2`. If yes, call `RenewCert`, write the new bundle via `cert_store::save`, and *replace the channel's TLS config* (rebuild the client).

### CLI / binary changes

In `crates/isengard/src/main.rs`:

- Drop the `let token = std::env::var("ISENGARD_TOKEN")...` reads in `Controller` and `Agent` arms.
- New top-level subcommand: `isengard ctl ...` (or just folded into Controller subcommands; pick: keep it scoped to controller because these need DB access).
  - `isengard controller token mint --role agent [--ttl 15m]` — instantiates the controller's storage layer (read-only mostly), calls `EnrollmentService::mint`, prints the token to stdout. Exits.
  - `isengard controller agent revoke <host_id> [--reason "..."]` — calls `revoke_agent`. Exits.
  - `isengard controller agent list` — lists hosts + cert serial + expires_at + revoked_at.
- `isengard agent` gains `--enroll-token <TOKEN>` flag (env: `ISENGARD_ENROLL_TOKEN`) — only required if `state-dir/certs/` is empty; ignored once enrolled.

### Dashboard plugin changes

**Settings → Enrollment tab (new):**
- "Mint enrollment token" button → opens a modal. Form: role (locked to "agent" for v1), TTL (default 15m, options up to 24h). On submit: POST `/api/v1/enrollment/tokens` (new endpoint that calls `EnrollmentService::mint`). Returns the token.
- Display the token + the full `docker run` command with token + controller URL pre-filled, copy-to-clipboard. Token shown exactly once; once the modal closes it's gone.
- Below: list of active (unconsumed, unexpired) tokens with "Revoke" button (DELETE `/api/v1/enrollment/tokens/:hash_prefix`).

**Settings → Hosts tab (existing — modify):**
- Per-host: cert serial (last 8 chars), expires_at relative ("in 23 days"), revoked badge if applicable.
- Per-host action: "Revoke" → DELETE `/api/v1/hosts/:id/cert` → `revoke_agent`.

## Data flow

### First-time enrollment

1. **Controller boots** (no special env vars). On first boot, `Authority::load_or_init` generates the CA + persists. On subsequent boots, loads from `ca` table.
2. **Operator mints token:** `isengard controller token mint --role agent` → `01ABCD...` (52 chars).
3. **Operator runs the agent:**
   ```bash
   docker run -d --name isengard-agent \
     -e ISENGARD_CONTROLLER=https://controller:9417 \
     -e ISENGARD_ENROLL_TOKEN=01ABCD... \
     -v /var/run/docker.sock:/var/run/docker.sock \
     -v isengard-agent-data:/var/lib/isengard \
     ghcr.io/dirdmaster/isengard-agent:next
   ```
4. **Agent boots, no `agent.json` present** → reads `ISENGARD_ENROLL_TOKEN`, opens a TLS channel to the controller (trusting the controller's self-presented cert for this single RPC, bootstrap), calls `Enroll(token, host_info)`.
5. **Controller validates:** token exists, not expired, not consumed. Generates host_id (UUID), signs leaf cert via `Authority`, marks token consumed, inserts into `agent_certs`. Returns `EnrollResponse`.
6. **Agent persists:** `agent.json` (host_id, controller_url), `certs/{ca.pem, agent.crt, agent.key}`.
7. **Agent reconnects** with mTLS using the new bundle. Drops the enroll token from memory.

### Steady state

- Every gRPC call from agent: TLS handshake presents `agent.crt`. Controller validates: cert chain → CA root, expiry, serial not in revocation set.
- Heartbeat (every 10s) doubles as cert TTL check. At 50% TTL: agent calls `RenewCert(host_id)`.
- Controller signs a fresh leaf (same key — agent reuses key — or fresh key — pick: **fresh key for safety**), inserts into `agent_certs` (old row stays for audit), returns `RenewCertResponse`. Agent atomic-swaps via `cert_store::save`, rebuilds the gRPC channel.

### Revocation

1. Operator: `isengard controller agent revoke <host_id> --reason "host decommissioned"` (or dashboard button).
2. Controller: marks `agent_certs.revoked_at = now`, adds serial to `RevocationSet` in memory.
3. Next gRPC from that agent: interceptor sees serial in revocation set → returns `Unauthenticated`.
4. Agent's gRPC client surfaces the error → restart loop → tries to reconnect → keeps failing. Logs are clear about the cause.
5. To re-enroll: operator removes `agent.json` + `certs/` from the agent's state-dir, mints a fresh token, runs the agent again.

### Migration

None. The Rust rewrite (`next` branch) has no production users. Operators wipe state-dir, regenerate, re-enroll. Document in release notes.

The legacy Go binary on `main` is untouched by this phase.

## Error handling

- **Token expired / consumed / unknown:** Enroll returns `Unauthenticated`. Agent logs and exits (no point looping; the operator needs to mint a new one). Document this; the wizard's renderer must reflect token validity.
- **Cert expired (e.g. agent was offline > 30 days):** mTLS handshake fails. Agent logs, exits (operator must wipe + re-enroll). This is the worst-case for clock skew or extended outages — the renewal-at-50%-TTL gives a 15-day grace window which should cover any reasonable outage.
- **CA missing on controller boot (corrupted state-dir):** controller logs and exits with a clear message. There is no recovery — the entire fleet is now untrusted, must wipe + re-enroll.
- **Agent cert + state-dir survive but CA was rotated:** mTLS fails. Same recovery as above.
- **Revoked agent reconnects:** `Unauthenticated` on first RPC. Agent logs, exits.
- **mTLS handshake fails for non-cert reasons (network, version):** agent retries with backoff (existing reconnect logic).

## Testing

### Unit tests (per crate)

- `isengard-storage`:
  - CA insert + load round-trip
  - Token insert / find / consume / re-consume rejection
  - Agent cert insert / active lookup / revoke / revoked_serials
- `isengard-controller`:
  - `Authority::sign_agent_leaf` produces a valid cert chain (verify against the CA root)
  - `EnrollmentService::redeem` rejects expired tokens, double-redemption, unknown tokens
  - `RevocationSet` add/contains semantics

### Integration tests

- `crates/isengard-controller/tests/enrollment_e2e.rs`:
  - Boot in-memory controller (already pattern-matched in existing tests)
  - `mint` a token, simulate agent calling `Enroll(token, host_info)` with a fake plain-TLS channel
  - Assert: response has all 4 fields, `agent_certs` has the new row, token is consumed
  - Re-call `Enroll` with same token: assert `Unauthenticated`
- `crates/isengard-controller/tests/mtls_e2e.rs`:
  - Boot controller with mTLS server config (use the test CA)
  - Build a tonic client with the test agent cert; call `Heartbeat` → success
  - Build a tonic client with a *different* CA's cert; call `Heartbeat` → `Unauthenticated`
  - Revoke the agent's cert; call `Heartbeat` → `Unauthenticated`
- `crates/isengard-controller/tests/cert_renewal_e2e.rs`:
  - Issue a cert with TTL=2s, sleep, call `RenewCert` → new cert, old serial still in active list (renewals append, don't revoke), but the new cert is what the agent uses

### Real-Docker e2e

- `crates/isengard-agent/tests/auth_e2e.rs`:
  - Spawn controller container with a fresh state-dir
  - Mint token via `isengard controller token mint` (subprocess)
  - Spawn agent container with the token
  - Assert: agent enrolls, certs land in agent state-dir, mTLS heartbeat succeeds
  - Revoke the agent via `isengard controller agent revoke`
  - Assert: next agent heartbeat fails with `Unauthenticated`

## Engineering picks

| Choice | Pick | Why |
|---|---|---|
| Cert library | `rcgen` | Pure Rust, rustls-aware, simple API, no OpenSSL/Boring drag-in for this purpose. Pingora's BoringSSL stays scoped to Phase 8. |
| Cert algorithm | ECDSA P-256 | Modern default, small certs, fast verify. Universal client support. |
| Cert TTL | 30 days (leaf), 10 years (CA) | Survives outages, doesn't burn CPU on renewal, doesn't force a rotation story for the CA in v1. |
| Renewal trigger | 50% TTL (15d) | Standard for mTLS systems. Survives any reasonable extended outage. |
| Enrollment token format | 32-byte opaque random, base32-encoded | Simpler than JWT (no signing key needed), revocable, no leaked claims. |
| Enrollment token TTL | 15 minutes default, 24h max | Short enough to limit blast radius, long enough for ops automation. |
| Token storage | sha256 hash | Plaintext shown once; if DB leaks, tokens aren't usable. |
| CA private key protection | chmod 600 PEM in DB blob | File permissions on the SQLite file are the only protection. Encrypted-at-rest with passphrase is a v2 add when there's a real multi-tenant story. **Documented as a known limitation.** |
| Cert renewal: same key or fresh? | Fresh key | Limits compromise window; cheap on ECDSA. |
| mTLS at the right layer | tonic native via `ServerTlsConfig` | Tonic supports rustls; no separate proxy needed. |
| Bootstrap (Enroll-without-cert) | Single tonic listener, optional client cert, per-RPC enforcement via interceptor | Avoids running two ports / two listeners. |
| Revocation distribution | Controller-local in-memory set | Single controller; no CRL distribution needed. |
| Migration | Wipe state, re-enroll | No prod users on `next`. Document in release notes. |

## Plan size estimate

12-15 tasks, single PR (no stacking needed). Comparable to Phase 8 Plan A in scope. Estimated ~25-35 commits including review fixes.

## Open known limitations (deferred to later phases)

- CA private key not encrypted at rest (only file permissions protect it).
- No CA rotation story (rotating means re-enrolling everyone; documented).
- No dashboard auth (Cloudflare Access integration is the planned answer; v1.x roadmap).
- No multi-role identity (manager vs worker vs admin).
- No cert pinning during the bootstrap Enroll RPC (agent trusts whatever cert the controller presents during enrollment). A hostile network during enrollment could MITM. Mitigations: TOFU-pin in agent.json after first enrollment, or out-of-band CA fingerprint distribution. Both defer to a follow-up.
