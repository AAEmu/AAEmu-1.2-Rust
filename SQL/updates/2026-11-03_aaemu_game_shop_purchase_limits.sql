-- `items.limited_sale_count` / `items.one_time_sale` are per-character purchase limits. The
-- 1.2 client loads both columns but never reads them (no remaining-count field in the NPC store
-- UI), and the only sale-limit error in its table is `store_one_time_sale` (752); the limit is
-- enforced server-side. The counters are written with the wallet / items in the save transaction
-- and loaded at startup. Selling an item back does not change a counter, and non-purchase item
-- grants (GM give, quests, loot) are not limited.

CREATE TABLE IF NOT EXISTS `character_shop_purchases` (
  `character_id`   int unsigned NOT NULL COMMENT 'characters.id',
  `item_id`        int unsigned NOT NULL COMMENT 'items.id (the template, not an item instance)',
  `purchase_count` int NOT NULL DEFAULT 0 COMMENT 'Copies bought so far',
  PRIMARY KEY (`character_id`, `item_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Per-character purchases of limited-sale item templates';
