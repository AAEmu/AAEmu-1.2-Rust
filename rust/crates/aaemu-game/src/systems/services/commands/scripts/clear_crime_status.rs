//! Retail `/clear_crime_status` (WP-SL6b): reset a character's crime and infamy points.
//!
//! The 1.2 client has the retail name only (evidence `siege-social-2.md` section 1.5), so the
//! effect is the assumed meaning: current crime points and total infamy go to zero through
//! the gameplay setters (`CrimePoint` / `InfamyPoint`), which also drop the wanted buff.

use super::prelude::*;
use super::retail;
use crate::systems::social::crime;
use crate::systems::social::packets::trial::SCCrimeChangedPacket;

/// `clear_crime_status`.
pub struct ClearCrimeStatus;

impl ClearCrimeStatus {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for ClearCrimeStatus {
    fn command_names(&self) -> Vec<String> {
        names(&["clear_crime_status"])
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
            CommandManager::send_error_text(self, out, "Usage: clear_crime_status <character>");
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
        let now = crate::world::now_ms();
        let (packet, change, old) = {
            let mut c = target.data();
            let old = (c.crime_point, c.infamy_point);
            let mut change = crime::set_crime_point(server, &mut c, 0, now);
            change = change.merge(crime::set_infamy_point(server, &mut c, 0, now));
            let packet = SCCrimeChangedPacket {
                points: -i32::from(old.0),
                crime_points: c.crime_point,
                infamy_points: c.infamy_point,
                crime_state: crime::crime_state(&c),
            };
            (packet, change, old)
        };
        if !change.is_empty() {
            crate::systems::social::faction::apply_crime_faction_change(server, &target, change);
        }
        if old != (0, 0) {
            target.send(packet);
            crate::systems::dispatch_buff_events(server, &WorldObject::from(&target));
        }
        CommandManager::send_normal_text(
            self,
            out,
            &format!(
                "Cleared {target_name}'s crime status (crime {}, infamy {}).",
                old.0, old.1
            ),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Sets an online character's current crime points and total infamy to zero through the\n\
         normal CrimePoint / InfamyPoint setters (the wanted buff is removed when the crime\n\
         points fall below the threshold). The retail client has the name only; online-only\n\
         is assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::GameData;
    use crate::db_character::BUFF_CONTEMPTUOUS;
    use crate::systems::social::crime::{self, buff};

    fn data() -> GameData {
        let mut data = GameData::empty();
        for id in [buff::WANTED, BUFF_CONTEMPTUOUS] {
            data.skills
                .buffs
                .insert(id, BuffTemplate::simple(id, BuffKind::Bad));
        }
        data
    }

    #[tokio::test]
    async fn clears_crime_infamy_and_the_wanted_buff() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let mut crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        {
            let mut c = crook.p.data();
            crime::set_crime_point(&server, &mut c, 60, crate::world::now_ms());
            crime::set_infamy_point(&server, &mut c, 80, crate::world::now_ms());
        }
        assert!(crook.p.data().unit.buffs.check_buff(buff::WANTED));
        crook.frames();

        let (handled, out) = harness::run(&server, &gm.p, "clear_crime_status Crook").await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "clear_crime_status",
                "Cleared Crook's crime status (crime 60, infamy 80)."
            )]
        );
        {
            let c = crook.p.data();
            assert_eq!((c.crime_point, c.infamy_point), (0, 0));
            assert!(!c.unit.buffs.check_buff(buff::WANTED));
            assert_eq!(crime::crime_state(&c), 0);
        }
        let frames = crook.packets();
        let crime: Vec<_> = frames
            .iter()
            .filter(|(op, _)| *op == crate::opcodes::sc::SCCrimeChangedPacket)
            .collect();
        assert_eq!(crime.len(), 1);
        assert_eq!(
            crime[0].1,
            crate::systems::social::packets::body(
                &crate::systems::social::packets::trial::SCCrimeChangedPacket {
                    points: -60,
                    crime_points: 0,
                    infamy_points: 0,
                    crime_state: 0,
                }
            )
        );
    }

    #[tokio::test]
    async fn argument_and_target_errors() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "clear_crime_status").await;
        assert!(
            out[0].starts_with("Help for |cFFFFFFFF/clear_crime_status|r |cFFEEEEAA<character>|r")
        );
        let (_, out) = harness::run(&server, &gm.p, "clear_crime_status Nobody").await;
        assert_eq!(
            out,
            [harness::error(
                "clear_crime_status",
                "Character Nobody was not found."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "clear_crime_status Owner").await;
        assert_eq!(
            out,
            [harness::error(
                "clear_crime_status",
                "Character Owner is not online."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "clear_crime_status Owner extra").await;
        assert_eq!(
            out,
            [harness::error(
                "clear_crime_status",
                "Usage: clear_crime_status <character>"
            )]
        );
    }
}
