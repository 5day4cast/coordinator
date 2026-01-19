ALTER TABLE user ADD COLUMN username TEXT;
ALTER TABLE user ADD COLUMN password_hash TEXT;
ALTER TABLE user ADD COLUMN encrypted_nsec TEXT;

CREATE UNIQUE INDEX idx_user_username ON user(username) WHERE username IS NOT NULL;
