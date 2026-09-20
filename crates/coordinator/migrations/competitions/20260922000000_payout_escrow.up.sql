-- Enclave-escrowed payouts: the policy sealed in the entry's Keymeld
-- registration (address + payee node), the authorized signing batch the
-- market maker must present to claim a preimage, and what each payout was
-- paid to, kept for the proof of payment.
ALTER TABLE entries ADD COLUMN payout_policy TEXT;
ALTER TABLE competitions ADD COLUMN signing_receipt TEXT;
ALTER TABLE payouts ADD COLUMN lightning_address TEXT;
ALTER TABLE payouts ADD COLUMN lnurl_metadata TEXT;
