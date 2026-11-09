-- Mail retention upgrade marker (no schema change).
--
-- The 1.2 mail_guide windows (5/30 normal, 5/90 system) replace the old flat 14-day policy.
-- The game server reads this row's `updates.install_date` as the first start with this
-- version and gives mail that already existed then one full retention window
-- (`Mail.RetentionGraceStartUnix`, `MailRetention::grace_start_unix`), so an upgrade cannot
-- delete old mail on its first tick. Do not rename this file without updating
-- `MAIL_RETENTION_UPDATE` in `rust/crates/aaemu-game/src/main.rs`.
DO 0;
