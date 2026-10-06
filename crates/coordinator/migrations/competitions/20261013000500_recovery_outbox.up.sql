-- Recovery records waiting to reach the Nostr relays, one row per replaceable event (its `d`
-- tag). A new version of a record replaces the row and is published again. `content_sha256` is
-- the digest of the record's plaintext, so an unchanged record is not encrypted and sent again.
-- `accepted_relays` is a JSON list of the relays that took this version. `published_at` is set
-- once every relay took it (or it ran out of attempts with at least one); until then the row is
-- tried again at `next_attempt_at`. Times are Unix seconds. See docs/RECOVERY.md.
CREATE TABLE recovery_outbox (
    d_tag TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('wallet', 'entry', 'competition')),
    user_pubkey TEXT,
    competition_id TEXT,
    content_sha256 TEXT NOT NULL,
    event_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    accepted_relays TEXT NOT NULL DEFAULT '[]',
    published_at INTEGER,
    last_error TEXT
);
CREATE INDEX recovery_outbox_due ON recovery_outbox(next_attempt_at) WHERE published_at IS NULL;
CREATE INDEX recovery_outbox_user ON recovery_outbox(user_pubkey);
CREATE INDEX recovery_outbox_competition ON recovery_outbox(competition_id);
