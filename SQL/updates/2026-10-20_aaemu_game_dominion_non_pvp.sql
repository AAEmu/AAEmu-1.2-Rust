-- Special effect 110 (skill 23992 "Dominion peace") stores the peace declaration start and
-- the authored duration (value1 = 120 minutes) on the territory authority row.
ALTER TABLE `dominions`
  ADD COLUMN `non_pvp_start_at` bigint NOT NULL DEFAULT 0,
  ADD COLUMN `non_pvp_duration_min` smallint unsigned NOT NULL DEFAULT 0;
