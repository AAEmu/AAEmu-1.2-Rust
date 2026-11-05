-- Premium service shop (WP-C). `compact.sqlite3.premium_points` has no catalogue (0 rows), so
-- the list answered by `CSPremiumServiceList` (0x136) comes from this operator table instead of
-- the C# hard-coded 30-day entry.
--
-- One row becomes one `SCPremiumServiceList` (0x1d8) detail per packet. `size` is the expected
-- TOTAL row count, not the count in this packet: the client handler `FUN_391e25e0` appends the
-- detail only while `(end - begin) / 0x1b0 < size`, so every packet carries `size = rows` and
-- `isEnd` is set only on the last one. A `size = 0` packet marks the list ready (empty
-- catalogue); it does not clear a list. The catalogue is capped at 255 rows (the field is a
-- u8). `SCPremiumServiceList` field order (`FUN_397c49b0` -> `FUN_397bf920`): isEnd, size,
-- { cId, cname <= 400, pId, isSell, isHidden, ptime, ptype, price, id, bcount }, exchangeRatio.
--
-- `price_type` uses the cash shop currency enum (`ics_skus.currency`): 0 credits (AA cash),
-- 1 AA points, 2 loyalty, 3 coins. The buy path can atomically debit the account columns credits
-- and loyalty together with the premium grant; AA points have no balance carrier yet (WP-D), so
-- a `price_type = 1` service is refused with `PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT` (775) and any
-- other price type with `PREMIUM_SERVICE_BUY_FAIL` (773).
--
-- The catalogue is empty by default: content is operator policy and no 1.2 retail row is
-- evidenced. Example (not inserted), kept compatible with the removed C# stub id:
--   INSERT INTO `premium_services`
--     (`c_id`, `name`, `premium_id`, `hours`, `price_type`, `price`, `grade`, `is_sell`,
--      `is_hidden`, `buy_max`)
--   VALUES (8000001, 'Premium (30 days)', 1, 720, 0, 300, 0, 1, 0, 0);

CREATE TABLE IF NOT EXISTS `premium_services` (
    `c_id` INT NOT NULL COMMENT 'CSPremiumServiceBuy.cId and SCPremiumServiceList.cId',
    `name` VARCHAR(400) NOT NULL DEFAULT '' COMMENT 'SCPremiumServiceList.cname (client max 400 bytes)',
    `premium_id` SMALLINT UNSIGNED NOT NULL DEFAULT '0' COMMENT 'SCPremiumServiceList.pId',
    `hours` INT NOT NULL DEFAULT '0' COMMENT 'Premium hours granted by a purchase (SCPremiumServiceList.ptime)',
    `price_type` TINYINT UNSIGNED NOT NULL DEFAULT '0' COMMENT 'Credits(0), AAPoints(1), Loyalty(2), Coins(3)',
    `price` INT NOT NULL DEFAULT '0' COMMENT 'Price in price_type units',
    `grade` TINYINT UNSIGNED NOT NULL DEFAULT '0' COMMENT 'Copied to SCPremiumServiceList.id; meaning UNKNOWN, display only',
    `is_sell` TINYINT(1) NOT NULL DEFAULT '0' COMMENT 'Buy allowed (1 = for sale)',
    `is_hidden` TINYINT(1) NOT NULL DEFAULT '0' COMMENT 'SCPremiumServiceList.isHidden',
    `buy_max` INT NOT NULL DEFAULT '0' COMMENT 'Copied to SCPremiumServiceList.bcount; not enforced (meaning UNKNOWN)',
    PRIMARY KEY (`c_id`) USING BTREE
)
COMMENT='Premium service shop catalogue (WP-C)'
COLLATE='utf8mb4_general_ci'
ENGINE=InnoDB
;
