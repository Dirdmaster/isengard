-- 0030: drop the Phase 0.13 manifest layer.
--
-- The one-file stack model (Track A, 2026-05-15) lands the stack name
-- in `stacks.name` (already exists), per-service strategy in the
-- compose body's `services.<name>.strategy` key (parsed by the agent),
-- and secrets in the compose body's native `secrets:` block (still
-- bound via stack_secrets, which survives).
--
-- The manifest TOML, sha256, import timestamp, fleet name, stack-level
-- deploy strategy override, and lifecycle hooks all disappear.
--
-- ALTER TABLE DROP COLUMN requires SQLite 3.35.0+; the bundled
-- libsqlite3-sys 0.30 ships 3.46.

DROP TABLE IF EXISTS stack_hooks;

ALTER TABLE stacks DROP COLUMN manifest_toml;
ALTER TABLE stacks DROP COLUMN manifest_sha256;
ALTER TABLE stacks DROP COLUMN manifest_imported_at;
ALTER TABLE stacks DROP COLUMN manifest_fleet;
ALTER TABLE stacks DROP COLUMN deploy_strategy;
