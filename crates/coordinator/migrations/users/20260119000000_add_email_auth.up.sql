-- Add email authentication fields
-- These are nullable because existing users use Nostr extension auth only
-- Note: SQLite doesn't support adding UNIQUE constraint with ALTER TABLE,
-- so we use a UNIQUE INDEX instead
ALTER TABLE user ADD COLUMN email TEXT;
ALTER TABLE user ADD COLUMN password_hash TEXT;
ALTER TABLE user ADD COLUMN encrypted_nsec TEXT;

-- Unique index for email (enforces uniqueness and speeds up lookups)
CREATE UNIQUE INDEX idx_user_email ON user(email) WHERE email IS NOT NULL;
