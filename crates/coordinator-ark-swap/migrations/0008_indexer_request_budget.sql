-- A shared budget for every caller using this swapd, retained across restarts.
CREATE TABLE indexer_request_budget (
    id INTEGER PRIMARY KEY,
    requested_at_ms INTEGER NOT NULL
);
CREATE INDEX indexer_request_budget_time ON indexer_request_budget(requested_at_ms);
