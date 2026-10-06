-- Lightning payouts the coordinator will not send until an operator looks: after a restore,
-- LND reported a payment the database had no record of, for the amount this entry is owed.
-- `released_at` is set once the operator releases it; a later reconciliation keeps it released.
CREATE TABLE payout_holds (
    entry_id TEXT NOT NULL,
    payment_hash TEXT NOT NULL,
    amount_sats INTEGER NOT NULL,
    reason TEXT NOT NULL,
    held_at TEXT NOT NULL,
    released_at TEXT,
    PRIMARY KEY (entry_id, payment_hash)
);
