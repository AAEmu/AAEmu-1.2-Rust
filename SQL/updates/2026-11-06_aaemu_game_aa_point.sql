-- Per-character AA-point wallet (WP-AA1). The 1.2 client reads the two balances from the
-- carried and bank purses in `SCCharacterState` (third / fourth `i64` moneyAmount), and they
-- live on the character, not the account. Both are `bigint` like `money` / `money2`; 0 is the
-- value the client showed before this update (the packet wrote zeroes).

ALTER TABLE `characters`
ADD COLUMN `aa_point` bigint NOT NULL DEFAULT '0' AFTER `money2`,
ADD COLUMN `bank_aa_point` bigint NOT NULL DEFAULT '0' AFTER `aa_point`;
