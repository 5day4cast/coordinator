CREATE TABLE list_updates (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);
CREATE INDEX list_updates_time ON list_updates(kind, updated_at);
CREATE INDEX entries_user_page ON entries(pubkey, id);
CREATE INDEX entries_event_page ON entries(pubkey, event_id, id);
INSERT INTO list_updates SELECT 'competition', id, COALESCE(completed_at, cancelled_at, failed_at, pools_finished_at, created_at) FROM competitions;
INSERT INTO list_updates SELECT 'entry', entries.id, COALESCE(entries.signed_at, competitions.created_at) FROM entries JOIN competitions ON competitions.id = entries.event_id;
CREATE TRIGGER competitions_list_insert AFTER INSERT ON competitions BEGIN
    INSERT INTO list_updates VALUES ('competition', NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
END;
CREATE TRIGGER competitions_list_update AFTER UPDATE ON competitions BEGIN
    INSERT INTO list_updates VALUES ('competition', NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
END;
CREATE TRIGGER entries_list_insert AFTER INSERT ON entries BEGIN
    INSERT INTO list_updates VALUES ('entry', NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
    INSERT INTO list_updates VALUES ('competition', NEW.event_id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
END;
CREATE TRIGGER entries_list_update AFTER UPDATE ON entries BEGIN
    INSERT INTO list_updates VALUES ('entry', NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
    INSERT INTO list_updates VALUES ('competition', NEW.event_id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
END;
CREATE TRIGGER payouts_list_insert AFTER INSERT ON payouts BEGIN
    INSERT INTO list_updates SELECT 'entry', NEW.entry_id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
    UPDATE list_updates SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE kind='competition' AND id=(SELECT event_id FROM entries WHERE id=NEW.entry_id);
END;
CREATE TRIGGER payouts_list_update AFTER UPDATE ON payouts BEGIN
    INSERT INTO list_updates SELECT 'entry', NEW.entry_id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
    UPDATE list_updates SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE kind='competition' AND id=(SELECT event_id FROM entries WHERE id=NEW.entry_id);
END;
CREATE TRIGGER tickets_list_insert AFTER INSERT ON tickets BEGIN
    INSERT INTO list_updates SELECT 'entry', id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now') FROM entries WHERE ticket_id=NEW.id ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
    UPDATE list_updates SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE kind='competition' AND id=NEW.event_id;
END;
CREATE TRIGGER tickets_list_update AFTER UPDATE ON tickets BEGIN
    INSERT INTO list_updates SELECT 'entry', id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now') FROM entries WHERE ticket_id=NEW.id ON CONFLICT(kind,id) DO UPDATE SET updated_at = excluded.updated_at;
    UPDATE list_updates SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE kind='competition' AND id=NEW.event_id;
END;
