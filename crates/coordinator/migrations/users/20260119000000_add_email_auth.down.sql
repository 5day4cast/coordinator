-- Remove email authentication fields
DROP INDEX IF EXISTS idx_user_email;
ALTER TABLE user DROP COLUMN encrypted_nsec;
ALTER TABLE user DROP COLUMN password_hash;
ALTER TABLE user DROP COLUMN email;
