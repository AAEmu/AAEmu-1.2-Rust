-- Roster limits frozen when an explicit siege is scheduled (WP-S4 follow-up): the defender's
-- cap before the reinforcement wave (`ni`) and after it (`nr`); the attacker uses `nr`.
-- Freezing keeps a mid-siege change in the defender's castle count (or a lost ephemeral GM
-- dominion) from moving the caps under a running siege. The default 100 keeps sieges that
-- were already persisted before this migration on the old native array capacity.
ALTER TABLE `dominions`
  ADD COLUMN `siege_defender_ni` tinyint unsigned NOT NULL DEFAULT 100 AFTER `siege_reinforced`,
  ADD COLUMN `siege_defender_nr` tinyint unsigned NOT NULL DEFAULT 100 AFTER `siege_defender_ni`;
