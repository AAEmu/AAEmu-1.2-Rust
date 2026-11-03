-- Player nations (1.2 "independence"): a top-level user faction created from an expedition.
--
-- The 1.2 client keeps one faction map and refuses a duplicate id (`FUN_398a5280`), so a
-- nation id comes from the same allocator as expedition faction ids (`ExpeditionIdManager`,
-- which also extracts `nations.id`). The nation record carries the founding expedition and
-- its owner; `expeditions.mother` and `characters.faction_id` are the authoritative
-- membership pointers (systems/social/nation.rs, `save`).
--
-- `nation_relations` is the per-nation relation overlay. A nation is created with a copy of
-- faction 166's `system_faction_relations` rows (NATION-2, `systems/social/nation.rs`); stage 3
-- stores relation changes / expiry here (`expire_at`, `next_state`).

CREATE TABLE IF NOT EXISTS `nations` (
  `id` int unsigned NOT NULL COMMENT 'Faction id; same id space as expeditions.id',
  `expedition_id` int unsigned NOT NULL COMMENT 'Founding expedition (its mother becomes this nation)',
  `owner_character_id` int unsigned NOT NULL COMMENT 'King: the founding expedition owner',
  `owner_name` varchar(128) NOT NULL,
  `name` varchar(128) NOT NULL,
  `created_at` bigint NOT NULL DEFAULT 0 COMMENT 'Unix seconds (SystemFaction.Created)',
  `allow_change_name` tinyint unsigned NOT NULL DEFAULT 0,
  PRIMARY KEY (`id`),
  UNIQUE KEY `nations_name` (`name`),
  KEY `nations_expedition` (`expedition_id`),
  KEY `nations_owner` (`owner_character_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Player nations (top-level user factions)';

CREATE TABLE IF NOT EXISTS `nation_relations` (
  `nation_id` int unsigned NOT NULL,
  `other_faction_id` int unsigned NOT NULL,
  `state` tinyint unsigned NOT NULL COMMENT '1 hostile, 2 neutral, 3 friendly',
  `expire_at` bigint NOT NULL DEFAULT 0 COMMENT 'Unix seconds; 0 = no expiry',
  `next_state` tinyint unsigned NOT NULL DEFAULT 0 COMMENT 'State to switch to at expire_at',
  PRIMARY KEY (`nation_id`,`other_faction_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Nation relation overlay';
