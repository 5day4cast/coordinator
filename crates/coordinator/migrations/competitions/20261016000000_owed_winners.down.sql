ALTER TABLE entries DROP COLUMN split_output_spent_at;
DROP INDEX IF EXISTS owed_winners_by_competition;
DROP TABLE IF EXISTS owed_winners;
