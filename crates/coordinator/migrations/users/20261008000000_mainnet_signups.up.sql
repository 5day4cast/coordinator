CREATE TABLE mainnet_signups (
    email TEXT PRIMARY KEY COLLATE NOCASE NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX mainnet_signups_created_at ON mainnet_signups(created_at DESC, email);
