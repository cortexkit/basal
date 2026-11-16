ALTER TABLE outbox ADD COLUMN bus_published_at INTEGER;
ALTER TABLE outbox ADD COLUMN bus_attempts INTEGER NOT NULL DEFAULT 0 CHECK (bus_attempts >= 0);
ALTER TABLE outbox ADD COLUMN bus_retry_at INTEGER NOT NULL DEFAULT 0;

-- Retained owner notices predate bus delivery; replaying them would report old failures.
UPDATE outbox SET bus_published_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE kind = 'flow.owner_notice';

CREATE INDEX outbox_bus_pending ON outbox(bus_retry_at, seq)
WHERE bus_published_at IS NULL AND kind IN ('flow.owner_notice', 'codemode.late_result_ready');
