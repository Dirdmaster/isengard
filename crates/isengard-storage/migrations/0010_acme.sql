-- Phase 8e-1: extend tls_certs for ACME state + create the ACME account singleton.
-- See docs/superpowers/specs/2026-05-03-phase-8e-8g-tls-and-adapters-design.md §7.

ALTER TABLE tls_certs ADD COLUMN last_attempt_at TEXT;
ALTER TABLE tls_certs ADD COLUMN last_error TEXT;
ALTER TABLE tls_certs ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE acme_account (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    contact_email   TEXT NOT NULL,
    directory_url   TEXT NOT NULL,
    account_key_pem TEXT NOT NULL,
    kid             TEXT,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
