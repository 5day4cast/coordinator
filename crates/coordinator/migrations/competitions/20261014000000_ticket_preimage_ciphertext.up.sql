-- A ticket's preimage sealed with the coordinator's ticket key (see ticket_preimage.rs):
-- nonce || AES-256-GCM ciphertext || tag, bound to the ticket's id and hash. NULL until sealed.
-- `encrypted_preimage` is not encrypted, despite its name: it holds the preimage as plaintext hex
-- for releases that predate this column, and is cleared once every release reads this one.
ALTER TABLE tickets ADD COLUMN preimage_ciphertext BLOB;

-- A release that predates this column rotates a ticket's preimage and hash without touching the
-- ciphertext. The stale ciphertext is dropped, so the new plaintext is read until the backfill
-- seals it again.
CREATE TRIGGER tickets_preimage_ciphertext_stale AFTER UPDATE OF hash ON tickets
WHEN NEW.hash IS NOT OLD.hash AND NEW.preimage_ciphertext IS OLD.preimage_ciphertext
BEGIN
    UPDATE tickets SET preimage_ciphertext = NULL WHERE id = NEW.id;
END;

-- The backfill finds the tickets still to seal.
CREATE INDEX IF NOT EXISTS tickets_unsealed ON tickets(id) WHERE preimage_ciphertext IS NULL;
