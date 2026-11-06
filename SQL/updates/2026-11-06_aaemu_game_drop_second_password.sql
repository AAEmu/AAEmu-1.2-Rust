-- The second password is not supported (owner decision 2026-11-06). Remove the per-account
-- state that WP-A added. Existing databases that applied
-- 2026-11-03_aaemu_game_second_password.sql still have the three columns; a fresh install of
-- the base schema does not. MySQL has no `DROP COLUMN IF EXISTS`, so run each ALTER only when
-- information_schema shows the column; the script is therefore safe to run again and harmless
-- on a fresh install.

SET @has_second_password_hash := (
  SELECT COUNT(*) FROM `information_schema`.`COLUMNS`
  WHERE `TABLE_SCHEMA` = DATABASE()
    AND `TABLE_NAME` = 'accounts'
    AND `COLUMN_NAME` = 'second_password_hash'
);
SET @sql := IF(@has_second_password_hash > 0,
  'ALTER TABLE `accounts` DROP COLUMN `second_password_hash`',
  'SELECT 1');
PREPARE drop_second_password_hash_stmt FROM @sql;
EXECUTE drop_second_password_hash_stmt;
DEALLOCATE PREPARE drop_second_password_hash_stmt;

SET @has_second_password_failed := (
  SELECT COUNT(*) FROM `information_schema`.`COLUMNS`
  WHERE `TABLE_SCHEMA` = DATABASE()
    AND `TABLE_NAME` = 'accounts'
    AND `COLUMN_NAME` = 'second_password_failed'
);
SET @sql := IF(@has_second_password_failed > 0,
  'ALTER TABLE `accounts` DROP COLUMN `second_password_failed`',
  'SELECT 1');
PREPARE drop_second_password_failed_stmt FROM @sql;
EXECUTE drop_second_password_failed_stmt;
DEALLOCATE PREPARE drop_second_password_failed_stmt;

SET @has_second_password_locked_until := (
  SELECT COUNT(*) FROM `information_schema`.`COLUMNS`
  WHERE `TABLE_SCHEMA` = DATABASE()
    AND `TABLE_NAME` = 'accounts'
    AND `COLUMN_NAME` = 'second_password_locked_until'
);
SET @sql := IF(@has_second_password_locked_until > 0,
  'ALTER TABLE `accounts` DROP COLUMN `second_password_locked_until`',
  'SELECT 1');
PREPARE drop_second_password_locked_until_stmt FROM @sql;
EXECUTE drop_second_password_locked_until_stmt;
DEALLOCATE PREPARE drop_second_password_locked_until_stmt;
