ALTER TABLE user ADD COLUMN display_name TEXT;

CREATE UNIQUE INDEX idx_user_display_name ON user(display_name COLLATE NOCASE) WHERE display_name IS NOT NULL;
