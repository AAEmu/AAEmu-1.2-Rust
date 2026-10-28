-- Tuning: every kill of a coinpurse NPC drops its coinpurse, with the purse contents scaled
-- down so the expected loot per kill stays the same.
--
-- 2568 NPCs drop exactly one of the five coinpurse packs (loot_pack_dropping_npcs):
--   8055 Farmer's (29203), 8056 Priest's (29204), 8057 Merchant's (29205),
--   8058 Noble's (29206), 8059 Jester's (29207).
-- Each pack's only group rolled at 20% (loot_groups.drop_rate 2000000 of 10000000); it is
-- raised to 100%. Five times as many purses are opened, so each opening gives a fifth:
--   * Coin (item 500) amounts of the use packs 11130-11134 are divided by 5 (same average).
--   * The bonus groups 5-9 of 11131-11134 (archeum, lunadrops, points, lunarite, prism /
--     garden powder) roll at a fifth of their rate.
-- Groups 2-4 of 11130-11134 (the jackpot tiers) carry rates but no loot rows in the 1.2
-- data, so they never pay out and are left alone.
-- Purses found in other containers (packs 8231 and 8340) use the same use packs and so
-- also give the reduced amounts.
--
-- Every statement matches the old value, so applying the patch twice changes nothing.

-- Drop the coinpurse on every kill (20% -> 100%).
UPDATE `loot_groups` SET `drop_rate` = 10000000
WHERE `pack_id` IN (8055, 8056, 8057, 8058, 8059) AND `group_no` = 1 AND `drop_rate` = 2000000;

-- Coins per opening divided by 5.
UPDATE `loots` SET `min_amount` = 30, `max_amount` = 50
WHERE `id` = 76320 AND `loot_pack_id` = 11130 AND `item_id` = 500 AND `min_amount` = 150 AND `max_amount` = 250;
UPDATE `loots` SET `min_amount` = 62, `max_amount` = 82
WHERE `id` = 76324 AND `loot_pack_id` = 11131 AND `item_id` = 500 AND `min_amount` = 310 AND `max_amount` = 410;
UPDATE `loots` SET `min_amount` = 74, `max_amount` = 94
WHERE `id` = 76331 AND `loot_pack_id` = 11132 AND `item_id` = 500 AND `min_amount` = 370 AND `max_amount` = 470;
UPDATE `loots` SET `min_amount` = 86, `max_amount` = 106
WHERE `id` = 76346 AND `loot_pack_id` = 11133 AND `item_id` = 500 AND `min_amount` = 430 AND `max_amount` = 530;
UPDATE `loots` SET `min_amount` = 110, `max_amount` = 130
WHERE `id` = 76364 AND `loot_pack_id` = 11134 AND `item_id` = 500 AND `min_amount` = 550 AND `max_amount` = 650;

-- Bonus item groups at a fifth of their rate.
UPDATE `loot_groups` SET `drop_rate` = 40741  WHERE `id` = 1092 AND `pack_id` = 11131 AND `group_no` = 5 AND `drop_rate` = 203704;
UPDATE `loot_groups` SET `drop_rate` = 122222 WHERE `id` = 1097 AND `pack_id` = 11132 AND `group_no` = 5 AND `drop_rate` = 611111;
UPDATE `loot_groups` SET `drop_rate` = 30000  WHERE `id` = 1098 AND `pack_id` = 11132 AND `group_no` = 6 AND `drop_rate` = 150000;
UPDATE `loot_groups` SET `drop_rate` = 3667   WHERE `id` = 1099 AND `pack_id` = 11132 AND `group_no` = 7 AND `drop_rate` = 18333;
UPDATE `loot_groups` SET `drop_rate` = 32593  WHERE `id` = 1104 AND `pack_id` = 11133 AND `group_no` = 5 AND `drop_rate` = 162963;
UPDATE `loot_groups` SET `drop_rate` = 40000  WHERE `id` = 1105 AND `pack_id` = 11133 AND `group_no` = 6 AND `drop_rate` = 200000;
UPDATE `loot_groups` SET `drop_rate` = 4889   WHERE `id` = 1106 AND `pack_id` = 11133 AND `group_no` = 7 AND `drop_rate` = 24444;
UPDATE `loot_groups` SET `drop_rate` = 80000  WHERE `id` = 1107 AND `pack_id` = 11133 AND `group_no` = 8 AND `drop_rate` = 400000;
UPDATE `loot_groups` SET `drop_rate` = 8000   WHERE `id` = 1108 AND `pack_id` = 11133 AND `group_no` = 9 AND `drop_rate` = 40000;
UPDATE `loot_groups` SET `drop_rate` = 8148   WHERE `id` = 1113 AND `pack_id` = 11134 AND `group_no` = 5 AND `drop_rate` = 40741;
UPDATE `loot_groups` SET `drop_rate` = 33333  WHERE `id` = 1114 AND `pack_id` = 11134 AND `group_no` = 6 AND `drop_rate` = 166667;
UPDATE `loot_groups` SET `drop_rate` = 6111   WHERE `id` = 1115 AND `pack_id` = 11134 AND `group_no` = 7 AND `drop_rate` = 30556;
UPDATE `loot_groups` SET `drop_rate` = 55000  WHERE `id` = 1116 AND `pack_id` = 11134 AND `group_no` = 8 AND `drop_rate` = 275000;
UPDATE `loot_groups` SET `drop_rate` = 10000  WHERE `id` = 1117 AND `pack_id` = 11134 AND `group_no` = 9 AND `drop_rate` = 50000;
