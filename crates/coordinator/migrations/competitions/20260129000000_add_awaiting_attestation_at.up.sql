-- Add awaiting_attestation_at column to track when competition started waiting for oracle attestation
ALTER TABLE competitions ADD COLUMN awaiting_attestation_at TEXT;
