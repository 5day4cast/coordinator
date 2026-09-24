CREATE TABLE swaps (
    id TEXT PRIMARY KEY NOT NULL,
    escrow_address TEXT NOT NULL,
    amount_sat INTEGER NOT NULL,
    payment_hash TEXT NOT NULL UNIQUE,
    preimage TEXT NOT NULL,
    invoice TEXT NOT NULL,
    state TEXT NOT NULL,
    escrow_vtxo TEXT,
    ark_txid TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX swaps_by_escrow ON swaps (escrow_address);
CREATE INDEX swaps_by_state ON swaps (state);
