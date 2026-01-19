-- Add invoices_settled_at column to track when hold invoices were settled
ALTER TABLE competitions ADD COLUMN invoices_settled_at TEXT;
