//! `/questmap <100|200|300|0>`: the "Quest Details Map Display" range (Rust only).
//!
//! The 1.2 client has no given-quest marker distance option; the injected client driver sends
//! this level-0 command after entering the world and when the option is saved. The server then
//! streams safe quest giver / reporter NPCs within the selected range (see
//! `systems::quests::quest_map`).

use super::prelude::*;
use crate::systems::quests::quest_map::{self, Range};

pub struct QuestMap;

impl QuestMap {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for QuestMap {
    fn command_names(&self) -> Vec<String> {
        names(&["questmap"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let server = ctx.server()?;
        let player = ctx.player()?;
        let Some(arg) = args.first() else {
            CommandManager::send_normal_text(
                self,
                out,
                "Usage: /questmap <100|200|300|0> (0 = unlimited)",
            );
            return Ok(());
        };
        let range = if arg.eq_ignore_ascii_case("unlimited") {
            Some(Range::Unlimited)
        } else {
            try_parse::<u32>(arg).and_then(Range::from_meters)
        };
        let Some(range) = range else {
            CommandManager::send_error_text(
                self,
                out,
                "Range must be 100, 200, 300 or 0 (unlimited).",
            );
            return Ok(());
        };
        quest_map::set_range(server, &player, range);
        // The driver sends this on every world entry and every options-window init; a chat
        // confirmation would spam the player and an info line would spam the log, so it is only
        // traced (invalid arguments above still answer the caller).
        tracing::debug!(
            character_id = player.character_id,
            meters = range.meters(),
            "quest details map display range set"
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<100|200|300|0>".into()
    }

    fn get_command_help_text(&self) -> String {
        "[Quest] Sets the range of quest giver markers on the map (0 = unlimited).".into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use super::*;
    use crate::data::GameData;

    #[tokio::test]
    async fn questmap_sets_the_range_at_level_zero_and_rejects_bad_values() {
        let server = harness::server_with(GameData::empty());
        server
            .systems
            .services
            .commands
            .access_levels
            .load(&[("questmap".into(), 0)]);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (handled, out) = harness::run_at(&server, &gm.p, "questmap 300", 0).await;
        assert!(handled);
        assert!(out.is_empty(), "the driver's per-login command is silent");
        assert_eq!(
            server.systems.quests.quest_map.range_of(gm.p.character_id),
            Range::Meters300
        );

        // A second change inside the same second is recorded (the tick applies it).
        let (handled, out) = harness::run_at(&server, &gm.p, "questmap 0", 0).await;
        assert!(handled);
        assert!(out.is_empty());
        assert_eq!(
            server.systems.quests.quest_map.range_of(gm.p.character_id),
            Range::Unlimited
        );

        // The driver repeats the stored range: silent, and nothing changes.
        let (handled, out) = harness::run_at(&server, &gm.p, "questmap unlimited", 0).await;
        assert!(handled);
        assert!(out.is_empty());
        assert_eq!(
            server.systems.quests.quest_map.range_of(gm.p.character_id),
            Range::Unlimited
        );

        let (handled, out) = harness::run_at(&server, &gm.p, "questmap 150", 0).await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::error(
                "questmap",
                "Range must be 100, 200, 300 or 0 (unlimited)."
            )]
        );
    }
}
