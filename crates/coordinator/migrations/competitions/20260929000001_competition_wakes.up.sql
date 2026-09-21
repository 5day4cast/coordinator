-- Wakes one coordinator sends to competitions another may drive, as during a blue/green deploy.
-- Each process polls rows after the last it saw and wakes its own runners.
CREATE TABLE IF NOT EXISTS competition_wakes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    competition_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    -- UNIX seconds.
    woken_at INTEGER NOT NULL
);
