-- Phase 14: Auth & Identity. CA, enrollment tokens, per-agent certs.
-- See docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md

CREATE TABLE ca (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    root_cert_pem   TEXT NOT NULL,
    root_key_pem    TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE enrollment_tokens (
    token_hash      BLOB PRIMARY KEY,
    role            TEXT NOT NULL CHECK (role IN ('agent')),
    expires_at      TEXT NOT NULL,
    consumed_at     TEXT,
    consumed_by     BLOB,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_enrollment_tokens_active
    ON enrollment_tokens(expires_at)
    WHERE consumed_at IS NULL;

CREATE TABLE agent_certs (
    serial          BLOB PRIMARY KEY,
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    cert_pem        TEXT NOT NULL,
    issued_at       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT NOT NULL,
    revoked_at      TEXT,
    revoke_reason   TEXT
);

CREATE INDEX idx_agent_certs_host_active
    ON agent_certs(host_id, issued_at DESC)
    WHERE revoked_at IS NULL;
