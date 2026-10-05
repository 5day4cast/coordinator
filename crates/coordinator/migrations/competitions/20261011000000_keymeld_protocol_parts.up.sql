-- A session's protocol state kept in parts, so a checkpoint writes what a step changed instead
-- of the whole state. `format` says how a row of keymeld_protocol_state is kept:
--   1  the whole state sealed in `encrypted_state`, as every row written before this;
--   2  `encrypted_state` is a sealed list of the state's parts, which are rows below.
-- New writes use format 1 until operators enable format 2.
ALTER TABLE keymeld_protocol_state ADD COLUMN format INTEGER NOT NULL DEFAULT 1;

-- One part of a session's state: `digest` is the session-bound HMAC-SHA256 of its text, `body` the text
-- compressed and sealed. A part is written once; a changed part is a new row.
CREATE TABLE keymeld_protocol_parts (
    session_id TEXT NOT NULL REFERENCES keymeld_protocol_state(session_id) ON DELETE CASCADE,
    digest BLOB NOT NULL,
    body BLOB NOT NULL,
    PRIMARY KEY (session_id, digest)
);
