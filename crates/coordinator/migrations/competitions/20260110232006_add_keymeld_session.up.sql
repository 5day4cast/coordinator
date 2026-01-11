-- Add keymeld session storage to competitions table
-- Stores the DLC keygen session data for remote MuSig2 signing via Keymeld
ALTER TABLE competitions ADD COLUMN keymeld_session BLOB;
