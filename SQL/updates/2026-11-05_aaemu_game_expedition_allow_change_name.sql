-- Guild rename right. A GM grants a guild the right to rename itself once (`/allowguildrename`);
-- the guild leader then gets the client's rename dialog (`SCExpeditionShowRenameUI`, also on the
-- next login while the right is pending) and `CSRenameExpedition` consumes the right.
-- `SystemFaction.allow_change_name` is sent to the client as this flag.

ALTER TABLE `expeditions`
  ADD COLUMN `allow_change_name` tinyint unsigned NOT NULL DEFAULT 0 COMMENT '1 = the leader may rename the guild once' AFTER `mother`;
