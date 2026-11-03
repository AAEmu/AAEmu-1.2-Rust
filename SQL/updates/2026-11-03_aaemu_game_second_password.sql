-- 1.2 second password (WP-A): per-account PBKDF2 hash through the shared password service, the
-- consecutive failure counter and the timed lock (`SecondPassword:MaxFailCount` /
-- `SecondPassword:LockMinutes`). The lock time is unix seconds like `SCSecondPass*` and
-- `SCAccountInfo.pay_end`; 0 = not locked. The hash is written/read by `systems::services::
-- second_password` under `AccountManager::lock_of`.

ALTER TABLE `accounts`
  ADD COLUMN `second_password_hash` TEXT NULL DEFAULT NULL
    COMMENT 'PBKDF2 (ASP.NET Identity V3) hash of the 1.2 second password; NULL = not set',
  ADD COLUMN `second_password_failed` TINYINT UNSIGNED NOT NULL DEFAULT 0
    COMMENT 'Consecutive failed second-password checks',
  ADD COLUMN `second_password_locked_until` BIGINT NOT NULL DEFAULT 0
    COMMENT 'Unix seconds until second-password checks are refused; 0 = not locked';
