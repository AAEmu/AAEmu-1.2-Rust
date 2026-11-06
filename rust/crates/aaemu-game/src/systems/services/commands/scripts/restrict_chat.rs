//! Retail `/restrict_chat <character> [minutes]` (WP-SL4b): a persisted chat mute.
//!
//! The 1.2 client's command string table has the retail name only (`siege-social-2.md` section
//! 1.5); how it takes its arguments is UNKNOWN. The shape here is assumed: an online or offline
//! character name and an optional duration in minutes, default [`DEFAULT_MINUTES`], `0` lifts
//! the mute. The mute is `characters.chat_restricted_until` (UTC), loaded and saved with the
//! character; an online target is written through its live character, an offline one with a
//! direct row update. `world::session::send_chat_message` refuses every outgoing message while
//! the value is in the future, and the expiry lifts the mute automatically.
//!
//! The retail duration is UNKNOWN, so the default is INFERRED (`CHAT-RESTRICT-1`). Starting or
//! lifting a mute of an online target also sends `SCResultRestrictCheck(code 0, result 1 / 0)`,
//! which the 1.2 client turns into its chat-restriction flag and the `restrict_start` /
//! `restrict_end` system line (`FUN_391e0ea0`).

use futures::future::BoxFuture;

use super::prelude::*;
use super::retail;
use crate::systems::services::character_packets::SCResultRestrictCheckPacket;

/// Duration applied when `<minutes>` is omitted: one hour (the retail argument is UNKNOWN).
pub const DEFAULT_MINUTES: i64 = 60;

/// `restrict_chat`.
pub struct RestrictChat;

impl RestrictChat {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }

    fn usage(out: &mut dyn MessageOutput) {
        out.send_message("Usage: restrict_chat <character> [minutes]");
    }
}

