# Enrollment error diagnosis design

Polish-only change on top of Phase 14. The agent's first-boot enrollment can fail for half a dozen ops-level reasons (wrong scheme, wrong CA, expired token, controller down) but today they all surface as the same opaque tonic/h2/transport chain. This spec adds a thin diagnosis layer that pattern-matches the chain into a friendly explanation while preserving the underlying error for `RUST_LOG=debug`.

## What this is and is not

This is presentation only. No retry policy, no recovery, no new state. The agent still bails on enrollment failure exactly as before. The only change is what the operator sees on stderr.

Out of scope: post-enrollment RPC errors (sync stream, RenewCert), agent-side TLS subsystem errors, controller-side errors. Those have their own paths and aren't worth coupling here.

## Module shape

New file `crates/isengard-agent/src/enroll_diagnosis.rs`. Single public surface:

```rust
pub enum EnrollmentDiagnosis {
    HttpSchemeMismatch,   // case 1: h2 protocol error talking plaintext to TLS
    TokenRejected,        // case 2: tonic Status code Unauthenticated / PermissionDenied
    ConnectionRefused,    // case 3: ECONNREFUSED on the bootstrap dial
    CertificateUntrusted, // case 4: TLS handshake fails to verify server cert
}

pub fn diagnose(err: &anyhow::Error) -> Option<EnrollmentDiagnosis>;

pub fn render(
    diagnosis: EnrollmentDiagnosis,
    controller_url: &str,
) -> String;
```

`diagnose` walks `err.chain()` (anyhow's error-chain iterator) and pattern-matches on the chain's debug/display strings. We do NOT downcast to concrete tonic/h2/hyper types: those types are private (`tonic::transport::error::Kind` is `pub(crate)`, `h2::Error` doesn't expose its variants stably). String matching is the documented escape hatch and is plenty robust for the four cases here. Each case has at least two anchor substrings to avoid false positives.

`render` builds the user-visible block. All strings live in this module so tests can assert exact copy. No em dashes, no en dashes, no emoji.

## Pattern-match anchors

| Case | Anchor substrings (all must appear in the chain) |
|------|--------------------------------------------------|
| HttpSchemeMismatch    | `"h2 protocol error"` AND (`"frame with invalid size"` OR `"FRAME_SIZE_ERROR"`) |
| TokenRejected         | `"status: Unauthenticated"` OR `"status: PermissionDenied"` OR `"token"` AND `"invalid"` |
| ConnectionRefused     | `"Connection refused"` OR `"connection refused"` OR `"ECONNREFUSED"` |
| CertificateUntrusted  | `"certificate"` AND (`"verify"` OR `"unknown issuer"` OR `"self-signed"` OR `"UnknownIssuer"`) |

Order of evaluation: TokenRejected first (cheapest, most specific), then HttpSchemeMismatch, ConnectionRefused, CertificateUntrusted. First match wins. No match returns `None` and the caller falls through to the raw anyhow chain.

## Wire-up

Two touch points in `lib.rs`:

1. The `enroll::enroll(...)` call inside `run_agent` is wrapped: on `Err(e)`, run `enroll_diagnosis::diagnose(&e)`. If `Some(d)`, print `render(d, &opts.controller_url)` to stderr (via `eprintln!`, not `tracing`: the diagnosis is a one-shot UX message, not structured logging) and ALSO log the raw chain at `debug!` so `RUST_LOG=debug` still gets the underlying error. If `None`, propagate the original error unchanged.
2. Either way, `run_agent` still returns `Err(e)`. The new behavior is purely additive output.

The diagnosis path runs only on the enroll RPC, not on later RPCs. Wrapping every tonic call is out of scope: those errors have different operator implications (cert renewal, sync drop) and need their own diagnoses.

## Testing

Six unit tests in `enroll_diagnosis.rs#tests` covering:

1. `http_scheme_mismatch_diagnoses`: synthesize an anyhow chain whose strings include `"h2 protocol error"` and `"frame with invalid size"`. Assert `Some(HttpSchemeMismatch)`.
2. `token_rejected_diagnoses_unauthenticated`: chain contains `"status: Unauthenticated"`. Assert `Some(TokenRejected)`.
3. `connection_refused_diagnoses`: chain contains `"Connection refused"`. Assert `Some(ConnectionRefused)`.
4. `certificate_untrusted_diagnoses_unknown_issuer`: chain contains `"certificate"` + `"UnknownIssuer"`. Assert `Some(CertificateUntrusted)`.
5. `unknown_error_returns_none`: chain contains generic `"oops"`. Assert `None`.
6. `render_includes_controller_url_and_no_em_dash`: render each variant, assert the URL appears verbatim and the output contains no U+2014 / U+2013.

Synthetic chains are built with `anyhow!("...").context("...").context("...")` so we don't need real tonic/h2/hyper machinery in tests. The matching is on string content, not types, so synthetics are equivalent to the real thing as long as the anchor substrings line up.

## Release artifact

`docs/RELEASE_NOTES_ENROLLMENT_ERRORS.md`: short blurb listing the four cases with before/after output. Linked from the PR body.

## Hard rules

- No em dashes (U+2014) or en dashes (U+2013) in user-facing strings or comments.
- 6+ unit tests for the diagnoser.
- All gates green: fmt, clippy `-D warnings`, test, deny.
- Spec + plan together stay around 80 lines each (this file: ~80 lines).
