-- AA points are not supported (owner decision 2026-11-06). Remove the per-character wallet that
-- WP-AA1 added. Existing databases that applied 2026-11-06_aaemu_game_aa_point.sql still have
-- the two columns; a fresh install of the base schema does not. MySQL has no
-- `DROP COLUMN IF EXISTS`, so run each ALTER only when information_schema shows the column;
-- the script is therefore safe to run again and harmless on a fresh install.

SET @has_aa_point := (
  SELECT COUNT(*) FROM `information_schema`.`COLUMNS`
  WHERE `TABLE_SCHEMA` = DATABASE()
    AND `TABLE_NAME` = 'characters'
    AND `COLUMN_NAME` = 'aa_point'
);
SET @sql := IF(@has_aa_point > 0,
  'ALTER TABLE `characters` DROP COLUMN `aa_point`',
  'SELECT 1');
PREPARE drop_aa_point_stmt FROM @sql;
EXECUTE drop_aa_point_stmt;
DEALLOCATE PREPARE drop_aa_point_stmt;

SET @has_bank_aa_point := (
  SELECT COUNT(*) FROM `information_schema`.`COLUMNS`
  WHERE `TABLE_SCHEMA` = DATABASE()
    AND `TABLE_NAME` = 'characters'
    AND `COLUMN_NAME` = 'bank_aa_point'
);
SET @sql := IF(@has_bank_aa_point > 0,
  'ALTER TABLE `characters` DROP COLUMN `bank_aa_point`',
  'SELECT 1');
PREPARE drop_bank_aa_point_stmt FROM @sql;
EXECUTE drop_bank_aa_point_stmt;
DEALLOCATE PREPARE drop_bank_aa_point_stmt;
