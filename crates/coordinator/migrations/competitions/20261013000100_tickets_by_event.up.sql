-- A queued competition's entry cap and the seats its pages show count its live tickets.
CREATE INDEX IF NOT EXISTS tickets_by_event ON tickets(event_id);
