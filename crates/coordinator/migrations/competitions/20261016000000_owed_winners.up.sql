-- Winners the coordinator owes: their Lightning payout window closed with them unpaid. Their split
-- output is swept to the coordinator's key only once an operator approves (`sweep_approved_at`),
-- and they stay owed until an operator records paying them (`settled_at`) or they claim the output
-- on chain themselves (`claimed_on_chain_at`). One row per entry, written once.
CREATE TABLE owed_winners (
    entry_id TEXT PRIMARY KEY NOT NULL,
    competition_id TEXT NOT NULL,
    amount_sats INTEGER NOT NULL,
    owed_since TEXT NOT NULL,
    -- The first block at which the coordinator's reclaim path on the output opens.
    sweepable_at_height INTEGER,
    sweep_approved_at TEXT,
    claimed_on_chain_at TEXT,
    claim_txid TEXT,
    settled_at TEXT,
    settled_note TEXT
);
CREATE INDEX owed_winners_by_competition ON owed_winners (competition_id);

-- A winner's split output the coordinator found spent by a transaction it has no record of making:
-- the winner's own claim, or its own sweep whose record was lost. Settlement counts the output as
-- handled instead of retrying a sweep that can never confirm.
ALTER TABLE entries ADD COLUMN split_output_spent_at DATETIME;
