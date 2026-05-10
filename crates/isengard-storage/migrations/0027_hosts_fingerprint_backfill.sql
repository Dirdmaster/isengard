-- v0.4.1: backfill hosts.fingerprint for rows enrolled before the
-- empty-string-fingerprint bug was fixed.
--
-- Pre-fix, enrollment.rs::redeem always passed fingerprint = String::new()
-- to inventory.enroll_host. The very first enrollment on a controller
-- inserted a row with fingerprint = ''. The UNIQUE constraint on
-- hosts.fingerprint (migration 0001) then rejected the second enrollment
-- with an opaque 'enroll host' error, blocking multi-host setups.
--
-- The fix re-orders the redeem flow so the controller signs the leaf cert
-- first, then uses its SHA-256 as the host fingerprint (unique by
-- construction: random 16-byte serial per leaf). Existing controllers may
-- already have one row with an empty fingerprint; this migration replaces
-- it with a deterministic, unique sentinel so the table state is
-- consistent and a future schema audit isn't surprised by an empty value.
--
-- We can't recompute the real SHA-256 here because SQLite has no native
-- digest function and we don't want to ship a sqlite extension just for
-- one migration. 'legacy:<lowercase hex host_id>' is unique by
-- construction (host_id is the primary key) and self-documenting: anyone
-- grepping the table later sees that the value pre-dates the fix.
--
-- Idempotent: a fresh DB has zero rows with empty fingerprint, so the
-- UPDATE touches nothing.

UPDATE hosts
SET fingerprint = 'legacy:' || lower(hex(id))
WHERE fingerprint = '';
