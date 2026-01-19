-- SQLite doesn't support DROP COLUMN directly, but this migration is safe to leave
-- as the column will simply be ignored if not used
ALTER TABLE competitions DROP COLUMN invoices_settled_at;
