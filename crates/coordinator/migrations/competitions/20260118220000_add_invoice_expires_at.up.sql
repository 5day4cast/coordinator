-- Add invoice_expires_at to track when the HODL invoice expires
-- This allows us to check if we can reuse an existing invoice or need to create a new one
ALTER TABLE tickets ADD COLUMN invoice_expires_at DATETIME;
