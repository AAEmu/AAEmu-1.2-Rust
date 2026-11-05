//! Retail `/set_jury <character> <trialId>` (WP-SL6b): seat a player on an active trial's
//! jury without an invitation.
//!
//! The 1.2 client has the retail name only (evidence `siege-social-2.md` section 1.5), so the
//! argument shape is assumed. Seating goes through `TrialManager.force_summon_jury`, which
//! uses the same `TrialData.SummonJuryMember` effect path as an accepted jury invite (teleport,
//! seat packet, jury buffs and trial records); only the invitation and queue checks are
//! skipped for the GM command.

use super::prelude::*;
use super::retail;
use crate::systems::social::trial::ForceJuryError;

/// `set_jury`.
pub struct SetJury;

impl SetJury {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

fn force_error(error: ForceJuryError, trial_id: u32, name: &str) -> String {
    match error {
        ForceJuryError::NoTrial => format!("Trial {trial_id} does not exist."),
        ForceJuryError::NotSummoning => {
            format!("Trial {trial_id} is not awaiting jury summons.")
        }
        ForceJuryError::NotInMainWorld => {
            format!("Character {name} is not in the main world.")
        }
        ForceJuryError::AlreadyInTrial => {
            format!("Character {name} is already on a jury.")
        }
        ForceJuryError::NoSeat => format!("Trial {trial_id} has no free jury seat."),
    }
}

impl Command for SetJury {
    fn command_names(&self) -> Vec<String> {
        names(&["set_jury"])
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
            CommandManager::send_error_text(self, out, "Usage: set_jury <character> <trialId>");
            return Ok(());
        }
        let Some(trial_id) = try_parse::<u32>(&args[1]) else {
            CommandManager::send_error_text(self, out, &format!("Invalid trial id: {}", args[1]));
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
        match server.systems.social.trials.force_summon_jury(
            server,
            trial_id,
            &target,
            crate::systems::social::util::now_ms(),
        ) {
            Ok(()) => CommandManager::send_normal_text(
                self,
                out,
                &format!("Seated {target_name} on the jury of trial {trial_id}."),
            ),
            Err(error) => {
                CommandManager::send_error_text(
                    self,
                    out,
                    &force_error(error, trial_id, &target_name),
                );
            }
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character> <trialId>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Seats an online character in the first free jury seat of a trial that is awaiting\n\
         jury summons, without an invitation or queue entry. The retail client has the name\n\
         only; the positional arguments are assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::harness;
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::models::doodad::Doodad;
    use crate::server::GameServer;
    use crate::systems::social::config::{
        CourtRoomConfig, JailConfig, JusticeConfig, SpawnPosition,
    };
    use crate::systems::social::crime::buff;
    use crate::systems::social::util;
    use crate::test_support::TEST_ZONE;
    use crate::world::{DoodadRef, Entity};

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

    fn seat(id: u32, x: f32) -> DoodadRef {
        let mut doodad = Doodad::new(4937);
        doodad
            .transform
            .set_position(x, 500.0, 20.0, 0.0, 0.0, 1.25);
        Entity::new(id, doodad)
    }

    fn jurisdiction_server() -> Arc<GameServer> {
        let mut data = GameData::empty();
        for id in [
            buff::PRISONER_NUIAN,
            buff::FORCIBLY_AWAITING_TRIAL,
            buff::JURY,
            buff::TRIAL_DEFENDANT,
            buff::WANTED,
            4832,
            buff::COURT_HOUSE,
            buff::CANNOT_ESCAPE,
        ] {
            let mut template = BuffTemplate::simple(id, BuffKind::Good);
            template.duration = 1000;
            data.skills.buffs.insert(id, template);
        }
        for (id, mother) in [(101, NUIA), (NUIA, 0)] {
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
            .systems
            .social
            .trials
            .initialize_doodads(&[("justice.1.seat.1".into(), seat(0x900, 400.0))]);
        server
    }

    async fn summoning_trial(server: &GameServer) -> (crate::world::PlayerRef, u32) {
        let defendant = harness::pc(server, 2, "Defendant", 120.0, 100.0);
        {
            let mut c = defendant.p.data();
            c.crime_point = 50;
            c.infamy_point = 50;
        }
        // An empty jury queue makes `UpdateTrialStates` skip the jury wait; a queued
        // (not yet invited) eligible player keeps the trial in `AwaitingJurySummons`.
        server.systems.social.trials.update_jury_queues(server);
        let tid = server
            .systems
            .social
            .trials
            .arrest_criminal(server, &defendant.p, None)
            .expect("trial case");
        server.systems.social.trials.reply_imprison_or_trial(
            server,
            &defendant.p,
            true,
            util::now_ms(),
        );
        server
            .systems
            .social
            .trials
            .update_tick(server, util::now_ms() + 1)
            .await;
        assert_eq!(
            server.systems.social.trials.get_trial(tid).unwrap().step,
            crate::systems::social::trial::trial_step::AWAITING_JURY_SUMMONS
        );
        (defendant.p, tid)
    }

    #[tokio::test]
    async fn seats_a_player_without_an_invitation() {
        let server = jurisdiction_server();
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Defendant", 1002);
        server.names.add_character(3, "Juror", 1003);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let queued = harness::pc(&server, 9, "Queued", 150.0, 100.0);
        queued.p.data().jury_point = 1;
        let (_defendant, tid) = summoning_trial(&server).await;
        let mut juror = harness::pc(&server, 3, "Juror", 130.0, 100.0);
        juror.frames();

        let (handled, out) = harness::run(&server, &gm.p, &format!("set_jury Juror {tid}")).await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "set_jury",
                &format!("Seated Juror on the jury of trial {tid}.")
            )]
        );
        let seat_holder = server
            .systems
            .social
            .trials
            .get_trial(tid)
            .unwrap()
            .jury
            .get(&1)
            .and_then(|seat| seat.jury_member.as_ref().map(|m| m.character_id));
        assert_eq!(seat_holder, Some(3));
        let frames = juror.frames();
        assert!(frames
            .iter()
            .any(|(op, _)| *op == crate::opcodes::sc::SCSummonJuryPacket));
        assert!(frames
            .iter()
            .any(|(op, _)| *op == crate::opcodes::sc::SCJuryBeSeatedPacket));
        assert!(frames
            .iter()
            .any(|(op, _)| *op == crate::opcodes::sc::SCTeleportUnitPacket));
    }

