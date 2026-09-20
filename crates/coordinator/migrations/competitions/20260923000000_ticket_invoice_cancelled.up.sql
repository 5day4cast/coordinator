-- When a competition dies, every accepted-but-unsettled HODL invoice is
-- cancelled so the payer's funds are released. Recorded per ticket so the
-- release is retried until LND confirms it and never repeated after.
ALTER TABLE tickets ADD COLUMN invoice_cancelled_at DATETIME;
