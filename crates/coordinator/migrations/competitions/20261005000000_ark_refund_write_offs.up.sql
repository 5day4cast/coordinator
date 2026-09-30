-- An operator's decision that a funded Arkade escrow will not be refunded: its refund can never
-- finish, for instance because its player never sent the registration that signs it. Cleanup
-- skips a written-off escrow, and the pages no longer count it as owed. Keyed by the ticket's
-- hash like its escrow, so a recycled ticket's new escrow is not written off with the old one.
CREATE TABLE ticket_ark_refund_write_offs (
    ticket_id TEXT PRIMARY KEY NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    ticket_hash TEXT NOT NULL,
    reason TEXT NOT NULL,
    -- The refund's state when it was written off; NULL when none was ever minted.
    refund_state TEXT,
    -- UNIX seconds.
    written_off_at INTEGER NOT NULL
);