    #[tokio::test]
    async fn refuses_missing_trials_unknown_targets_and_a_full_jury() {
        let server = jurisdiction_server();
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Defendant", 1002);
        server.names.add_character(3, "Juror", 1003);
        server.names.add_character(4, "Second", 1004);
        server.names.add_character(5, "Offline", 1005);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let queued = harness::pc(&server, 9, "Queued", 150.0, 100.0);
        queued.p.data().jury_point = 1;
        let (_, tid) = summoning_trial(&server).await;
        let _juror = harness::pc(&server, 3, "Juror", 130.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, &format!("set_jury Juror {tid}")).await;
        assert_eq!(
            out,
            [harness::normal(
                "set_jury",
                &format!("Seated Juror on the jury of trial {tid}.")
            )]
        );

        let _second = harness::pc(&server, 4, "Second", 140.0, 100.0);
        let (_, out) = harness::run(&server, &gm.p, &format!("set_jury Second {tid}")).await;
        assert_eq!(
            out,
            [harness::error(
                "set_jury",
                &format!("Trial {tid} has no free jury seat.")
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_jury Juror 9999").await;
        assert_eq!(
            out,
            [harness::error("set_jury", "Trial 9999 does not exist.")]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_jury Nobody 9999").await;
        assert_eq!(
            out,
            [harness::error(
                "set_jury",
                "Character Nobody was not found."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_jury Offline 9999").await;
        assert_eq!(
            out,
            [harness::error(
                "set_jury",
                "Character Offline is not online."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_jury").await;
        assert!(
            out[0].starts_with("Help for |cFFFFFFFF/set_jury|r |cFFEEEEAA<character> <trialId>|r")
        );
        let (_, out) = harness::run(&server, &gm.p, "set_jury Juror abc").await;
        assert_eq!(out, [harness::error("set_jury", "Invalid trial id: abc")]);
        let (_, out) = harness::run(&server, &gm.p, "set_jury Juror").await;
        assert_eq!(
            out,
            [harness::error(
                "set_jury",
                "Usage: set_jury <character> <trialId>"
            )]
        );
    }
}
