Pure policy resolver: walks layered scopes to produce a [`ResolvedPolicy`]
with field-level provenance.

See spec §"Resolver" of
`docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.

This module is intentionally storage-free. The caller (typically the
updater plugin or a REST handler) loads `PolicyRow` values from
`isengard-storage`, projects them down to `(PolicyScopeType, scope_key,
&Policy)` tuples, then hands them to [`resolve_policy`] together with a
[`PolicyContext`] describing the target.

Resolution algorithm:

1. Filter rows that apply to the context (a `Fleet` row only applies if
   `ctx.fleet == Some(scope_key)`, and so on; `Global` always applies).
2. Sort survivors by `scope_type.rank()` ascending so more specific scopes
   overwrite less specific ones.
3. For each policy field, walk rows in rank order and overwrite whenever
   the row's field is `Some`. Update provenance to the row's origin.
4. Any field still unset falls back to the `defaults::DEFAULT_*` constant
   with origin `Default`.
