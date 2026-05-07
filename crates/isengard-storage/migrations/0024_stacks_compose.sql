-- v0.3c compose import: persist the YAML the agent reverse-engineers
-- from the running containers in a stack. Nullable; populated by
-- `StackComposeReport` AgentMessages and surfaced via
-- `GET /api/v1/stacks/<id>/compose`.

ALTER TABLE stacks ADD COLUMN compose_yaml TEXT;
ALTER TABLE stacks ADD COLUMN compose_sha256 TEXT;
ALTER TABLE stacks ADD COLUMN compose_imported_at TEXT;
