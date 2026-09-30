-- How many times in a row a refund was minted again while its escrow's recovery in a batch kept
-- failing. Each such remint waits twice as long as the one before, up to a day, so the player's
-- Lightning Address provider is not asked for an invoice every few hours while Arkade fails.
ALTER TABLE ticket_ark_refunds ADD COLUMN recovery_remints INTEGER NOT NULL DEFAULT 0;
