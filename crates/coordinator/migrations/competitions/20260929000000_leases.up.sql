-- Work that only one coordinator process may do at a time, such as driving a competition.
-- Two processes share this database during a blue/green deploy. `token` rises each time
-- another holder takes the lease, so a write fenced on it fails for a holder that lost it.
CREATE TABLE IF NOT EXISTS leases (
    resource TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    token INTEGER NOT NULL,
    -- UNIX milliseconds.
    expires_at INTEGER NOT NULL
);
