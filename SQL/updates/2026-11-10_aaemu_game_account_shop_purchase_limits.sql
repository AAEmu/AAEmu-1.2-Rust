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
-- characters and does not change what is refused. Character rows that no longer resolve to a
-- `characters` row are dropped: without the character there is no account to attribute them to.

CREATE TABLE IF NOT EXISTS `account_shop_purchases` (
  `account_id`     int unsigned NOT NULL COMMENT 'Login account id (characters.account_id)',
  `item_id`        int unsigned NOT NULL COMMENT 'items.id (the template, not an item instance)',
  `purchase_count` int NOT NULL DEFAULT 0 COMMENT 'Copies bought so far',
  PRIMARY KEY (`account_id`, `item_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Per-account purchases of limited-sale item templates';

INSERT INTO `account_shop_purchases` (`account_id`, `item_id`, `purchase_count`)
SELECT c.`account_id`, p.`item_id`, MAX(p.`purchase_count`)
FROM `character_shop_purchases` p
JOIN `characters` c ON c.`id` = p.`character_id`
GROUP BY c.`account_id`, p.`item_id`;

DROP TABLE IF EXISTS `character_shop_purchases`;
