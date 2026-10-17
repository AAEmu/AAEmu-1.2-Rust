-- Growth state of world-spawned ("natural") doodads (`systems::npcs::doodad::natural`): trees, ore,
-- crops and other doodads whose template waits at least 5 minutes in some phase. Without it every
-- server start recreated them in their Start phase, so resources that need more than the uptime
-- (up to ~100 h) never matured.
--
-- `spawn_key` identifies the spawn entry (`<authored template id>:<x*10>:<y*10>:<z*10>`), not the
-- doodad: `template_id` is the template at save time (a ratio respawn can replace it). The row is
-- deleted when the doodad is harvested or despawned. `plant_time` is UTC Unix seconds;
-- `growth_time` / `phase_time` are UTC Unix milliseconds.

CREATE TABLE IF NOT EXISTS `doodad_spawn_states` (
  `world_id`           int unsigned NOT NULL COMMENT 'World instance id of the spawn entry',
  `spawn_key`          varchar(64) NOT NULL COMMENT 'Authored template id and position (0.1 m) of the spawn entry',
  `template_id`        int unsigned NOT NULL COMMENT 'Doodad template at save time',
  `func_group_id`      int unsigned NOT NULL COMMENT 'Current phase',
  `plant_time`         bigint NOT NULL COMMENT 'Unix seconds UTC',
  `growth_time`        bigint NOT NULL DEFAULT 0 COMMENT 'Unix milliseconds UTC',
  `phase_time`         bigint NOT NULL DEFAULT 0 COMMENT 'Unix milliseconds UTC, start of the current phase',
  `data`               int NOT NULL DEFAULT 0,
  `interaction_counts` text NULL COMMENT 'JSON object: completed interactions of the current phase',
  `updated`            bigint NOT NULL DEFAULT 0 COMMENT 'Unix seconds UTC of the snapshot',
  PRIMARY KEY (`world_id`, `spawn_key`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Growth state of world-spawned doodads, restored at server start';
