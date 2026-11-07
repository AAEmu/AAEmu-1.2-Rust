-- Wanted-player bounty (`systems::social::bounty`): the pledges whose copper already left the
-- payer's purse. A row stays after payout as the record of who contributed; `paid` says the
-- whole accumulated total was credited to the arresting player. `created` is UTC Unix seconds.
-- `id` is the ledger's id, so a restart continues allocation above the highest saved id.
--
-- There is no expiry, fee or refund column: the 1.2 client evidence does not define them
-- (PORTING.md, BOUNTY-1 / BOUNTY-2). No foreign key to `characters`.

CREATE TABLE IF NOT EXISTS `bounty_pledges` (
  `id`        bigint unsigned NOT NULL COMMENT 'BountyLedger pledge id',
  `payer_id`  int unsigned NOT NULL COMMENT 'characters.id of the payer',
  `target_id` int unsigned NOT NULL COMMENT 'characters.id of the wanted target',
  `amount`    bigint NOT NULL COMMENT 'Escrowed copper',
  `created`   bigint NOT NULL COMMENT 'Unix seconds UTC',
  `paid`      tinyint unsigned NOT NULL DEFAULT 0 COMMENT 'Paid to the arresting player',
  PRIMARY KEY (`id`),
  KEY `target_id` (`target_id`, `paid`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Wanted-player bounty pledges (escrowed copper)';
