-- A refund is submitted to Arkade before it is finalized, and only the owner can sign the
-- checkpoint the server hands back. Keeping that signed checkpoint lets a refund interrupted
-- between the two finish without signing or spending the escrow again.
ALTER TABLE ticket_ark_refunds ADD COLUMN checkpoint_psbt TEXT;
