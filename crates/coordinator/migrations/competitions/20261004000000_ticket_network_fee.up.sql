-- A ticket's share of the Bitcoin network fees, added to its invoice. It is fixed for one
-- payment hash, `network_fee_hash`, when that hash first gets an escrow policy or an invoice,
-- and never changes afterwards; a ticket whose hash rotates gets a fresh fee. Tickets from
-- before the fee were priced without one.
ALTER TABLE tickets ADD COLUMN network_fee_sats INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tickets ADD COLUMN network_fee_hash TEXT;

-- The latest kickoff check of an Arkade competition or pool: whether what its entries paid beyond
-- the pot covers its chain cost at the kickoff fee rate, and the Lightning allowance. A pool
-- that fails it is cancelled and refunded; one that passes builds its contract at the rate
-- checked.
CREATE TABLE competition_kickoff_checks (
    competition_id TEXT PRIMARY KEY NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    check_json TEXT NOT NULL,
    checked_at DATETIME NOT NULL
);
