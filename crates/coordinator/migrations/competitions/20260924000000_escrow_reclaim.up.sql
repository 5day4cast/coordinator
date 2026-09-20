-- Escrow outputs of dead competitions are swept back through the descriptor's
-- coordinator reclaim branch; recorded per ticket so a sweep runs once.
ALTER TABLE tickets ADD COLUMN escrow_reclaimed_at DATETIME;
