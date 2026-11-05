-- The kinds 57-60 siege-result achievements of one siege id have been credited (WP-SD6a).
-- Persisted like `siege_reinforced`, so a repeated tick, a capture racing the tick or a restart
-- cannot credit the same siege twice; a cancelled outcome credits nobody. See
-- `systems::housing::dominion::credit`.
ALTER TABLE `dominions`
  ADD COLUMN `siege_outcome_credited` tinyint(1) NOT NULL DEFAULT 0 AFTER `siege_defender_nr`;
