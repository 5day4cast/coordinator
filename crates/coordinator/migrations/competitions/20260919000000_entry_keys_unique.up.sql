-- Entry keys and payout preimages are now re-derived in the browser from the
-- wallet seed and the entry id, so the encrypted copies are no longer stored.
ALTER TABLE entries DROP COLUMN ephemeral_privatekey_encrypted;
ALTER TABLE entries DROP COLUMN payout_preimage_encrypted;

-- A ticketed DLC needs a distinct key and payout hash per player. Enforcing it
-- here stops an entry from reusing another entry's key or hash.
CREATE UNIQUE INDEX IF NOT EXISTS entries_ephemeral_pubkey_unique ON entries (ephemeral_pubkey);
CREATE UNIQUE INDEX IF NOT EXISTS entries_payout_hash_unique ON entries (payout_hash);
