-- Competitions funded from Arkade escrows, and the batch that funded each pool.
-- The commitment transaction fixes the funding outpoint that settlement presents to Keymeld.
CREATE TABLE ark_funded_competitions (
    event_id TEXT PRIMARY KEY NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    batch_id TEXT,
    commitment_tx TEXT,
    funding_vout INTEGER
);

-- A ticket's Arkade escrow, fixed with its payout policy. A recycled ticket has a new hash,
-- and so a new escrow.
CREATE TABLE ticket_ark_escrows (
    ticket_id TEXT PRIMARY KEY NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    ticket_hash TEXT NOT NULL,
    escrow_tap_tree TEXT NOT NULL,
    escrow_address TEXT NOT NULL,
    swap_id TEXT,
    vtxo_outpoint TEXT,
    vtxo_sats INTEGER,
    funded_at INTEGER
);
