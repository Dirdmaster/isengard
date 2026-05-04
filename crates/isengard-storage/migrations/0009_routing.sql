-- Phase 8a: routing rules + per-host adapter config + cert metadata.
-- See docs/superpowers/specs/2026-05-03-phase-8-networking-and-proxy-design.md §6.

CREATE TABLE routing_rules (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    fleet           TEXT NOT NULL,
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    stack_id        INTEGER REFERENCES stacks(id) ON DELETE CASCADE,
    service_name    TEXT NOT NULL,
    container_port  INTEGER NOT NULL,
    public_hostname TEXT NOT NULL,
    protocol        TEXT NOT NULL CHECK (protocol IN ('http', 'tcp')),
    adapter         TEXT NOT NULL,
    tls_mode        TEXT NOT NULL CHECK (tls_mode IN ('edge', 'acme', 'manual')),
    healthcheck_path TEXT,
    healthcheck_interval_secs INTEGER NOT NULL DEFAULT 10,
    auth            TEXT,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'active', 'draining', 'failed')),
    source          TEXT NOT NULL CHECK (source IN ('ui', 'label', 'imported')),
    source_container_id TEXT,
    source_imported_from TEXT,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(public_hostname, host_id),
    CHECK (source != 'label' OR source_container_id IS NOT NULL),
    CHECK (source != 'imported' OR source_imported_from IS NOT NULL)
);

CREATE INDEX idx_routing_rules_host ON routing_rules(host_id);
CREATE INDEX idx_routing_rules_stack ON routing_rules(stack_id);
CREATE INDEX idx_routing_rules_hostname ON routing_rules(public_hostname);

CREATE TABLE routing_rule_overrides (
    routing_rule_id INTEGER NOT NULL REFERENCES routing_rules(id) ON DELETE CASCADE,
    field           TEXT NOT NULL,
    value_json      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (routing_rule_id, field)
);

CREATE TABLE adapter_config (
    host_id     BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    adapter     TEXT NOT NULL,
    config_json TEXT NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, adapter)
);

CREATE TABLE tls_certs (
    public_hostname TEXT PRIMARY KEY,
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    issuer          TEXT NOT NULL,
    not_before      TEXT NOT NULL,
    not_after       TEXT NOT NULL,
    last_renewed_at TEXT,
    next_renewal_at TEXT NOT NULL,
    serial          TEXT
);
