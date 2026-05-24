# Lightweight Dev Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make normal feature pushes fast while preserving explicit full confidence gates.

**Architecture:** Keep GitHub CI as the authoritative merge gate. Change local `pre-push` from a full CI mirror to a cheap PR-loop gate, with `LEFTHOOK_FULL=1` restoring the old full local gate. Add `just` recipes that expose the three lanes from the spec.

**Tech Stack:** Lefthook, Just, Cargo, cargo-nextest, cargo-deny, OrbStack `wisp` Linux mirror.

---

## File Structure

- Modify `lefthook.yml`: default `pre-push` checks become light, full gate moves behind `LEFTHOOK_FULL=1` inside the same hook.
- Modify `Justfile`: add `ci-fast`, `ci-full`, and update `ci-linux` comments so the lanes are discoverable.
- No Rust source changes.
- No migration changes.

## Tasks

### Task 1: Add Just Recipes For The Three Lanes

**Files:**
- Modify: `Justfile:44-80`

- [ ] **Step 1: Add `ci-fast` below the existing `test` recipe**

Insert this block after the `test` recipe:

```make
# Fast PR-loop checks. Mirrors the default pre-push hook.
ci-fast: (_disk-preflight min_free_gb)
    cargo fmt --check
    cargo check --workspace --all-targets
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "ERROR: cargo-deny is not installed. Run: cargo install cargo-deny"; \
        exit 1; \
    fi
    cargo deny check
```

- [ ] **Step 2: Add `ci-full` after `ci-fast`**

Insert this block after `ci-fast`:

```make
# Full native confidence gate. Use before risky merges or live upgrades.
ci-full: (_disk-preflight min_free_gb)
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run --workspace; \
    else \
        echo "(install cargo-nextest for faster runs: cargo install cargo-nextest)"; \
        cargo test --workspace; \
    fi
    @if ! command -v cargo-deny >/dev/null 2>&1; then \
        echo "ERROR: cargo-deny is not installed. Run: cargo install cargo-deny"; \
        exit 1; \
    fi
    cargo deny check
```

- [ ] **Step 3: Add `ci-release` after `ci-linux`**

Insert this block after the existing `ci-linux` recipe:

```make
# Full native confidence gate plus Linux mirror. Use before release-grade work.
ci-release: ci-full ci-linux
```

- [ ] **Step 4: Verify recipes are listed**

Run: `just --list`

Expected: output includes `ci-fast`, `ci-full`, `ci-linux`, and `ci-release`.

- [ ] **Step 5: Commit**

```bash
git add Justfile
git commit -m "chore: add dev loop ci lanes"
```

### Task 2: Make Pre-push Light By Default

**Files:**
- Modify: `lefthook.yml:30-127`

- [ ] **Step 1: Replace the pre-push commands with light defaults and full opt-in gates**

Replace the whole `pre-push:` block with:

```yaml
pre-push:
  # Default push lane is intentionally light: catch obvious junk locally,
  # let GitHub CI be the authoritative merge gate. Opt into the old full
  # local gate with `LEFTHOOK_FULL=1 git push` or `just ci-release`.
  parallel: false
  commands:
    fmt-check:
      run: env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_PREFIX -u GIT_EXEC_PATH -u GIT_EDITOR cargo fmt --check
      tags: rust

    check:
      run: env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_PREFIX -u GIT_EXEC_PATH -u GIT_EDITOR cargo check --workspace --all-targets
      tags: rust

    deny:
      run: |
        if ! command -v cargo-deny >/dev/null 2>&1; then
          echo "ERROR: cargo-deny is not installed."
          echo "  Install via: cargo install cargo-deny"
          echo "  Or:          just install-hooks"
          exit 1
        fi
        env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_PREFIX -u GIT_EXEC_PATH -u GIT_EDITOR cargo deny check
      tags: rust

    full-clippy:
      run: |
        if [ "${LEFTHOOK_FULL:-0}" != "1" ]; then
          echo "full-clippy: skipped (set LEFTHOOK_FULL=1 for full local gate)"
          exit 0
        fi
        env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_PREFIX -u GIT_EXEC_PATH -u GIT_EDITOR cargo clippy --workspace --all-targets -- -D warnings
      tags: rust

    full-rustdoc:
      run: |
        if [ "${LEFTHOOK_FULL:-0}" != "1" ]; then
          echo "full-rustdoc: skipped (set LEFTHOOK_FULL=1 for full local gate)"
          exit 0
        fi
        env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_PREFIX -u GIT_EXEC_PATH -u GIT_EDITOR RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
      tags: rust

    full-test:
      run: |
        if [ "${LEFTHOOK_FULL:-0}" != "1" ]; then
          echo "full-test: skipped (set LEFTHOOK_FULL=1 for full local gate)"
          exit 0
        fi
        env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_PREFIX -u GIT_EXEC_PATH -u GIT_EDITOR sh -c '
          if command -v cargo-nextest >/dev/null 2>&1; then
            cargo nextest run --workspace
          else
            cargo test --workspace
          fi
        '
      tags: rust

    linux-mirror:
      run: |
        if [ "${LEFTHOOK_FULL:-0}" != "1" ]; then
          echo "linux-mirror: skipped (set LEFTHOOK_FULL=1 for full local gate)"
          exit 0
        fi
        if [ "${LEFTHOOK_SKIP_LINUX_MIRROR:-0}" = "1" ]; then
          echo "linux-mirror: skipped (LEFTHOOK_SKIP_LINUX_MIRROR=1)"
          exit 0
        fi
        if ! command -v orb >/dev/null 2>&1; then
          echo "linux-mirror: skipped (orb not installed; install OrbStack to catch Linux-only issues locally)"
          exit 0
        fi
        if ! orbctl list 2>/dev/null | awk '{print $1}' | grep -qx 'wisp'; then
          echo "linux-mirror: skipped (no 'wisp' OrbStack machine; see comment in lefthook.yml for bootstrap)"
          exit 0
        fi
        WORKTREE="$(pwd)"
        orb -m wisp bash -lc "
          set -euo pipefail
          source ~/.cargo/env
          cd '$WORKTREE'
          export RUSTFLAGS='-D warnings'
          export CARGO_BUILD_JOBS=2
          cargo fmt --check
          cargo clippy --workspace --all-targets -- -D warnings
          if command -v cargo-nextest >/dev/null 2>&1; then
            cargo nextest run --workspace
          else
            cargo test --workspace
          fi
        "
      tags: rust
```

