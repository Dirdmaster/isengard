# Release notes: friendly enrollment error diagnosis

Polish-only change on top of Phase 14. The agent's first-boot enrollment now
diagnoses common ops-level failures and prints a friendly explanation to
stderr instead of raising an opaque tonic/h2/transport chain. The underlying
error is still preserved and surfaces at `RUST_LOG=debug`.

## What changed

- New `crates/isengard-agent/src/enroll_diagnosis.rs` module: pattern-matches
  the `anyhow` chain returned from `enroll::enroll(...)` against four known
  failure modes and renders a multi-line operator-friendly block.
- `run_agent` wraps the enroll call: on a recognized error it prints the
  block and logs the raw chain at debug level. On an unrecognized error it
  falls through to the existing behavior (raw anyhow chain on stderr).
- 8 unit tests cover the diagnoser and renderer.

No behavior change beyond stderr output. Enrollment still bails on failure.
No new state, no retry policy, no new CLI flags.

## Cases handled

### 1. HTTP/HTTPS scheme mismatch

The agent's URL uses `http://` but the controller listens on TLS (Phase 14
mTLS default).

**Before:**
```
Error: Enroll RPC failed

Caused by:
    0: status: Unknown, message: "h2 protocol error: http2 error", details: [], metadata: MetadataMap { headers: {} }
    1: transport error
    2: http2 error
    3: connection error detected: frame with invalid size
```

**After:**
```
x Failed to enroll with controller at http://controller:9417

  Reason: HTTP/2 protocol error during the enrollment handshake.

  Most likely cause: the controller listens on TLS but the URL uses http://
  (not https://). Phase 14 added mTLS enrollment.

  Fix:
    1. Check the controller URL scheme. Should be https:// for 9417.
    2. Make sure ISENGARD_CONTROLLER_CA_PEM_PATH or _PEM is set, since
       the controller uses a self-signed CA by default.
       Export the CA: docker exec isd-controller isengard controller ca export
    3. Re-run.

  For full error chain: rerun with RUST_LOG=debug
```

### 2. Token expired or invalid

The controller rejects the token (gRPC `Unauthenticated` or `PermissionDenied`).

**Before:**
```
Error: Enroll RPC failed

Caused by:
    status: Unauthenticated, message: "token expired", details: [], metadata: MetadataMap { headers: {} }
```

**After:**
```
x Enrollment token rejected by controller at https://controller:9417.

  The token has expired or was already redeemed. Mint a fresh one:
    docker exec isd-controller isengard controller token mint --role agent

  For full error chain: rerun with RUST_LOG=debug
```

### 3. Connection refused

The controller is not listening on the address (container down, wrong port,
wrong hostname).

**Before:**
```
Error: connect bootstrap channel to https://controller:9417

Caused by:
    0: transport error
    1: tcp connect error: Connection refused (os error 61)
```

**After:**
```
x Cannot reach controller at https://controller:9417

  Reason: connection refused. The controller is not listening on this address.

  Common fixes:
    - Is the controller container running? `docker ps | grep isd-controller`
    - Check the controller's listen address. Default 0.0.0.0:9417 for gRPC.
    - From inside another container, the controller's hostname may differ
      from the host (e.g. `controller` via Compose DNS, NOT 127.0.0.1).

  For full error chain: rerun with RUST_LOG=debug
```

### 4. Certificate untrusted

The CA pinned on the agent doesn't validate the controller's cert (wrong CA,
controller regenerated CA, hostname / SAN mismatch).

**Before:**
```
Error: connect bootstrap channel to https://controller:9417

Caused by:
    0: transport error
    1: tls handshake error
    2: invalid peer certificate: UnknownIssuer
```

**After:**
```
x Failed to verify controller's TLS certificate at https://controller:9417

  The CA the agent has does not match the controller's cert. Either:
    - Wrong CA bundled on the agent (re-export with `controller ca export`)
    - Controller regenerated its CA but agent still has the old one
    - Hostname in the URL doesn't match the cert's SAN

  For full error chain: rerun with RUST_LOG=debug
```

## Out of scope

- Post-enrollment RPC errors (sync stream, RenewCert).
- Controller-side errors.
- Retry / recovery logic.

These have different operator implications and warrant their own diagnoses
later if needed.
