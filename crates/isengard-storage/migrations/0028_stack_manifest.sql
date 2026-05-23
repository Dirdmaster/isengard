-- Phase 0.13: stack.toml manifest persistence.
--
-- Adds:
-- 1. Five nullable columns to `stacks` for the verbatim manifest body,
--    its sha256 hash + import timestamp, the manifest-level deploy
--    strategy override, and the captured fleet name.
-- 2. A `stack_secrets` table binding per-fleet secret names to stacks.
--    Names reference the `secrets` table introduced in 0025; ON DELETE
--    RESTRICT on the secret side forces operators to unbind a secret
--    before they can drop it.
-- 3. A `stack_hooks` table holding lifecycle hooks (pre-deploy,
--    post-deploy, failure). `cmd_json` is the argv list as a JSON
--    string. Read as one bundle per deploy; relational normalization
--    would force a join with no upside.
--
-- All new columns are nullable; existing rows survive unchanged. The
-- migration runs in a single SQLite transaction.

-- 1. Manifest body + metadata on the existing stacks row.
ALTER TABLE stacks ADD COLUMN manifest_toml TEXT;
ALTER TABLE stacks ADD COLUMN manifest_sha256 TEXT;
ALTER TABLE stacks ADD COLUMN manifest_imported_at TEXT;

-- Manifest-level deploy strategy. NULL = inherit per-service from
-- phase 10g labels (today's default). When set, the agent treats every
-- service in the stack as having this strategy unless the compose
-- itself pins a per-service override.
ALTER TABLE stacks ADD COLUMN deploy_strategy TEXT
    CHECK (deploy_strategy IS NULL
        OR deploy_strategy IN ('auto','blue-green','rolling','recreate'));

-- Source fleet binding from the manifest. Nullable. Future phases use
-- this to filter `isd deploy --all` against the operator's saved
-- contexts; phase 0.13 captures the value but does not consume it.
ALTER TABLE stacks ADD COLUMN manifest_fleet TEXT;

-- 2. Per-stack secret bindings.
CREATE TABLE stack_secrets (
    stack_id    INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    secret_name TEXT    NOT NULL REFERENCES secrets(name) ON DELETE RESTRICT,
    bound_at    TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (stack_id, secret_name)
);

CREATE INDEX idx_stack_secrets_secret ON stack_secrets(secret_name);

-- 3. Per-stack lifecycle hooks.
CREATE TABLE stack_hooks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    stack_id    INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    on_event    TEXT    NOT NULL
                CHECK (on_event IN ('pre-deploy','post-deploy','failure')),
    cmd_json    TEXT    NOT NULL,
    timeout_ms  INTEGER NOT NULL DEFAULT 60000,
    on_error    TEXT    NOT NULL DEFAULT 'abort'
                CHECK (on_error IN ('abort','continue')),
    ordinal     INTEGER NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_stack_hooks_stack_event
    ON stack_hooks(stack_id, on_event, ordinal);
