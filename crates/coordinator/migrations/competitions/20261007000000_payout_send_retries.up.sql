-- A payment that failed for a reason that can pass, such as no route, is sent again with the
-- same invoice. `next_send_at` is when the invoice may next be sent, as Unix seconds; it is
-- NULL while a send is awaited. `send_attempts` counts the sends that failed that way.
ALTER TABLE payouts ADD COLUMN next_send_at INTEGER;
ALTER TABLE payouts ADD COLUMN send_attempts INTEGER NOT NULL DEFAULT 0;
