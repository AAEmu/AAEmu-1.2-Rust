-- Account payment and premium state (WP-B). The game server now reads the payment state from
-- `accounts` instead of the permanent C# `AccountPayment` default, and the premium point drives
-- the grade (`premium_grades`, highest `grade_id` whose `point` <= the account point; 1.2 client
-- `FUN_397a7d10`) and the grade benefits (`premium_benefits`, `pcbang_buffs`).
--
-- `pay_method` 0 means "no explicit payment": such rows use `Account:DefaultPayment` (owner
-- decision 2026-11-03: `premium` by default, so existing databases keep today's behaviour).
-- `pay_start` / `pay_end` are unix seconds like `SCAccountInfo.payStart` / `payEnd` (the client
-- reads i64 at +0x18 / +0x20, serializer FUN_397aeb90 = `payMethod, payLocation, payStart,
-- payEnd, realPayTime`). `premium_point` is clamped to `premium_configs.max_point` and pushed
-- with `SCUpdatePremiumPoint(point, oldPg, pg)` (serializer FUN_397b8b00).

ALTER TABLE `accounts`
  ADD COLUMN `pay_method` TINYINT NOT NULL DEFAULT 0
    COMMENT '1 Premium, 3 Demo, 5 None; 0 = Account:DefaultPayment',
  ADD COLUMN `pay_location` INT NOT NULL DEFAULT 1,
  ADD COLUMN `pay_start` BIGINT NOT NULL DEFAULT 0 COMMENT 'Unix seconds',
  ADD COLUMN `pay_end` BIGINT NOT NULL DEFAULT 0 COMMENT 'Unix seconds',
  ADD COLUMN `premium_point` INT NOT NULL DEFAULT 0 COMMENT 'Premium point; grade = premium_grades lookup';
