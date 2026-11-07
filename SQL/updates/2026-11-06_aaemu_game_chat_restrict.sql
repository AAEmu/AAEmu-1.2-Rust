-- Persisted chat mute for the retail `/restrict_chat` GM command (WP-SL4b). `0`-valued
-- (`0001-01-01 00:00:00`) means not muted; otherwise the UTC instant the mute expires.
-- `world::session::send_chat_message` refuses a muted sender's message before routing and
-- lifts the mute automatically once the instant has passed. The value is loaded / saved with
-- the character (`db_character::load_character` / `save_character`).

ALTER TABLE `characters`
  ADD COLUMN `chat_restricted_until` datetime NOT NULL DEFAULT '0001-01-01 00:00:00' AFTER `leave_time`;
