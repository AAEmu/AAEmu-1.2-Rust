//! Retail `/imprison <character>` (WP-SL6b): move an online character to their court's jail.
//!
//! The 1.2 client has the retail name only (evidence `siege-social-2.md` section 1.5). The
//! command reuses the same effect as `crime court jail`: the `FORCIBLY_AWAITING_TRIAL` buff,
//! the teleport to the configured holding cell and the jail position packet.

use super::prelude::*;
use super::retail;

/// `imprison`.
pub struct Imprison;

impl Imprison {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for Imprison {
    fn command_names(&self) -> Vec<String> {
        names(&["imprison"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let Some(name) = args.first() else {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        };
        if args.len() > 1 {
            CommandManager::send_error_text(self, out, "Usage: imprison <character>");
            return Ok(());
        }

        let server = ctx.server()?;
        let Some(target) = retail::online(server, name) else {
            let message = if retail::character_id(server, name).is_some() {
                format!("Character {name} is not online.")
            } else {
                format!("Character {name} was not found.")
            };
            CommandManager::send_error_text(self, out, &message);
            return Ok(());
        };
        let target_name = name_of(&target);
        let faction = target.data().unit.faction_id;
        let region = crate::systems::social::trial::court_room_region_by_faction(
            crate::systems::social::util::mother_id(server, faction),
        );
        if region == crate::systems::social::chat::court_room_region::INVALID {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Character {target_name} is not in a justice region."),
            );
            return Ok(());
        }
        let Some(court_room) = server
            .systems
            .social
            .trials
            .court_rooms()
            .into_iter()
            .find(|room| room.region == region)
        else {
            CommandManager::send_error_text(
                self,
                out,
                &format!("No court room is configured for {target_name}'s region."),
            );
            return Ok(());
        };
        super::sub::crimes::jail_court_room(server, &target, &court_room);
        CommandManager::send_normal_text(
            self,
            out,
            &format!(
                "Moved {target_name} to the jail of {} ({}).",
                court_room.name, court_room.id
            ),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Moves an online character to the holding cell of their faction's court room with the\n\
         FORCIBLY_AWAITING_TRIAL buff, exactly like `crime court jail`. The retail client has\n\
         the name only; the argument is assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::server::GameServer;
    use crate::systems::social::config::{
        CourtRoomConfig, JailConfig, JusticeConfig, SpawnPosition,
    };
    use crate::systems::social::crime::buff;
    use crate::test_support::TEST_ZONE;

    const NUIA: u32 = 148;

    fn pos(x: f32, y: f32, z: f32) -> SpawnPosition {
        SpawnPosition {
            world_id: 0,
            zone_id: TEST_ZONE,
            x,
            y,
            z,
            yaw: 0.0,
        }
    }

    fn jurisdiction_server() -> std::sync::Arc<GameServer> {
        let mut data = GameData::empty();
        for id in [
            buff::PRISONER_NUIAN,
            buff::FORCIBLY_AWAITING_TRIAL,
            buff::CANNOT_ESCAPE,
        ] {
            let mut template = BuffTemplate::simple(id, BuffKind::Good);
            template.duration = 1000;
            data.skills.buffs.insert(id, template);
        }
        for (id, mother) in [(101, NUIA), (NUIA, 0), (161, 0)] {
            data.factions.add(SystemFaction {
                id,
                mother_id: mother,
                ..SystemFaction::default()
            });
        }
        let server = harness::server_with(data);
        server.config.update(|c| {
            c.justice = JusticeConfig {
                court_rooms: vec![CourtRoomConfig {
                    id: 1,
                    name: "Nuian Main Court".into(),
                    faction: NUIA,
                    defendant: pos(150.0, 160.0, 70.0),
                    holding_cell: pos(200.0, 210.0, 60.0),
                }],
                jails: vec![JailConfig {
                    id: 1,
                    name: "Nuian Jail".into(),
                    faction: NUIA,
                    pos: pos(300.0, 310.0, 50.0),
                }],
                ..JusticeConfig::default()
            };
        });
        server.systems.social.trials.load(&server);
        server
    }

    #[tokio::test]
    async fn moves_a_player_to_the_court_jail() {
        let server = jurisdiction_server();
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let mut crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        crook.frames();

        let (handled, out) = harness::run(&server, &gm.p, "imprison Crook").await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "imprison",
                "Moved Crook to the jail of Nuian Main Court (1)."
            )]
        );
        {
            let c = crook.p.data();
            assert!(c.unit.buffs.check_buff(buff::FORCIBLY_AWAITING_TRIAL));
            assert!(c.disabled_set_position);
        }
        let frames = crook.frames();
        assert!(frames
            .iter()
            .any(|(op, _)| *op == crate::opcodes::sc::SCTeleportUnitPacket));
    }

    #[tokio::test]
    async fn argument_and_target_errors() {
        let server = jurisdiction_server();
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        server.names.add_character(3, "Foreigner", 1003);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let _foreigner = {
            let pc = harness::pc(&server, 3, "Foreigner", 120.0, 100.0);
            pc.p.data().unit.faction_id = 161;
            pc
        };

        let (_, out) = harness::run(&server, &gm.p, "imprison").await;
        assert!(out[0].starts_with("Help for |cFFFFFFFF/imprison|r |cFFEEEEAA<character>|r"));
        let (_, out) = harness::run(&server, &gm.p, "imprison Nobody").await;
        assert_eq!(
            out,
            [harness::error(
                "imprison",
                "Character Nobody was not found."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "imprison Owner").await;
        assert_eq!(
            out,
            [harness::error("imprison", "Character Owner is not online.")]
        );
        let (_, out) = harness::run(&server, &gm.p, "imprison Foreigner").await;
        assert_eq!(
            out,
            [harness::error(
                "imprison",
                "Character Foreigner is not in a justice region."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "imprison Foreigner extra").await;
        assert_eq!(
            out,
            [harness::error("imprison", "Usage: imprison <character>")]
        );
    }
}
