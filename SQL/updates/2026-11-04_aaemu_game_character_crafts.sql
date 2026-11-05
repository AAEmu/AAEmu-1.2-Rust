-- Crafts learned from recipe items (`item_recipes`) and plain training skills
-- (`train_craft_effects`), restored on login and saved with the character.
--
-- A recipe item uses the generic skill 11144 / 10003 whose effect is `TrainCraftEffect`; the
-- craft to learn comes from `item_recipes` for the source item template. `craft_id` is
-- `crafts.id` from compact.sqlite3. No foreign key to `characters`: `Character.Save` uses
-- `REPLACE INTO characters`, and a cascading child would lose `learned_at` on every save.

CREATE TABLE IF NOT EXISTS `character_crafts` (
  `character_id` INT UNSIGNED NOT NULL COMMENT 'Character who learned the craft',
  `craft_id`     INT UNSIGNED NOT NULL COMMENT 'crafts.id from compact.sqlite3',
  `learned_at`   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT 'UTC timestamp of the first learn',
  PRIMARY KEY (`character_id`, `craft_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Crafts learned from recipe items / TrainCraftEffect';
