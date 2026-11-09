-- Automatic posting of each cycle's Siege Camp lots (WP-SD1c). `auction_posted_cycle` is the
-- `Cycle::auction_start` of the last cycle whose declaration-auction lot this territory posted,
-- so a restart inside the auction period cannot post the same lot twice. 0 = none yet.
-- See `systems::housing::dominion::auction_post` and `state`.
ALTER TABLE `dominions`
  ADD COLUMN `auction_posted_cycle` bigint NOT NULL DEFAULT 0 AFTER `declaration_hq_z`;
