-- `AccountAttributeEffect` (kind 1 = auction posting authority, `SpecialEffect.AuctionPostAuthority`
-- special 117). The data rows are account-wide grants (item 8000027 `경매장 등록권` adds it, the
-- removal tickets delete it) with an optional expiry and optional world binding.
--
-- `world_id` 0 = not world bound (`bind_world` false). `end_time` NULL = permanent. `count` is the
-- effect's count (0 for the 1.2 rows).

CREATE TABLE IF NOT EXISTS `account_attributes` (
  `account_id` INT UNSIGNED NOT NULL,
  `kind_id` INT NOT NULL,
  `world_id` INT NOT NULL DEFAULT 0,
  `count` INT NOT NULL DEFAULT 0,
  `end_time` DATETIME NULL DEFAULT NULL COMMENT 'UTC; NULL = permanent',
  PRIMARY KEY (`account_id`, `kind_id`, `world_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Account-wide attributes (auction post authority, ...)';
