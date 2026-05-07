# Enrollment error diagnosis plan

Spec: `docs/superpowers/specs/2026-05-06-enrollment-error-diagnosis-design.md`.

Single PR off `next`, branch `chore/polish-errors`. Two commits, each a clean unit.

## T1: diagnoser module + tests

Files:
- `crates/isengard-agent/src/enroll_diagnosis.rs` (new):
  - `pub enum EnrollmentDiagnosis { HttpSchemeMismatch, TokenRejected, ConnectionRefused, CertificateUntrusted }`.
  - `pub fn diagnose(err: &anyhow::Error) -> Option<EnrollmentDiagnosis>`. Walks `err.chain()`, collects each link's `to_string()` and `format!("{:?}", ...)` into one buffer, then runs the anchor checks in the spec's order: TokenRejected, HttpSchemeMismatch, ConnectionRefused, CertificateUntrusted.
  - `pub fn render(d: EnrollmentDiagnosis, controller_url: &str) -> String`. Returns the multi-line block exactly matching the four examples in the spec request. Leading line uses ASCII `x` (no glyph), no em/en dashes anywhere.
- `crates/isengard-agent/src/lib.rs`: add `pub mod enroll_diagnosis;` next to the existing `pub mod enroll;`.

Tests added inline (`#[cfg(test)] mod tests`):
1. `http_scheme_mismatch_diagnoses`: build `anyhow!("connection error detected: frame with invalid size").context("h2 protocol error").context("transport error").context("Enroll RPC failed")`, assert `Some(HttpSchemeMismatch)`.
2. `token_rejected_diagnoses_unauthenticated`: `anyhow!("status: Unauthenticated, message: \"token expired\"").context("Enroll RPC failed")`, assert `Some(TokenRejected)`.
3. `connection_refused_diagnoses`: `anyhow!("tcp connect error: Connection refused (os error 61)").context("connect bootstrap channel")`, assert `Some(ConnectionRefused)`.
4. `certificate_untrusted_diagnoses_unknown_issuer`: chain contains `"invalid peer certificate: UnknownIssuer"`, assert `Some(CertificateUntrusted)`.
5. `unknown_error_returns_none`: `anyhow!("oops, unrelated error")`, assert `None`.
6. `render_has_no_em_dash_and_includes_url`: for each variant, render with `"https://controller:9417"`, assert URL is in the output AND output contains neither `'\u{2014}'` nor `'\u{2013}'`.

Plus one bonus correctness test:
7. `token_rejected_wins_over_h2`: chain has BOTH `"status: Unauthenticated"` AND `"h2 protocol error"`. Assert `Some(TokenRejected)` (order-of-evaluation property).

Commit: `feat(agent): enrollment error diagnoser module + 7 tests`

## T2: wire diagnoser into run_agent + release notes

Files:
- `crates/isengard-agent/src/lib.rs`: in `run_agent`, replace
  ```rust
  let outcome = enroll::enroll(...).await?;
  ```
  with a match block that, on `Err(e)`:
  - calls `enroll_diagnosis::diagnose(&e)`. If `Some(d)`, `eprintln!("{}", enroll_diagnosis::render(d, &opts.controller_url))` and `tracing::debug!(error = ?e, "enrollment failed: full error chain")`.
  - returns `Err(e)` unchanged.
- `docs/RELEASE_NOTES_ENROLLMENT_ERRORS.md` (new): four sections (one per case), each with the before block (raw anyhow chain) and the after block (rendered diagnosis). Note that full error is preserved at `RUST_LOG=debug`.

No new tests on the wire-up: the unit tests cover the diagnoser, and the wire-up is a 5-line glue change. A future integration test could spin up a plaintext-on-TLS controller stub and assert stderr, but that's out of scope for a polish PR.

Commit: `feat(agent): friendly enrollment error diagnosis`

## Gates

Before pushing, run locally:
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run -p isengard-agent` (the new tests live here)
- `cargo nextest run --workspace` (sanity sweep)

Push branch, open PR vs `next`, title `feat(agent): friendly enrollment error diagnosis`, body shows before/after for the four cases.

## Verification

After CI is green, eyeball-verify by running locally:
```
cargo run -p isengard -- agent --controller http://localhost:9417 --state-dir /tmp/iso-broken
```
against a TLS controller. Output should lead with the friendly block, not the raw chain.
