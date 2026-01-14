-- Add encrypted_keymeld_private_key column to entries table
-- This stores the user's ephemeral private key encrypted to the keymeld enclave's public key
-- Used for server-side keymeld registration
ALTER TABLE entries ADD COLUMN encrypted_keymeld_private_key TEXT;

-- Add keymeld_auth_pubkey column to entries table
-- This stores the user's auth public key derived from their ephemeral private key
-- Used for keymeld session authentication during server-side registration
ALTER TABLE entries ADD COLUMN keymeld_auth_pubkey TEXT;