impl Command for RestrictChat {
    fn command_names(&self) -> Vec<String> {
        names(&["restrict_chat"])
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        anyhow::bail!("RestrictChat runs through execute_async")
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if args.is_empty() || args.len() > 2 {
                CommandManager::send_default_help_text(self, out);
                return Ok(());
            }
            let minutes = match args.get(1) {
                Some(raw) => match raw.parse::<i64>() {
                    Ok(minutes) if minutes >= 0 => minutes,
                    _ => {
                        Self::usage(out);
                        return Ok(());
                    }
                },
                None => DEFAULT_MINUTES,
            };
            let server = ctx.server()?;
            let name = &args[0];
            let Some(character_id) = retail::character_id(server, name) else {
                CommandManager::send_error_text(
                    self,
                    out,
                    &format!("Character {name} was not found."),
                );
                return Ok(());
            };
            let until = if minutes == 0 {
                0
            } else {
                chrono::Utc::now()
                    .timestamp()
                    .saturating_add(minutes.saturating_mul(60))
            };
            let target = retail::online(server, name);
            match &target {
                // The live character is saved by the save manager; no second DB write that a
                // concurrent character save could race.
                Some(target) => target.data().chat_restricted_until = until,
                // Offline: READ + save of the row is not involved, so write the column now.
                None => {
                    sqlx::query(
                        "UPDATE `characters` SET `chat_restricted_until` = ? WHERE `id` = ?",
                    )
                    .bind(crate::db::from_unix(until))
                    .bind(character_id)
                    .execute(&server.db)
                    .await?;
                }
            }
            // The client's `SCResultRestrictCheck` handler `FUN_391e0ea0` turns `code 0,
            // result 1` into its chat-restriction flag plus `restrict_start`, and `result 0`
            // into `restrict_end`; only a connected target can receive it.
            if let Some(target) = &target {
                target.send(SCResultRestrictCheckPacket {
                    character_id,
                    code: 0,
                    result: u8::from(minutes != 0),
                });
            }
            let target_name = retail::character_name(server, character_id);
            if minutes == 0 {
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!("Lifted the chat restriction of {target_name}."),
                );
            } else {
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!("Chat of {target_name} is restricted for {minutes} minute(s)."),
                );
            }
            Ok(())
        })
    }

    fn get_command_line_help(&self) -> String {
        "<character> [minutes]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Restricts (or, with 0 minutes, lifts) a character's chat, online or offline. The mute\n\
         is persisted (characters.chat_restricted_until) and expires automatically. The retail\n\
         argument shape is unknown: the optional minute count and the default of 60 are assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;

    #[tokio::test]
    async fn restricts_an_online_character_and_lifts_with_zero() {
        let server = harness::server();
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let mut crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        let before = chrono::Utc::now().timestamp();

        let (handled, out) = harness::run(&server, &gm.p, "restrict_chat Crook 30").await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "restrict_chat",
                "Chat of Crook is restricted for 30 minute(s)."
            )]
        );
        let until = crook.p.data().chat_restricted_until;
        assert!(
            (before + 1_800..=before + 1_801).contains(&until),
            "{until}"
        );
        // The target's own connection gets `SCResultRestrictCheck(character, code 0, result 1)`
        // (`FUN_391e0ea0` -> chat restriction flag + `restrict_start`).
        assert_eq!(
            crook.frames(),
            vec![(
                crate::opcodes::sc::SCResultRestrictCheckPacket,
                vec![2, 0, 0, 0, 0, 1]
            )]
        );

        let (_, out) = harness::run(&server, &gm.p, "restrict_chat Crook 0").await;
        assert_eq!(
            out,
            [harness::normal(
                "restrict_chat",
                "Lifted the chat restriction of Crook."
            )]
        );
        assert_eq!(crook.p.data().chat_restricted_until, 0);
        // Lifting sends `result 0` (chat flag restored + `restrict_end`).
        assert_eq!(
            crook.frames(),
            vec![(
                crate::opcodes::sc::SCResultRestrictCheckPacket,
                vec![2, 0, 0, 0, 0, 0]
            )]
        );
    }

    #[tokio::test]
    async fn the_default_duration_is_applied() {
        let server = harness::server();
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        let before = chrono::Utc::now().timestamp();
        let (_, out) = harness::run(&server, &gm.p, "restrict_chat Crook").await;
        assert_eq!(
            out,
            [harness::normal(
                "restrict_chat",
                "Chat of Crook is restricted for 60 minute(s)."
            )]
        );
        let until = crook.p.data().chat_restricted_until;
        assert!(
            (before + 3_600..=before + 3_601).contains(&until),
            "{until}"
        );
    }

    #[tokio::test]
    async fn argument_and_target_errors() {
        let server = harness::server();
        server.names.add_character(1, "Gm", 1001);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "restrict_chat").await;
        assert!(out[0]
            .starts_with("Help for |cFFFFFFFF/restrict_chat|r |cFFEEEEAA<character> [minutes]|r"));
        for text in ["restrict_chat Nobody 5", "restrict_chat Nobody 5 extra"] {
            let (_, out) = harness::run(&server, &gm.p, text).await;
            if text.ends_with("extra") {
                assert!(out[0].starts_with("Help for |cFFFFFFFF/restrict_chat|r"));
            } else {
                assert_eq!(
                    out,
                    [harness::error(
                        "restrict_chat",
                        "Character Nobody was not found."
                    )]
                );
            }
        }
        for text in ["restrict_chat Crook abc", "restrict_chat Crook -5"] {
            let (_, out) = harness::run(&server, &gm.p, text).await;
            assert_eq!(
                out,
                ["Usage: restrict_chat <character> [minutes]".to_string()],
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn a_non_gm_cannot_run_the_command() {
        use crate::config::AppConfig;

        let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
        if !app.join("Config.json").exists() {
            return;
        }
        let args: Vec<String> = [
            "--LoginNetwork:Port=1234",
            "/Connections:MySQLProvider:Port",
            "3306",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let config = AppConfig::load_from(&app, None, Vec::new(), &args).unwrap();
        assert_eq!(config.access_level_of("restrict_chat"), Some(100));
        let server = harness::server();
        server
            .systems
            .services
            .commands
            .access_levels
            .load(&config.access_level);
        server.names.add_character(1, "Gm", 1001);
        let player = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let (handled, out) = harness::run_at(&server, &player.p, "restrict_chat Nobody 5", 0).await;
        assert!(handled);
        assert_eq!(out, ["|cFFFF0000Insufficient privileges.|r"]);
    }

    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
    async fn private_mysql_offline_character_is_restricted_in_the_row() {
        use crate::db_character::save_character;
        use crate::models::unit_model::UnitCustomModelParams;
        use crate::systems::social::achievement_persistence::temporary_shadow_definition;
        use crate::test_support::test_character;
        use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
        use std::str::FromStr;

        let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
            return;
        };
        let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
        assert_eq!(
            options.get_port(),
            3317,
            "only private MySQL3317 is allowed"
        );
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("private MySQL");
        for table in [
            "characters",
            "accounts",
            "options",
            "abilities",
            "actabilities",
            "appellations",
            "character_active_buffs",
            "character_cooldowns",
            "portal_book_coords",
            "portal_visited_district",
            "friends",
            "blocked",
            "skills",
            "quests",
            "completed_quests",
            "character_records",
            "character_achievements",
            "character_crafts",
        ] {
            let (_, definition): (String, String) =
                sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            sqlx::query(&temporary_shadow_definition(&definition))
                .execute(&pool)
                .await
                .unwrap();
        }

        let mut target = test_character(2, "Offline", 100.0, 100.0);
        target.account_id = 1002;
        target.unit.model_params = UnitCustomModelParams::default();
        target.created = chrono::Utc::now().timestamp();
        let mut tx = pool.begin().await.unwrap();
        save_character(&mut tx, &mut target, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let server = harness::server_with_db(crate::data::GameData::empty(), pool.clone());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Offline", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let before = chrono::Utc::now().timestamp();
        let (_, out) = harness::run(&server, &gm.p, "restrict_chat Offline 30").await;
        assert_eq!(
            out,
            [harness::normal(
                "restrict_chat",
                "Chat of Offline is restricted for 30 minute(s)."
            )]
        );
        let until: chrono::NaiveDateTime =
            sqlx::query_scalar("SELECT `chat_restricted_until` FROM `characters` WHERE `id` = 2")
                .fetch_one(&pool)
                .await
                .unwrap();
        let until = until.and_utc().timestamp();
        assert!(
            (before + 1_800..=before + 1_801).contains(&until),
            "{until}"
        );

        let (_, out) = harness::run(&server, &gm.p, "restrict_chat Offline 0").await;
        assert_eq!(
            out,
            [harness::normal(
                "restrict_chat",
                "Lifted the chat restriction of Offline."
            )]
        );
        let until: chrono::NaiveDateTime =
            sqlx::query_scalar("SELECT `chat_restricted_until` FROM `characters` WHERE `id` = 2")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(crate::db::unix(Some(until)), 0);
        pool.close().await;
    }
}
