-- `items.limited_sale_count` / `items.one_time_sale` limits are per ACCOUNT, not per character:
-- the 1.2 item descriptions of the scarecrow farms say "Can be purchased only once per account
-- and can't be traded." (items 30013 / 30014 / 30560; `localized_texts` rows for
-- `items.description`). The counters were keyed by `characters.id`; this moves them to
-- `characters.account_id`.
--
-- Existing rows are folded with MAX per (account, item), not SUM: each per-character row was that
-- character's own count, so taking the largest keeps every character at or below what it had
-- already bought (nobody is retroactively below a limit they had reached) and never invents
-- purchases. SUM would also be defensible, but it inflates accounts that simply bought on two
-- characters and does not change what is refused. Rows whose character has no `characters` row
-- any more (hard-deleted) are dropped: without the character there is no account to attribute
-- them to. A soft-deleted character (`characters.deleted` = 1) still resolves through the join,
-- so its count is kept and counts toward its account.
--
-- BACK UP the game database before this runs: the last statement drops `character_shop_purchases`
-- for good, and nothing keeps the per-character counts afterwards.

CREATE TABLE IF NOT EXISTS `account_shop_purchases` (
  `account_id`     int unsigned NOT NULL COMMENT 'Login account id (characters.account_id)',
  `item_id`        int unsigned NOT NULL COMMENT 'items.id (the template, not an item instance)',
  `purchase_count` int NOT NULL DEFAULT 0 COMMENT 'Copies bought so far',
  PRIMARY KEY (`account_id`, `item_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Per-account purchases of limited-sale item templates';

-- The old table is missing on a database that never had it (for example one created from the
-- current base schema), and the INSERT below would fail on that. Create it empty (the DDL it was
-- created with) so the script runs on any database. The DROP at the end removes it again.
CREATE TABLE IF NOT EXISTS `character_shop_purchases` (
  `character_id`   int unsigned NOT NULL COMMENT 'characters.id',
  `item_id`        int unsigned NOT NULL COMMENT 'items.id (the template, not an item instance)',
  `purchase_count` int NOT NULL DEFAULT 0 COMMENT 'Copies bought so far',
  PRIMARY KEY (`character_id`, `item_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Per-character purchases of limited-sale item templates';

INSERT INTO `account_shop_purchases` (`account_id`, `item_id`, `purchase_count`)
SELECT c.`account_id`, p.`item_id`, MAX(p.`purchase_count`)
FROM `character_shop_purchases` p
JOIN `characters` c ON c.`id` = p.`character_id`
GROUP BY c.`account_id`, p.`item_id`
-- Re-runnable: the updater has no transaction, so a failure after this INSERT re-runs it.
-- The column is qualified: the SELECT's `p`.`purchase_count` has the same name, so the bare name
-- is ambiguous in an INSERT ... SELECT.
ON DUPLICATE KEY UPDATE `purchase_count` = GREATEST(`account_shop_purchases`.`purchase_count`, VALUES(`purchase_count`));

-- Irreversible: back up first (see the top of this file).
DROP TABLE IF EXISTS `character_shop_purchases`;
