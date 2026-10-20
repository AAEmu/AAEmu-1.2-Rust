-- Shipyards (drydocks) were runtime-only: a restart lost every drydock with its build progress.
-- `systems::vehicles::shipyards_db` writes them with the regular save, deletes the row when the
-- shipyard is removed (finished ship, death / decay) and restores them at server start.
-- `id` is the ShipyardIdManager id, `owner_id` the character id of the owner, `spawned` Unix
-- seconds UTC (drives the 3 day tax protection / decay).

CREATE TABLE IF NOT EXISTS `shipyards` (
  `id`           int unsigned NOT NULL COMMENT 'ShipyardIdManager id',
  `template_id`  int unsigned NOT NULL COMMENT 'Shipyard template',
  `owner_id`     int unsigned NOT NULL COMMENT 'characters.id of the owner',
  `owner_name`   varchar(128) NOT NULL,
  `faction`      int unsigned NOT NULL DEFAULT 0,
  `instance_id`  int unsigned NOT NULL COMMENT 'World instance',
  `zone_id`      int unsigned NOT NULL DEFAULT 0,
  `x`            float NOT NULL,
  `y`            float NOT NULL,
  `z`            float NOT NULL,
  `rot_x`        float NOT NULL DEFAULT 0,
  `rot_y`        float NOT NULL DEFAULT 0,
  `z_rot`        float NOT NULL DEFAULT 0,
  `money_amount` int NOT NULL DEFAULT 0,
  `actions`      int NOT NULL DEFAULT 0 COMMENT 'ShipyardData.Actions',
  `step`         int NOT NULL DEFAULT 0 COMMENT 'ShipyardData.Step',
  `current_step` int NOT NULL DEFAULT 0 COMMENT 'Shipyard.CurrentStep (-1 = all steps built)',
  `num_action`   int NOT NULL DEFAULT 0 COMMENT 'Build actions done in the current step',
  `hp`           int NOT NULL DEFAULT 0 COMMENT 'Unit hp',
  `data_hp`      int NOT NULL DEFAULT 0 COMMENT 'ShipyardData.Hp',
  `spawned`      bigint NOT NULL COMMENT 'Unix seconds UTC',
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Shipyards (drydocks), restored at server start';
