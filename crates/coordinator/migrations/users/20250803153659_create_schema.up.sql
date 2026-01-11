CREATE TABLE IF NOT EXISTS user (
    nostr_pubkey TEXT NOT NULL UNIQUE PRIMARY KEY,          -- Login via verifying a random hash being signed
    encrypted_bitcoin_private_key TEXT NOT NULL UNIQUE,     -- User encrypted bitcoin key for dlctix wallet
    network TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
