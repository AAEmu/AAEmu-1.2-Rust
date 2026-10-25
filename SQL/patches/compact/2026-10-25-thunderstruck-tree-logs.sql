-- Fix: using a Thunderstruck Tree (item 15709) gives gliders instead of Thunderstruck Logs.
-- The item's use skill 8000033 ("Chop Thunderstruck Tree": "Spend 5 Labor to chop a
-- Thunderstruck Tree and receive 4 Thunderstruck Logs") runs effect 8000059 ->
-- GainLootPackItemEffect 8000013 -> loot pack 8000012. That pack instead held four
-- glider/wing rows (items 8000016, 25328, 11127 and 5x 8000003), one guaranteed per group.
--
-- This patch makes loot pack 8000012 a single guaranteed drop of 4x Thunderstruck Log
-- (item 8000055), matching the skill description. Pack 8000012 has no other users.

DELETE FROM `loots` WHERE `loot_pack_id` = 8000012 AND `id` IN (8000052, 8000053, 8000054);
UPDATE `loots`
SET `item_id` = 8000055, `min_amount` = 4, `max_amount` = 4, `drop_rate` = 10000000, `grade_id` = 0
WHERE `id` = 8000051 AND `loot_pack_id` = 8000012;
