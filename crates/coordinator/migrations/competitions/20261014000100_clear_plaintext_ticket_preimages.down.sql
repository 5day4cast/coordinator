-- The cleared plaintext preimages are not restored: a release that reads only the plaintext
-- column cannot run on this database again.
DROP INDEX IF EXISTS tickets_plaintext_preimage;
CREATE INDEX IF NOT EXISTS tickets_unsealed ON tickets(id) WHERE preimage_ciphertext IS NULL;
