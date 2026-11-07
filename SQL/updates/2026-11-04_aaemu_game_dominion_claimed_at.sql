-- The moment the current owner claimed the *unowned* territory. A siege capture updates
-- `reign_start_at` but not this column, so only a direct claim starts the
-- `world_var_defaults.var_delay_siege_x_days_after_dominion_declare` siege protection
-- (WP-S6). Existing rows keep 0 (no protection) until they are claimed again.
ALTER TABLE `dominions`
  ADD COLUMN `claimed_at` bigint NOT NULL DEFAULT 0 AFTER `reign_start_at`;
