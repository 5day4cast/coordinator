-- Preimage LND returns when a payout invoice settles: proof that the winner
-- was paid, kept for the payout audit trail and the enclave-escrowed release.
ALTER TABLE payouts ADD COLUMN payment_preimage TEXT;
