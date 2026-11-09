-- EU 1.2 Loyalty: account play-time counters and an idempotent daily login reward.
--
-- `loyalty_hour_day` is the UTC day number (unix seconds / 86400) the hourly counters belong to;
-- `loyalty_hour_seconds` accumulates in-world seconds in that day, `loyalty_hour_taken` counts the
-- hourly tokens already granted that day and `loyalty_hour_updated` is the last online sample, so a
-- crash or relog neither loses nor repeats play time.
--
-- `last_daily_reward` is the UTC midnight of the last daily login reward (credits + loyalty); the
-- grant claims it atomically so the login path and the midnight quest reset cannot both pay on the
-- same day. See `systems::services::timed_rewards`.
ALTER TABLE `accounts`
  ADD COLUMN `last_daily_reward` DATETIME NULL DEFAULT NULL COMMENT 'UTC midnight of the last daily login reward (credits/loyalty)' AFTER `last_loyalty_tick`,
  ADD COLUMN `loyalty_hour_day` INT NOT NULL DEFAULT 0 COMMENT 'UTC day number (unix days) of the hourly Loyalty counters' AFTER `last_daily_reward`,
  ADD COLUMN `loyalty_hour_seconds` INT NOT NULL DEFAULT 0 COMMENT 'In-world seconds counted today toward the next hourly Loyalty token' AFTER `loyalty_hour_day`,
  ADD COLUMN `loyalty_hour_taken` INT NOT NULL DEFAULT 0 COMMENT 'Hourly Loyalty tokens granted today' AFTER `loyalty_hour_seconds`,
  ADD COLUMN `loyalty_hour_updated` DATETIME NULL DEFAULT NULL COMMENT 'Last online sample (UTC) of the hourly Loyalty counters' AFTER `loyalty_hour_taken`;
