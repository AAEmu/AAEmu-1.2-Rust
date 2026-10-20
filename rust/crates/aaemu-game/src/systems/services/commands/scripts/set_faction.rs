//! `Scripts/Commands/SetFaction.cs`.

use super::prelude::*;

/// `SetFaction`.
pub struct SetFaction;

impl SetFaction {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for SetFaction {
    fn command_names(&self) -> Vec<String> {
        names(&["setfaction", "set_faction"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let server = ctx.server()?;
        let character = ctx.player()?;
        let Some(target_unit) = current_target(server, &character).filter(WorldObject::is_unit)
        else {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        };

        if args.is_empty() {
            // `Faction?.Name` / `Faction?.Id` (null for an unknown faction).
            let faction_id = target_unit.with_base_unit(|b| b.faction_id());
            let (name, id) = match server.game_data.factions.get(faction_id) {
                Some(f) => (
                    f.name.clone(),
                    enum_member_name(i64::from(f.id), FACTIONS_ENUM),
                ),
                None => (String::new(), String::new()),
            };
            CommandManager::send_error_text(
                self,
                out,
                &format!("Current target is from faction: {name} ({id})"),
            );
            // CommandManager.SendDefaultHelpText(this, messageOutput);
            return Ok(());
        }

        let faction =
            enum_try_parse_ci(&args[0], FACTIONS_ENUM, 0, i64::from(u32::MAX)).unwrap_or(0);
        if faction == 0 {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Invalid faction name or number {}", args[0]),
            );
            return Ok(());
        }

        if server.game_data.factions.get(faction as u32).is_none() {
            CommandManager::send_error_text(
                self,
                out,
                &format!("{} is not a valid faction", args[0]),
            );
            return Ok(());
        }

        // C#: the caller's faction is changed, not the selected target's.
        // A GM change is persistent (unlike the duel / battlefield factions).
        crate::systems::social::faction::set_faction_persistent(server, &character, faction as u32);
        CommandManager::send_normal_text(
            self,
            out,
            &format!(
                "Faction set to: {} ({faction})",
                enum_member_name(faction, FACTIONS_ENUM)
            ),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<faction name or id>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Gets or sets selected target's faction".into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::add_xp::tests::spawn_npc;
    use super::super::harness;
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::opcodes::sc;

    #[tokio::test]
    async fn setfaction_reports_and_changes_the_faction() {
        let mut data = GameData::empty();
        for (id, name) in [(101, "Nuian"), (115, "Monster"), (200, "Custom")] {
            data.factions.add(SystemFaction {
                id,
                name: name.into(),
                ..SystemFaction::default()
            });
        }
        let server = harness::server_with(data);
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().unit.faction_id = 101;

        let (_, out) = harness::run(&server, &gm.p, "setfaction 101").await;
        assert_eq!(
            out,
            ["Help for |cFFFFFFFF/setfaction|r |cFFEEEEAA<faction name or id>|r\n|cFF888888Gets or sets selected target's faction|r"]
        );

        let npc = spawn_npc(&server, 0x500, 10, 105.0, 100.0, 1_000);
        npc.data().unit.faction_id = 115;
        gm.p.data().unit.current_target = npc.obj_id;
        let (_, out) = harness::run(&server, &gm.p, "setfaction").await;
        assert_eq!(
            out,
            [harness::error(
                "setfaction",
                "Current target is from faction: Monster (Monstrosity)"
            )]
        );
        npc.data().unit.faction_id = 7;
        let (_, out) = harness::run(&server, &gm.p, "setfaction").await;
        assert_eq!(
            out,
            [harness::error(
                "setfaction",
                "Current target is from faction:  ()"
            )]
        );

        let (_, out) = harness::run(&server, &gm.p, "setfaction nope").await;
        assert_eq!(
            out,
            [harness::error(
                "setfaction",
                "Invalid faction name or number nope"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "setfaction invalid").await;
        assert_eq!(
            out,
            [harness::error(
                "setfaction",
                "Invalid faction name or number invalid"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "setfaction pirate").await;
        assert_eq!(
            out,
            [harness::error(
                "setfaction",
                "pirate is not a valid faction"
            )]
        );

        gm.packets();
        let (_, out) = harness::run(&server, &gm.p, "set_faction monstrosity").await;
        assert_eq!(out, ["[setfaction] Faction set to: Monstrosity (115)"]);
        // The caller changed faction, not the target.
        assert_eq!(gm.p.data().unit.faction_id, 115);
        assert_eq!(npc.data().unit.faction_id, 7);
        assert!(gm
            .packets()
            .iter()
            .any(|(op, _)| *op == sc::SCUnitFactionChangedPacket));
        // Undefined enum values parse as numbers.
        let (_, out) = harness::run(&server, &gm.p, "setfaction 200").await;
        assert_eq!(out, ["[setfaction] Faction set to: 200 (200)"]);
        assert_eq!(gm.p.data().unit.faction_id, 200);
        // A GM change is persistent: saved as is, not temporary.
        assert!(!gm.p.data().unit.faction_temporary);
        assert_eq!(crate::db_character::persisted_faction(&gm.p.data()), 200);
    }

    /// `/setfaction` during a temporary (duel / battlefield) faction replaces it for good.
    #[tokio::test]
    async fn setfaction_is_persistent_even_over_a_temporary_faction() {
        let mut data = GameData::empty();
        for (id, name) in [(101, "Nuian"), (115, "Monster"), (160, "Blue")] {
            data.factions.add(SystemFaction {
                id,
                name: name.into(),
                ..SystemFaction::default()
            });
        }
        let server = harness::server_with(data);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().unit.faction_id = 101;
        let npc = spawn_npc(&server, 0x500, 10, 105.0, 100.0, 1_000);
        gm.p.data().unit.current_target = npc.obj_id;
        crate::systems::social::faction::set_temporary_faction(&server, &gm.p, 160);
        assert!(gm.p.data().unit.faction_temporary);
        assert_eq!(crate::db_character::persisted_faction(&gm.p.data()), 101);
        let (_, out) = harness::run(&server, &gm.p, "setfaction 115").await;
        assert_eq!(out, ["[setfaction] Faction set to: Monstrosity (115)"]);
        assert!(!gm.p.data().unit.faction_temporary);
        assert_eq!(gm.p.data().unit.faction_id, 115);
        assert_eq!(crate::db_character::persisted_faction(&gm.p.data()), 115);
    }

    fn two_faction_server() -> std::sync::Arc<crate::server::GameServer> {
        let mut data = GameData::empty();
        for (id, name) in [(101, "Nuian"), (160, "Blue")] {
            data.factions.add(SystemFaction {
                id,
                name: name.into(),
                ..SystemFaction::default()
            });
        }
        harness::server_with(data)
    }

    /// `/setfaction` to the faction the character already has must return (it once locked the
    /// character twice in one expression and deadlocked).
    #[tokio::test]
    async fn setfaction_to_the_current_faction_does_not_deadlock() {
        let server = two_faction_server();
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().unit.faction_id = 101;
        let p = gm.p.clone();
        let call = tokio::task::spawn_blocking(move || {
            crate::systems::social::faction::set_faction_persistent(&server, &p, 101);
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("set_faction_persistent deadlocked on an unchanged faction")
            .unwrap();
        assert_eq!(gm.p.data().unit.faction_id, 101);
    }

    /// `/setfaction` to the faction a temporary change currently shows makes it permanent.
    #[tokio::test]
    async fn setfaction_to_the_temporary_faction_makes_it_persistent() {
        let server = two_faction_server();
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().unit.faction_id = 101;
        crate::systems::social::faction::set_temporary_faction(&server, &gm.p, 160);
        assert!(gm.p.data().unit.faction_temporary);
        crate::systems::social::faction::set_faction_persistent(&server, &gm.p, 160);
        assert_eq!(gm.p.data().unit.faction_id, 160);
        assert!(!gm.p.data().unit.faction_temporary);
        assert_eq!(crate::db_character::persisted_faction(&gm.p.data()), 160);
    }
}
