-- Skill and shared-tag cooldowns were runtime-only (`UnitCooldowns` in memory): a relog
-- cleared them, so a character could recast Recall / Escape / a long cooldown consumable at
-- once. `Character.Save` now rewrites this table and `Character.Load` restores the unexpired
-- rows before the select sync sends them to the 1.2 client (`SCCooldownsPacket`, native
-- `OnCooldowns` 391d43c0 -> 39899d40).
--
-- `cooldown_type` 0 is an individual skill id (`skills.id`), 1 is a shared cooldown group
-- (`skills.cooldown_tag_id`). `expires_at_ms` is an absolute Unix millisecond timestamp, so
-- the remaining time keeps running while the character is offline.

CREATE TABLE IF NOT EXISTS `character_cooldowns` (
  `character_id`  INT UNSIGNED NOT NULL COMMENT 'Character who owns this cooldown',
  `cooldown_type` TINYINT UNSIGNED NOT NULL COMMENT '0 = skill id, 1 = shared cooldown tag',
  `cooldown_id`   INT UNSIGNED NOT NULL COMMENT 'skills.id or skills.cooldown_tag_id',
  `expires_at_ms` BIGINT NOT NULL COMMENT 'Unix milliseconds UTC when the cooldown expires',
  PRIMARY KEY (`character_id`, `cooldown_type`, `cooldown_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Running skill / tag cooldowns, restored on login';
