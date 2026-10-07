-- Messages from the site's feedback form. App data, kept until the operator deletes it.
-- Times are UTC text of one fixed width (YYYY-MM-DDTHH:MM:SS.ffffffZ), so they sort as text.
CREATE TABLE feedback (
    id TEXT PRIMARY KEY NOT NULL,                       -- UUIDv7
    created_at TEXT NOT NULL,
    message TEXT NOT NULL,
    contact TEXT,                                       -- email or npub, when given
    page TEXT,                                          -- path the form was sent from
    rid TEXT,
    sid TEXT,
    pubkey TEXT,                                        -- first 16 hex of a signed-in key
    ip TEXT,
    user_agent TEXT,
    status TEXT NOT NULL DEFAULT 'new' CHECK (status IN ('new', 'seen', 'done')),
    operator_note TEXT,
    notified_at TEXT                                    -- when the alert went out
);
CREATE INDEX feedback_created_at ON feedback(created_at);
CREATE INDEX feedback_status ON feedback(status);
