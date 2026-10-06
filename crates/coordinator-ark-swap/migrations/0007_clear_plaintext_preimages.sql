-- Every preimage that has a sealed copy keeps only that. `swaps.preimage` cannot be null, so it is
-- left empty. Rows not yet sealed keep their plaintext until ark-swapd seals them.
UPDATE swaps SET preimage = '' WHERE preimage_ciphertext IS NOT NULL;
UPDATE refunds SET preimage = NULL WHERE preimage_ciphertext IS NOT NULL;
