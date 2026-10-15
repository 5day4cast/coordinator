-- Reserve each lookup before contacting Arkade. A restart or lease handover must
-- not turn a failed payment or refund into a fresh one-second polling loop.
CREATE TABLE indexer_backoff (
    purpose TEXT NOT NULL,
    id TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    retry_after INTEGER NOT NULL,
    PRIMARY KEY (purpose, id)
);
