-- Phase 0.15: kill fleets.
--
-- The fleet concept is being removed end-to-end (see spec
-- 2026-05-15-isengard-kill-fleets-design.md). Operators recover grouping
-- via host labels and placement selectors (`where = "fleet==lab"`).
--
-- This migration:
-- 1. Copies every non-empty `hosts.fleet` value into `agent_labels`
--    under key 'fleet' so operators retain their grouping intent.
-- 2. Copies every `routing_rules.fleet` value (via the rule's host_id)
--    into the same agent_labels table. routing_rules.fleet is then
--    redundant with the host label, and we drop the column.
-- 3. Drops `hosts.fleet`, `routing_rules.fleet`, `stacks.manifest_fleet`,
--    and the `fleets` table itself.
--
-- Numbering: 0030 is taken by `0030_placements.sql` on the placement
-- scheduler branch (and by `0030_drop_manifest_layer.sql` on the
-- manifest-teardown branch; that collision is owned by whoever lands
-- second). This migration sits at 0031 since it stacks on the
-- placement-scheduler branch.
--
-- Note on `policies.scope_type` CHECK constraint: still allows 'fleet'.
-- The kill-fleets plan does not touch the policies table; a future
-- cleanup can rebuild the table to drop that scope. Existing rows with
-- scope_type='fleet' (if any) remain queryable but are now orphans.

-- 1. Migrate hosts.fleet -> agent_labels.
INSERT OR IGNORE INTO agent_labels (host_id, key, value)
SELECT id, 'fleet', fleet
  FROM hosts
 WHERE fleet IS NOT NULL AND fleet != '';

-- 2. Migrate routing_rules.fleet -> agent_labels (skip rows whose host
--    already has a fleet label from step 1).
INSERT OR IGNORE INTO agent_labels (host_id, key, value)
SELECT DISTINCT host_id, 'fleet', fleet
  FROM routing_rules
 WHERE fleet IS NOT NULL AND fleet != '';

-- 3. Drop the fleet-related indexes first so SQLite can DROP COLUMN
--    without complaining about dangling index definitions.
DROP INDEX IF EXISTS idx_hosts_fleet;

-- 4. Drop the fleet columns. SQLite 3.35+ supports ALTER TABLE DROP
--    COLUMN directly (same prerequisite as migration 0030).
ALTER TABLE hosts DROP COLUMN fleet;
ALTER TABLE routing_rules DROP COLUMN fleet;
ALTER TABLE stacks DROP COLUMN manifest_fleet;

-- 5. Drop the now-unused fleets table.
DROP TABLE IF EXISTS fleets;
