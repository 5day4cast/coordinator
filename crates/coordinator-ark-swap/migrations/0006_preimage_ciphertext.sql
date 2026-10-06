-- Preimages sealed with a key derived from the wallet key: a random nonce, the ciphertext, then
-- the tag. Releases before this one read the plaintext `preimage` columns, which are still
-- written alongside until a later release stops writing them.
ALTER TABLE swaps ADD COLUMN preimage_ciphertext BLOB;
ALTER TABLE refunds ADD COLUMN preimage_ciphertext BLOB;
