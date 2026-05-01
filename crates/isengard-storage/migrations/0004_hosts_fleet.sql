ALTER TABLE hosts ADD COLUMN fleet TEXT NOT NULL DEFAULT 'default';
CREATE INDEX idx_hosts_fleet ON hosts(fleet);
