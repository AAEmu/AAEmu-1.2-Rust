-- The maximum HP a `housings.hp` value is relative to. Era profiles raise castle structures
-- above their template HP (EuAuroria: lodestone 1,000,000 -> 4,100,000, defense tower
-- 600,000 -> 1,500,000), so a stored HP without its basis cannot tell an old-scale row from an
-- already-scaled one. `NULL` means the template `housings.hp` (pre-era rows); the loader scales
-- once when the basis differs from the effective maximum and the next save writes the new basis.
-- See `systems::housing::manager::{load_house_row,row_from_house}`.
ALTER TABLE `housings`
  ADD COLUMN `hp_basis` int DEFAULT NULL COMMENT 'Maximum HP the hp value is relative to; NULL = template hp' AFTER `hp`;
