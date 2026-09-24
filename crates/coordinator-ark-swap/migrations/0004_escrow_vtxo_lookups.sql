-- A swap can settle before the indexer lists the escrow VTXO it paid. Such a swap is looked up
-- again on a backoff, apart from the live swaps, until the VTXO is found or the service gives up.
ALTER TABLE swaps ADD COLUMN vtxo_lookups INTEGER NOT NULL DEFAULT 0;
-- UNIX seconds. The next lookup is due then; NULL means at once.
ALTER TABLE swaps ADD COLUMN vtxo_lookup_after INTEGER;
-- UNIX seconds. Set when the service stops looking; the swap then needs an operator.
ALTER TABLE swaps ADD COLUMN vtxo_lookup_gave_up_at INTEGER;
