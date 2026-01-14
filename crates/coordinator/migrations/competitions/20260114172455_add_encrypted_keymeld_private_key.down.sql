-- SQLite doesn't support DROP COLUMN directly in older versions
-- For SQLite 3.35.0+ we can use:
ALTER TABLE entries DROP COLUMN keymeld_auth_pubkey;
ALTER TABLE entries DROP COLUMN encrypted_keymeld_private_key;
