-- Phase 14 follow-up (Imp-4): track cancelled enrollment tokens distinctly
-- from consumed ones so the audit trail isn't polluted by sentinel host_ids.
--
-- Pre-fix the dashboard's "revoke token" handler called
-- consume_enrollment_token with HostId::new() as the consumed_by, faking a
-- consumption to lock the token out. Adding a dedicated cancelled_at column
-- lets us distinguish "an agent redeemed this token (audit who)" from
-- "an operator cancelled this unused invitation".
--
-- SQLite ALTER TABLE ADD COLUMN doesn't require a table rebuild; safe with
-- the partial-index on consumed_at because we don't touch consumed_at.

ALTER TABLE enrollment_tokens ADD COLUMN cancelled_at TEXT;
