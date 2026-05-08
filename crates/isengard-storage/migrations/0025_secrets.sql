-- v0.3.6 secrets store: Isengard-managed encrypted secrets distributed to
-- agents over mTLS and mounted as tmpfs in operator containers. The
-- ciphertext column holds the binary age-encrypted bytes; the controller
-- derives the key from `ISENGARD_SECRETS_PASSPHRASE` on boot. Plaintext
-- never touches disk.
CREATE TABLE secrets (
    name        TEXT PRIMARY KEY,
    ciphertext  BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Free-form attribution: agent_id / "operator" / etc. Optional so
    -- programmatic callers without an identity can omit it.
    created_by  TEXT
);
