-- Add keymeld_keygen_completed_at to track when keymeld keygen completed
-- This is used to determine the AwaitingSignatures state in the keymeld flow
ALTER TABLE competitions ADD COLUMN keymeld_keygen_completed_at DATETIME;
