DROP INDEX IF EXISTS tickets_unsealed;
DROP TRIGGER IF EXISTS tickets_preimage_ciphertext_stale;
ALTER TABLE tickets DROP COLUMN preimage_ciphertext;
