-- Two ark-swapd instances can share this database during a blue/green deploy. Only the
-- lease holder runs the swap loop and moves the wallet's coins; both serve the API.
CREATE TABLE worker_lease (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    holder TEXT NOT NULL,
    -- UNIX milliseconds.
    expires_at INTEGER NOT NULL
);
