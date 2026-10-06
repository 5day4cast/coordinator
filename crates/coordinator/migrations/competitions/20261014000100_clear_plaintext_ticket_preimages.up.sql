-- Every release now reads tickets.preimage_ciphertext, so the plaintext copy of each sealed
-- preimage is cleared (the column is NOT NULL, so it holds ''). A row without a ciphertext keeps
-- its plaintext until the backfill seals it, and the backfill also clears the plaintext the
-- previous release writes while it runs beside this one.
UPDATE tickets SET encrypted_preimage = '' WHERE preimage_ciphertext IS NOT NULL;

-- The backfill now finds the tickets that still hold a plaintext preimage.
DROP INDEX IF EXISTS tickets_unsealed;
CREATE INDEX IF NOT EXISTS tickets_plaintext_preimage ON tickets(id) WHERE encrypted_preimage != '';
