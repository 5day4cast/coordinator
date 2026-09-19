-- A payout that has not failed is live: pending or succeeded. Only one may
-- exist per entry, so concurrent sellback requests cannot both be paid.
CREATE UNIQUE INDEX IF NOT EXISTS payouts_one_live_per_entry
    ON payouts (entry_id)
    WHERE failed_at IS NULL;
