-- An accepted siege declaration for one weekly cycle (WP-SD2a). The attacker expedition is
-- 0 while no declaration exists; `declaration_cycle` is the cycle's auction start, so a
-- declaration only describes that one cycle and a restart can discard a stale one. The camp
-- position becomes the offense HQ of the materialised siege. See
-- `systems::housing::dominion::{declare, state}`.
ALTER TABLE `dominions`
  ADD COLUMN `declaration_attacker` int unsigned NOT NULL DEFAULT 0 AFTER `non_pvp_duration_min`,
  ADD COLUMN `declaration_at` bigint NOT NULL DEFAULT 0 AFTER `declaration_attacker`,
  ADD COLUMN `declaration_cycle` bigint NOT NULL DEFAULT 0 AFTER `declaration_at`,
  ADD COLUMN `declaration_hq_x` float NOT NULL DEFAULT 0 AFTER `declaration_cycle`,
  ADD COLUMN `declaration_hq_y` float NOT NULL DEFAULT 0 AFTER `declaration_hq_x`,
  ADD COLUMN `declaration_hq_z` float NOT NULL DEFAULT 0 AFTER `declaration_hq_y`;
