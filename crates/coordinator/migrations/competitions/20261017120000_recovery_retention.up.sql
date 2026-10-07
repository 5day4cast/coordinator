-- Retention of recovery records (docs/RECOVERY.md, "Retention"). Times are Unix seconds.
--
-- `settled_at`: when the publisher first found the money a record describes settled; NULL while
-- any of it is held. `deleted_at`: the `created_at` of the NIP-09 deletion (kind 5) that retired
-- the record; `deletion_json` is that event, published like any other, and
-- `deletion_published_at` is set once every relay took it. A retired row keeps its last version
-- in `event_json` and `published_at` set, so an older coordinator neither offers it again nor
-- leaves it out of a recovery file.
ALTER TABLE recovery_outbox ADD COLUMN settled_at INTEGER;
ALTER TABLE recovery_outbox ADD COLUMN deleted_at INTEGER;
ALTER TABLE recovery_outbox ADD COLUMN deletion_json TEXT;
ALTER TABLE recovery_outbox ADD COLUMN deletion_published_at INTEGER;
CREATE INDEX recovery_outbox_settled ON recovery_outbox(settled_at)
    WHERE settled_at IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX recovery_outbox_deletions_due ON recovery_outbox(next_attempt_at)
    WHERE deleted_at IS NOT NULL AND deletion_published_at IS NULL;
