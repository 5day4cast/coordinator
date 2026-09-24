-- Each escrow payment is recorded before it is sent. A swap whose last payment's outcome is
-- unknown (a crash after sending, or a send that failed after arkd took it) is reconciled
-- against Arkade before it is paid again, so an escrow is never paid twice.
-- UNIX seconds.
ALTER TABLE swaps ADD COLUMN pay_attempted_at INTEGER;
ALTER TABLE swaps ADD COLUMN pay_attempts INTEGER NOT NULL DEFAULT 0;
