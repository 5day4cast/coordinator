-- The refund of a funded Arkade escrow whose competition never kicked off. It moves through one
-- state at a time so an outage resumes where it stopped, and never pays a player twice.
CREATE TABLE ticket_ark_refunds (
    ticket_id TEXT PRIMARY KEY NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    -- The swap ark-swapd minted for this refund, and what the player is paid through it.
    refund_id TEXT NOT NULL,
    invoice TEXT NOT NULL,
    payment_hash TEXT NOT NULL,
    fee_sats INTEGER NOT NULL,
    -- minted, submitted, paid, settled
    state TEXT NOT NULL,
    -- The Arkade transaction that moved the escrow into the swap.
    ark_txid TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX ticket_ark_refunds_by_state ON ticket_ark_refunds (state);
