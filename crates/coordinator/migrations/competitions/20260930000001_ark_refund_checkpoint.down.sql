-- SQLite cannot drop a column in older versions; the refund table is dropped with it.
ALTER TABLE ticket_ark_refunds DROP COLUMN checkpoint_psbt;
