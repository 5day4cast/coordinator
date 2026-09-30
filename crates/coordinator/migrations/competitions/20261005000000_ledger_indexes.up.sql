-- A player's entries page reads their entries, and each entry's payouts, in one query.
CREATE INDEX IF NOT EXISTS entries_by_pubkey ON entries (pubkey);
CREATE INDEX IF NOT EXISTS payouts_by_entry ON payouts (entry_id);
