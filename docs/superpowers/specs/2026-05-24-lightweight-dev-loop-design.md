# Lightweight Dev Loop Design

## Problem

Getting a small feature onto `next` takes too long because every push runs the full confidence suite locally, then GitHub runs it again, then Docker images build. Recent auto-adopt fixes showed the failure mode clearly: small controller-only changes still paid for workspace clippy, rustdoc, full tests, cargo-deny, and the Linux mirror before the branch could even reach GitHub.

The local hook should catch obvious mistakes. GitHub CI should be the merge gate. The Linux mirror should be explicit, not part of every feature push.

## Answer: no full workspace every time

Do not build or test the whole workspace for every code change.

Use three lanes instead:

1. Inner loop: package-scoped checks while editing.
2. PR loop: fast pre-push checks, then GitHub CI gates merge to `next`.
3. Release/deploy loop: explicit full local confidence when production risk justifies it.

## Lanes

### Inner loop

Target: 1 to 3 minutes.

Used during implementation. The developer picks the package and test filter relevant to the change.

Examples:

```bash
cargo fmt --check
cargo clippy -p isd --all-targets -- -D warnings
cargo test -p isd doctor --lib
cargo test -p isengard-controller compose_autoadopt --lib
```

This lane is manual and focused. It should not try to infer perfect dependency impact.

### PR loop

Target: branch push under 5 minutes on a warm cache.

`pre-push` should run only cheap checks by default:

- `cargo fmt --check`
- `cargo check --workspace --all-targets`
- `cargo deny check`

It should not run by default:

- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo doc --workspace --no-deps --document-private-items`
- `cargo nextest run --workspace`
- OrbStack Linux mirror

Those are still required before merge, but GitHub CI already runs them. Local push should not duplicate the whole gate.

### Release/deploy loop

Target: confidence, not speed.

Add explicit recipes:

- `just ci-fast`: same checks as default `pre-push`.
- `just ci-full`: native full workspace fmt, clippy, rustdoc, tests, and deny.
- `just ci-release`: `ci-full` plus Linux mirror.

Use this lane before risky live upgrades, dependency upgrades, workflow edits, or release tags.

## Pre-push controls

Default `git push` uses the PR loop.

Full local gate remains available by setting:

```bash
LEFTHOOK_FULL=1 git push
```

Linux mirror can still be skipped independently for emergencies:

```bash
LEFTHOOK_SKIP_LINUX_MIRROR=1 LEFTHOOK_FULL=1 git push
```

## GitHub stays authoritative

Branch protection should continue to require GitHub CI for `next`.

The lighter local hook is acceptable because merge still waits on:

- build
- lint (fmt + clippy + rustdoc)
- test (nextest)
- cargo-deny
- migration immutability
- Docker image build after merge

## Out of scope

- Automatic changed-crate detection. It is tempting, but brittle in a Rust workspace with shared crates and build scripts. Keep local checks explicit.
- Reworking Docker publishing. That can come later if live deploy still feels too slow after pre-push is fixed.
- Changing branch protection.

## Success criteria

- Normal branch push no longer runs full workspace tests, rustdoc, or Linux mirror locally.
- A developer can still run the old full confidence path with one command.
- The default hook output clearly explains how to opt into the full gate.
- `just ci-fast`, `just ci-full`, and `just ci-release` exist and match the lanes above.
