-- Queued competitions. A queued competition takes entries without a seat count. When
-- registration closes it splits its complete tickets into pools, and each pool is a competition
-- of its own that runs the usual lifecycle. See docs/QUEUED_COMPETITIONS.md.
--
-- kind: 'single' for every competition before this, 'queued' for the competition players enter,
-- 'pool' for each competition it forms. A pool names its queued competition and its index.
ALTER TABLE competitions ADD COLUMN kind TEXT NOT NULL DEFAULT 'single'
    CHECK (kind IN ('single', 'queued', 'pool'));
ALTER TABLE competitions ADD COLUMN parent_id TEXT;
ALTER TABLE competitions ADD COLUMN pool_index INTEGER;
-- When a queued competition formed its pools. It has no lifecycle of its own after that.
ALTER TABLE competitions ADD COLUMN pools_formed_at DATETIME;
CREATE INDEX IF NOT EXISTS competitions_by_parent ON competitions (parent_id);

-- What every player of a queued competition consents to. `terms_json` is the exact QueuedTerms
-- the players are given, and `terms_digest` its digest, which their key deposits are sealed under.
CREATE TABLE queued_competitions (
    competition_id TEXT PRIMARY KEY NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    pool_rules TEXT NOT NULL,
    stake_sats INTEGER NOT NULL CHECK (stake_sats > 0),
    max_entries INTEGER NOT NULL DEFAULT 500 CHECK (max_entries > 0),
    terms_json TEXT NOT NULL,
    terms_digest TEXT NOT NULL
);

-- One row per pool a queued competition formed: the seed's inputs (the closing block and every
-- ticket placed), and the pool's members, so anyone can recompute the pools.
CREATE TABLE competition_pools (
    competition_id TEXT PRIMARY KEY NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    parent_id TEXT NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    pool_index INTEGER NOT NULL CHECK (pool_index >= 0),
    close_height INTEGER NOT NULL,
    block_hash TEXT NOT NULL,
    -- Every ticket the kickoff placed, sorted.
    tickets_json TEXT NOT NULL,
    -- This pool's tickets, sorted.
    members_json TEXT NOT NULL,
    -- UNIX seconds.
    formed_at INTEGER NOT NULL,
    UNIQUE (parent_id, pool_index)
);
