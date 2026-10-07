DROP INDEX IF EXISTS recovery_outbox_deletions_due;
DROP INDEX IF EXISTS recovery_outbox_settled;
ALTER TABLE recovery_outbox DROP COLUMN deletion_published_at;
ALTER TABLE recovery_outbox DROP COLUMN deletion_json;
ALTER TABLE recovery_outbox DROP COLUMN deleted_at;
ALTER TABLE recovery_outbox DROP COLUMN settled_at;
