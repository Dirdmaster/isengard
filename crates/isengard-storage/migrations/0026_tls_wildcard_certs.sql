-- Phase 8h: persist wildcard cert PEM material so a controller restart doesn't
-- lose the cert. The existing tls_certs table is metadata only (validity dates,
-- serial) and FK-constrained to hosts(id), which doesn't fit the wildcard
-- model where the cert isn't pinned to a single host.
--
-- This table is the source of truth for wildcard cert MATERIAL. Hydrated into
-- the in-memory `WildcardCertStore` at controller boot; written by the
-- scheduler's `handle_issued()` after each LE issuance / renewal.
--
-- Keyed by primary_identifier (the canonical SAN, typically the *.<domain>
-- form when both the wildcard and apex are covered).

CREATE TABLE tls_wildcard_certs (
    primary_identifier TEXT NOT NULL PRIMARY KEY,
    identifiers_json   TEXT NOT NULL,        -- JSON array: ["*.foo.com","foo.com"]
    cert_pem           TEXT NOT NULL,
    key_pem            TEXT NOT NULL,
    not_before         TEXT NOT NULL,
    not_after          TEXT NOT NULL,
    serial             TEXT NOT NULL,
    issuer             TEXT NOT NULL,
    created_at         TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at         TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
