-- A winner's split output the coordinator left on chain because the fee to sweep it would leave
-- less than the dust limit. Recorded per entry so settlement counts the output as handled and
-- the competition can finish, instead of retrying a sweep the network rejects as dust. The
-- output stays spendable through its win, sellback and reclaim paths.
ALTER TABLE entries ADD COLUMN sweep_uneconomic_at DATETIME;
