-- Dedicated crafter character id for items created by crafting (items-crafting#11 /
-- mysql-schema#8 crafter half).
--
-- The 1.2 client's item record carries one u32 between `lifespanMins` and `worldId`
-- (`Item.Write`; client reader `FUN_3961f500` at `x2game.dll.c:1301392`). The tooltip builder
-- `FUN_39423560` reads it from item `+0x14` and resolves the character name through
-- `FUN_390192e0` / `FUN_39018560`; the world comes from the `worldId` byte in the same record.
-- The 1.2-era C# overloaded `made_unit_id` for this value; that column also holds the fish
-- template id and other non-crafter values, so crafted products now record the crafter in
-- `crafter_id` and the serializer writes it to the same client field. Rows written before this
-- column existed keep their value in `made_unit_id`, which stays the fallback
-- (`Item::crafter`), so no existing item loses the field it sends.
--
-- Guarded so a database that already has the column (a fresh install applies the base
-- `SQL/aaemu_game.sql`, which contains it) is left untouched when the updater replays or a
-- manual run repeats this script.

SET @item_crafter_ddl = (
  SELECT IF(
    EXISTS(
      SELECT 1
      FROM `INFORMATION_SCHEMA`.`COLUMNS`
      WHERE `TABLE_SCHEMA` = DATABASE()
        AND `TABLE_NAME` = 'items'
        AND `COLUMN_NAME` = 'crafter_id'
    ),
    'SELECT 1',
    'ALTER TABLE `items` ADD COLUMN `crafter_id` int unsigned NOT NULL DEFAULT ''0'' COMMENT ''Character id of the crafter (crafted items only, 0 = not crafted)'' AFTER `made_unit_id`'
  )
);

PREPARE item_crafter_stmt FROM @item_crafter_ddl;
EXECUTE item_crafter_stmt;
DEALLOCATE PREPARE item_crafter_stmt;
