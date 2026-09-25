-- The Keymeld registration a player's browser sends before it shows the ticket's invoice, so a
-- ticket paid for but never used for an entry can still be refunded. It belongs to one
-- reservation of the ticket, named by the ticket's hash, and is deleted when that reservation is
-- released, and once the competition has no more use for it.
CREATE TABLE ticket_keymeld_registrations (
    ticket_id TEXT PRIMARY KEY NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    ticket_hash TEXT NOT NULL,
    registration_json TEXT NOT NULL
);
