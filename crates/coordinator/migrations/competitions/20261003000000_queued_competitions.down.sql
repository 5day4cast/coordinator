DROP TABLE competition_pools;
DROP TABLE queued_competitions;
DROP INDEX competitions_by_parent;
ALTER TABLE competitions DROP COLUMN pools_formed_at;
ALTER TABLE competitions DROP COLUMN pool_index;
ALTER TABLE competitions DROP COLUMN parent_id;
ALTER TABLE competitions DROP COLUMN kind;
