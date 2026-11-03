-- Nation diplomacy: remember when the last type-25 treaty request was sent for a
-- (nation, diplomacy target) pair, so a deleted or declined mail cannot be re-sent every tick
-- and the cooldown survives a restart.
CREATE TABLE IF NOT EXISTS `nation_relation_offers` (
  `nation_id` int unsigned NOT NULL,
  `other_faction_id` int unsigned NOT NULL,
  `last_offer_at` bigint NOT NULL DEFAULT 0 COMMENT 'Unix seconds of the last type-25 mail',
  PRIMARY KEY (`nation_id`,`other_faction_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Nation treaty request cooldowns';
