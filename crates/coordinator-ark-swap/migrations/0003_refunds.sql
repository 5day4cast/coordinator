-- A refund swap: the VTXO an unused entry escrow is refunded into, on its way to the player's
-- Lightning Address. The service mints it here, the coordinator pays the invoice, and the
-- service then claims the VTXO with the preimage that payment revealed.
CREATE TABLE refunds (
    id TEXT PRIMARY KEY NOT NULL,
    -- The invoice's payment hash, which the swap's claim leaf commits to. One swap per hash, so
    -- a retried request returns the swap already minted for it.
    payment_hash TEXT NOT NULL UNIQUE,
    amount_sat INTEGER NOT NULL,
    -- The player's entry key, which reclaims the swap if this service never pays.
    player_key TEXT NOT NULL,
    -- UNIX seconds. After this the player may take the swap back.
    deadline INTEGER NOT NULL,
    -- The swap's PSBT `TapTree` field, hex, and its Ark address.
    swap_tap_tree TEXT NOT NULL,
    swap_address TEXT NOT NULL,
    state TEXT NOT NULL,
    -- Set once the coordinator reports paying the invoice.
    preimage TEXT,
    -- The VTXO the refund paid, and the transaction that claimed it.
    swap_vtxo TEXT,
    claim_txid TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX refunds_by_state ON refunds (state);
