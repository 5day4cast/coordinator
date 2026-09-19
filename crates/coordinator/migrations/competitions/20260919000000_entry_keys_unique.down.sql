DROP INDEX IF EXISTS entries_payout_hash_unique;
DROP INDEX IF EXISTS entries_ephemeral_pubkey_unique;
ALTER TABLE entries ADD COLUMN payout_preimage_encrypted TEXT NOT NULL DEFAULT '';
ALTER TABLE entries ADD COLUMN ephemeral_privatekey_encrypted TEXT NOT NULL DEFAULT '';
