-- Refuse to discard live manifests. Convert every format-2 checkpoint before rollback.
CREATE TEMP TABLE protocol_parts_rollback_guard (remaining INTEGER CHECK (remaining = 0));
INSERT INTO protocol_parts_rollback_guard
SELECT COUNT(*) FROM keymeld_protocol_state WHERE format != 1;
DROP TABLE protocol_parts_rollback_guard;
DROP TABLE keymeld_protocol_parts;
ALTER TABLE keymeld_protocol_state DROP COLUMN format;
