-- The nation monument of a dominion: the persistent territory-statue doodad
-- (`siege_zones.monument_doodad_id`, 7229..7234 for the six 1.2 castle zone groups) and its
-- position. `SCNationalMonumentChanged` (0x022) carries the same fields; the 1.2 client
-- handler `FUN_392f35f0` stores them in the dominion record. See
-- `systems::housing::dominion::state::DominionState` and `systems::social::nation`
-- (`place_national_monument`).
ALTER TABLE `dominions`
  ADD COLUMN `national_monument_db_id` bigint NOT NULL DEFAULT 0,
  ADD COLUMN `national_monument_x` float NOT NULL DEFAULT 0,
  ADD COLUMN `national_monument_y` float NOT NULL DEFAULT 0,
  ADD COLUMN `national_monument_z` float NOT NULL DEFAULT 0;
