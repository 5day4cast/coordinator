-- Existing competitions retain their original oracle entry identifiers.
CREATE TABLE automatic_payout_competitions (
    event_id TEXT PRIMARY KEY NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    payout_window_closed_at INTEGER
);

-- A profile change cannot redirect an already authorized entry.
CREATE TABLE entry_payout_policies (
    entry_id TEXT PRIMARY KEY NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    policy_json TEXT NOT NULL
);

-- Persist intent before requesting an invoice. Retrying a prepare request uses
-- the same job ID, including after a timeout or coordinator restart.
CREATE TABLE payout_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    request_json TEXT NOT NULL,
    prepared_json TEXT,
    payout_id TEXT UNIQUE REFERENCES payouts(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    retry_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    completed_at INTEGER,
    failed_at INTEGER
);
CREATE UNIQUE INDEX one_live_payout_job_per_entry
    ON payout_jobs(entry_id) WHERE failed_at IS NULL;

-- A Lightning payment cannot discharge two entry obligations.
CREATE TABLE payout_payment_hashes (
    payment_hash TEXT PRIMARY KEY NOT NULL,
    payout_id TEXT UNIQUE NOT NULL REFERENCES payouts(id) ON DELETE CASCADE
);

CREATE TABLE ticket_payout_policies (
    ticket_id TEXT PRIMARY KEY NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    ticket_hash TEXT NOT NULL,
    entry_pubkey TEXT NOT NULL,
    policy_json TEXT NOT NULL
);
CREATE TABLE payout_contract_bindings (
    event_id TEXT PRIMARY KEY NOT NULL REFERENCES competitions(id) ON DELETE CASCADE,
    binding_json TEXT NOT NULL
);
