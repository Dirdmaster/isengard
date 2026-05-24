#!/usr/bin/env bash
#
# Reject byte-level edits to any migration file that exists on `$base`.
#
# Migrations are immutable post-ship: sqlx hashes each file in
# `_sqlx_migrations` on first apply, then re-hashes it on every boot
# and refuses to start if the digest drifts. PR #287 (2026-05-23)
# changed a single comment in `0028_stack_manifest.sql` during a
# lexicon sweep, broke the controller's boot loop on `isd upgrade`,
# and cost a manual `isd restore`. This gate catches that class of
# change at PR review time.
#
# Policy: a migration file is "shipped" once it exists on the base
# branch. Editing one is forbidden, even when the only diff is a
# comment, whitespace, or a typo correction. Add a new migration
# (with the next sequential number) if you need a schema change; a
# follow-up migration is the only safe way to fix a typo in
# semantic SQL too.
#
# New files (not on base) are allowed. Deleting a shipped file is
# also forbidden for the same reason: missing-file means the
# checksum lookup fails. We treat delete and modify identically.
#
# Usage:
#   scripts/ci/check_migrations_immutable.sh <base-ref>
#
# `<base-ref>` defaults to `origin/main` when omitted. CI passes
# `origin/${BASE_REF}` (which is `next` for most PRs).

set -euo pipefail

base="${1:-origin/main}"
dir="crates/isengard-storage/migrations"

if ! git rev-parse --verify --quiet "$base" >/dev/null; then
  echo "::error::Base ref '$base' is not reachable; cannot diff." >&2
  exit 2
fi

# `git diff --name-status` flags every file path that changed between
# the base and HEAD. We care about M (modified), D (deleted), R
# (renamed away from a shipped path). Added files (A) are fine: a new
# migration file is exactly what we want operators to write.
offending=$(
  git diff --name-status --diff-filter=MDR "$base"...HEAD -- "$dir" || true
)

if [[ -z "$offending" ]]; then
  echo "All migrations under $dir are untouched relative to $base."
  exit 0
fi

cat >&2 <<EOF
::error::Shipped migration files cannot be modified or deleted.

Each migration in $dir is checksummed by sqlx after its first apply;
any change to the file's bytes makes the controller refuse to boot
on the next \`isd upgrade\` (see incident note in
\`Daily/2026-05-24.md\`).

Offending paths between $base and HEAD:

EOF
printf '%s\n' "$offending" | sed 's/^/  /' >&2
cat >&2 <<'EOF'

How to fix:
  - Comment typo or doc nit -> revert the change in the shipped
    file. Leave the typo or write the prose elsewhere.
  - Schema change you intended -> add a NEW migration file with the
    next sequential number, leaving the older file untouched.
  - Genuine rename / restructure of an old migration -> not
    supported. Open a discussion before forcing this.
EOF
exit 1
