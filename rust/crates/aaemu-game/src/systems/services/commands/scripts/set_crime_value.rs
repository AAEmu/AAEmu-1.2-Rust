//! Retail `/set_crime_value <character> <value>` (WP-SL6b): set a character's current crime
//! points to an exact value.
//!
//! The 1.2 client's GM console builds `SetCrimeValue(%s, %d)` (evidence
//! `siege-social-2.md` section 1.5). It goes through `CrimePoint` (`crime::set_crime_point`),
//! so the wanted buff and the pirate threshold react exactly like gameplay.

use super::prelude::*;
use super::retail;
use crate::systems::social::crime;
use crate::systems::social::packets::trial::SCCrimeChangedPacket;

/// `set_crime_value`.
pub struct SetCrimeValue;

impl SetCrimeValue {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for SetCrimeValue {
    fn command_names(&self) -> Vec<String> {
        names(&["set_crime_value"])
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
        if args.len() != 2 {
            CommandManager::send_error_text(
                self,
                out,
                "Usage: set_crime_value <character> <value>",
            );
            return Ok(());
        }
        let Some(value) = try_parse::<i32>(&args[1]) else {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Invalid crime value: {}", args[1]),
            );
            return Ok(());
        };
        let Ok(value) = i16::try_from(value) else {
            CommandManager::send_error_text(
                self,
                out,
                "Crime value must be between -32768 and 32767.",
            );
            return Ok(());
        };

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
        let now = crate::world::now_ms();
        let (packet, change, before) = {
            let mut c = target.data();
            let before = c.crime_point;
            let change = crime::set_crime_point(server, &mut c, value, now);
            let after = c.crime_point;
            let packet = SCCrimeChangedPacket {
                points: i32::from(after) - i32::from(before),
                crime_points: after,
                infamy_points: c.infamy_point,
                crime_state: crime::crime_state(&c),
            };
            (packet, change, before)
        };
        if !change.is_empty() {
            crate::systems::social::faction::apply_crime_faction_change(server, &target, change);
        }
        let after = packet.crime_points;
        if packet.points != 0 {
            target.send(packet);
            crate::systems::dispatch_buff_events(server, &WorldObject::from(&target));
        }
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Set {target_name}'s crime points to {after} (was {before})."),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character> <value>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Sets an online character's current crime points to <value> (negative values clamp to\n\
         0, like the CrimePoint setter). The retail console shape is SetCrimeValue(%s, %d);\n\
         online-only is assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::GameData;
    use crate::db_character::BUFF_CONTEMPTUOUS;
    use crate::systems::social::crime::buff;

    fn data() -> GameData {
        let mut data = GameData::empty();
        for id in [buff::WANTED, BUFF_CONTEMPTUOUS] {
            data.skills
                .buffs
                .insert(id, BuffTemplate::simple(id, BuffKind::Bad));
        }
        data
    }

    fn points(body: &[u8]) -> i32 {
        i32::from_le_bytes([body[0], body[1], body[2], body[3]])
    }

    #[tokio::test]
    async fn sets_the_value_and_moves_the_wanted_threshold() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let mut crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        crook.p.data().crime_point = 10;
        crook.frames();

        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Crook 60").await;
        assert_eq!(
            out,
            [harness::normal(
                "set_crime_value",
                "Set Crook's crime points to 60 (was 10)."
            )]
        );
        assert_eq!(crook.p.data().crime_point, 60);
        assert!(crook.p.data().unit.buffs.check_buff(buff::WANTED));
        let frames = crook.packets();
        let crime: Vec<_> = frames
            .iter()
            .filter(|(op, _)| *op == crate::opcodes::sc::SCCrimeChangedPacket)
            .collect();
        assert_eq!(crime.len(), 1);
        assert_eq!(points(&crime[0].1), 50);

        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Crook 5").await;
        assert_eq!(
            out,
            [harness::normal(
                "set_crime_value",
                "Set Crook's crime points to 5 (was 60)."
            )]
        );
        assert_eq!(crook.p.data().crime_point, 5);
        assert!(!crook.p.data().unit.buffs.check_buff(buff::WANTED));
        let frames = crook.packets();
        let crime: Vec<_> = frames
            .iter()
            .filter(|(op, _)| *op == crate::opcodes::sc::SCCrimeChangedPacket)
            .collect();
        assert_eq!(points(&crime[0].1), -55);
    }

    #[tokio::test]
    async fn negative_values_clamp_to_zero() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let mut crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        crook.p.data().crime_point = 20;
        crook.frames();
        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Crook -1").await;
        assert_eq!(
            out,
            [harness::normal(
                "set_crime_value",
                "Set Crook's crime points to 0 (was 20)."
            )]
        );
        assert_eq!(crook.p.data().crime_point, 0);
        let frames = crook.packets();
        let crime: Vec<_> = frames
            .iter()
            .filter(|(op, _)| *op == crate::opcodes::sc::SCCrimeChangedPacket)
            .collect();
        assert_eq!(crime.len(), 1);
        assert_eq!(points(&crime[0].1), -20);
    }

    #[tokio::test]
    async fn argument_and_target_errors() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "set_crime_value").await;
        assert!(out[0]
            .starts_with("Help for |cFFFFFFFF/set_crime_value|r |cFFEEEEAA<character> <value>|r"));
        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Crook").await;
        assert_eq!(
            out,
            [harness::error(
                "set_crime_value",
                "Usage: set_crime_value <character> <value>"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Crook abc").await;
        assert_eq!(
            out,
            [harness::error(
                "set_crime_value",
                "Invalid crime value: abc"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Crook 70000").await;
        assert_eq!(
            out,
            [harness::error(
                "set_crime_value",
                "Crime value must be between -32768 and 32767."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Nobody 1").await;
        assert_eq!(
            out,
            [harness::error(
                "set_crime_value",
                "Character Nobody was not found."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_crime_value Owner 1").await;
        assert_eq!(
            out,
            [harness::error(
                "set_crime_value",
                "Character Owner is not online."
            )]
        );
    }
}
