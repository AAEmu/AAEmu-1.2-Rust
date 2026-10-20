-- `SpecialEffect.AddCharacterSlot` (special 91, item 8000002 `캐릭터 슬롯확장 주문서` -> skill
-- 8000002): the extra character slots bought for an account. The login server advertises a
-- fixed base slot count; the game server sends this value with `SCGetSlotCountPacket`.

ALTER TABLE `accounts`
  ADD `extra_character_slots` TINYINT UNSIGNED NOT NULL DEFAULT 0
  COMMENT 'Purchased character slots beyond the base account slots'
  AFTER `divine_clock_taken`;
