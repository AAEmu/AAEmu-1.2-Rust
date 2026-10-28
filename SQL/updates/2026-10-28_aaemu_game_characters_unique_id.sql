-- MySQL 8.4 defaults to `restrict_fk_on_non_standard_key=ON` and rejects a foreign key whose
-- referenced column is not a unique key of the referenced table, even when it is the left-most
-- column of a composite primary key. `characters` is keyed by `(id, account_id)`, so
-- `fk_character_records_owner` and `fk_character_achievements_owner`
-- (2026-10-13_aaemu_game_achievement_progress.sql) fail to install there with
-- "ERROR 6125: Missing unique key for constraint ... in the referenced table 'characters'".
--
-- Character ids come from `CharacterIdManager` (`first_id` 1, `last_id` 0xFFFFFF, read back from
-- `characters.id` and `slaves.id` at startup), so `id` is unique by construction; the base schema
-- now declares `uq_characters_id` too.
--
-- MySQL has no `ADD UNIQUE KEY IF NOT EXISTS`, so run the ALTER only when no unique index covers
-- exactly `id` (the primary key's `(id, account_id)` does not count: MySQL 8.4 rejects a prefix of
-- a composite unique key too). This keeps the script runnable a second time and harmless after a
-- fresh install, whose base schema already has `uq_characters_id`.

SET @has_unique_characters_id := (
  SELECT COUNT(*) FROM (
    SELECT `INDEX_NAME` FROM `information_schema`.`STATISTICS`
    WHERE `TABLE_SCHEMA` = DATABASE()
      AND `TABLE_NAME` = 'characters'
      AND `NON_UNIQUE` = 0
    GROUP BY `INDEX_NAME`
    HAVING COUNT(*) = 1 AND MAX(`COLUMN_NAME` = 'id') = 1
  ) AS `unique_characters_id`
);
SET @sql := IF(@has_unique_characters_id = 0,
  'ALTER TABLE `characters` ADD UNIQUE KEY `uq_characters_id` (`id`)',
  'SELECT 1');
PREPARE uq_characters_id_stmt FROM @sql;
EXECUTE uq_characters_id_stmt;
DEALLOCATE PREPARE uq_characters_id_stmt;
