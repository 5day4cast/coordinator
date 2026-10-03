-- When every pool a queued competition formed had finished: completed, or cancelled before its
-- contract was funded. The queue's own leftover tickets are refunded as before; this only marks
-- that it has nothing left to run.
ALTER TABLE competitions ADD COLUMN pools_finished_at TEXT;
