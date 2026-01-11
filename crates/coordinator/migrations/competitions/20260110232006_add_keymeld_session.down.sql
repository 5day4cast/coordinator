-- Remove keymeld session column
-- Note: SQLite doesn't support DROP COLUMN directly in older versions
-- This requires recreating the table without the column
-- For simplicity, we just document that reverting requires a full rebuild
ALTER TABLE competitions DROP COLUMN keymeld_session;
