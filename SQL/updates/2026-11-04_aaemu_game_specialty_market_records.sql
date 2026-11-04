-- Specialty (trade pack) sale-ratio history for `SCSpecialtyRecordsPacket` (0x09b).
-- `systems::economy::specialty` appends one row per sold pack (the ratio actually paid);
-- history is loaded at startup and written with the save transaction.
CREATE TABLE IF NOT EXISTS `specialty_market_records` (
  `id`            bigint unsigned NOT NULL AUTO_INCREMENT,
  `zone_group_id` int unsigned NOT NULL COMMENT 'Zone group where the pack was sold',
  `item_id`       int unsigned NOT NULL COMMENT 'Trade pack item template id',
  `ratio`         int NOT NULL COMMENT 'Sale ratio shown to the client (percent)',
  `recorded`      bigint NOT NULL COMMENT 'Unix seconds UTC',
  PRIMARY KEY (`id`),
  KEY `zone_item_recorded` (`zone_group_id`, `item_id`, `recorded`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Specialty sale ratio history (SCSpecialtyRecordsPacket)';
