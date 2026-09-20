CREATE TABLE keymeld_protocol_state (
    session_id TEXT PRIMARY KEY NOT NULL,
    version INTEGER NOT NULL CHECK(version >= 0),
    encrypted_state TEXT NOT NULL
);
