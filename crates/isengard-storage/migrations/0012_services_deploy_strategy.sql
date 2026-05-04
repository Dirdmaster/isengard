-- Phase 10g: per-service deploy strategy override.
-- Values: NULL or 'auto' = auto-detect (default), 'blue-green' or 'in-place' = explicit override.

ALTER TABLE services ADD COLUMN deploy_strategy_override TEXT
    CHECK (deploy_strategy_override IS NULL OR deploy_strategy_override IN ('auto', 'blue-green', 'in-place'));