- [ ] **Step 2: Fix typo in copied skip messages if present**

Search `lefthook.yml` for `LEFHOOK`.

Expected: no matches. Every variable should be spelled `LEFTHOOK`.

- [ ] **Step 3: Validate YAML enough to catch indentation mistakes**

Run: `lefthook run pre-push --command fmt-check`

Expected: command runs `cargo fmt --check` and exits 0.

- [ ] **Step 4: Run the default hook lane**

Run: `lefthook run pre-push`

Expected: `fmt-check`, `check`, and `deny` run. `full-clippy`, `full-rustdoc`, `full-test`, and `linux-mirror` print skipped messages.

- [ ] **Step 5: Commit**

```bash
git add lefthook.yml
git commit -m "chore: lighten pre-push gate"
```

### Task 3: Verify Full Opt-in Still Works

**Files:**
- No source edits expected.

- [ ] **Step 1: Run a narrow full opt-in smoke**

Run: `LEFTHOOK_FULL=1 LEFTHOOK_SKIP_LINUX_MIRROR=1 lefthook run pre-push --command full-clippy`

Expected: workspace clippy runs and exits 0.

- [ ] **Step 2: Run fast lane recipe**

Run: `just ci-fast`

Expected: fmt, check, and deny pass.

- [ ] **Step 3: Do not run `just ci-release` as part of this change**

Reason: the pre-existing full release lane is exactly what this work is trying to make explicit instead of mandatory. GitHub CI will still verify the PR.

### Task 4: Open And Merge The Tooling PR

**Files:**
- Commit history only.

- [ ] **Step 1: Push branch with the new light default hook**

Run: `git push -u origin <branch-name>`

Expected: push completes after the light default hook.

- [ ] **Step 2: Open PR**

```bash
gh pr create --base next --head <branch-name> --title "chore: lighten dev loop gates" --body-file - <<'EOF'
## Summary
- add explicit `just ci-fast`, `just ci-full`, and `just ci-release` lanes
- make pre-push light by default: fmt, check, deny
- keep full local gates behind `LEFTHOOK_FULL=1`

## Verification
- lefthook run pre-push
- LEFTHOOK_FULL=1 LEFTHOOK_SKIP_LINUX_MIRROR=1 lefthook run pre-push --command full-clippy
- just ci-fast
EOF
```

Before running, confirm there are no `LEFHOOK` typos in the PR body.

- [ ] **Step 3: Watch GitHub CI**

Run: `gh pr checks <number> --watch --interval 10`

Expected: all checks pass.

- [ ] **Step 4: Merge after green CI**

Run: `gh pr merge <number> --squash --subject "chore: lighten dev loop gates" --body ""`

Expected: PR merges into `next`.

## Self-Review

- Spec coverage: covers fast default hook, explicit full gates, Just recipes, and GitHub as merge gate.
- Placeholder scan: no TBD or open-ended implementation steps.
- Type consistency: environment variables use `LEFTHOOK_FULL` and `LEFTHOOK_SKIP_LINUX_MIRROR`; plan includes an explicit typo check because copied examples are easy to mistype.
