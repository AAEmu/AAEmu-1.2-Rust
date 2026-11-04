-- The defender's reinforcement wave (start of the siege + `bdm`) has been announced
-- (`SCSiegeReinforce` 0xeb). Persisted so a restart cannot repeat the announcement; the
-- in-memory authority also raises the defender roster limit from `ni` to `nr`.
-- See `systems::housing::dominion::reinforce`.
ALTER TABLE `dominions`
  ADD COLUMN `siege_reinforced` tinyint(1) NOT NULL DEFAULT 0 AFTER `siege_ended_at`;
