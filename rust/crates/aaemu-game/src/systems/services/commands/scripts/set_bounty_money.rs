//! Retail `/set_bounty_money` (WP-SL6b): place a GM-funded bounty on a wanted character.
//!
//! The 1.2 client has the retail name only (evidence `siege-social-2.md` section 1.5), so the
//! argument shape and the funding source are assumed: `set_bounty_money <character> <amount>`
//! makes the calling GM the payer of a normal `BountyManager.place` pledge. The copper comes
//! from the GM's purse and the pledge persists through the same `bounty_pledges` rows as a
//! player-placed bounty; the target must be online and wanted, exactly like the gameplay path.

use super::prelude::*;
use super::retail;
use crate::systems::social::bounty::PlacementError;

/// `set_bounty_money`.
pub struct SetBountyMoney;

impl SetBountyMoney {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

fn placement_error(error: PlacementError) -> &'static str {
    match error {
        PlacementError::TargetOffline => "The target is not online.",
        PlacementError::NotPermitted => "The bounty permit was refused.",
        PlacementError::SelfTarget => "You cannot place a bounty on yourself.",
        PlacementError::NotWanted => "The target is not wanted.",
        PlacementError::InvalidAmount => "The bounty amount must be positive.",
        PlacementError::NotEnoughMoney => "You do not have enough money.",
        PlacementError::Ledger(_) => "The bounty ledger refused the pledge.",
    }
}

impl Command for SetBountyMoney {
    fn command_names(&self) -> Vec<String> {
        names(&["set_bounty_money"])
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
                "Usage: set_bounty_money <character> <amount>",
            );
            return Ok(());
        }
        let Some(amount) = try_parse::<i32>(&args[1]) else {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Invalid bounty amount: {}", args[1]),
            );
            return Ok(());
        };
        if amount <= 0 {
            CommandManager::send_error_text(self, out, "The bounty amount must be positive.");
            return Ok(());
        }

        let server = ctx.server()?;
        let gm = ctx.player()?;
        let Some(target) = retail::online(server, name) else {
            let message = if retail::character_id(server, name).is_some() {
                format!("Character {name} is not online.")
            } else {
                format!("Character {name} was not found.")
            };
            CommandManager::send_error_text(self, out, &message);
            return Ok(());
        };
        if target.character_id == gm.character_id {
            CommandManager::send_error_text(self, out, "You cannot place a bounty on yourself.");
            return Ok(());
        }

        let target_name = name_of(&target);
        let bounties = &server.systems.social.bounties;
        // The gameplay path first records a permit from `CSRequestSetBountyMoney`; the GM
        // command issues its own and then places the pledge immediately.
        bounties.permit(gm.character_id, target.character_id);
        match bounties.place(server, &gm, target.character_id, amount) {
            Ok(total) => CommandManager::send_normal_text(
                self,
                out,
                &format!("Placed a {amount} copper bounty on {target_name} (total {total})."),
            ),
            Err(error) => {
                CommandManager::send_error_text(self, out, placement_error(error));
            }
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character> <amount>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Places a bounty of <amount> copper on an online wanted character, paid from the GM's\n\
         purse through the normal bounty ledger. The retail client has the name only; the\n\
         positional arguments and GM funding are assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::GameData;
    use crate::systems::social::crime::{self, buff};

    fn data() -> GameData {
        let mut data = GameData::empty();
        let mut template = BuffTemplate::simple(buff::WANTED, BuffKind::Bad);
        template.duration = 3_600_000;
        data.skills.buffs.insert(buff::WANTED, template);
        data.systems
            .skills
            .buff_tags
            .insert(buff::WANTED, vec![buff::TAG_WANTED]);
        data.systems
            .skills
            .tagged_buffs
            .insert(buff::TAG_WANTED, vec![buff::WANTED]);
        data
    }

    fn make_wanted(server: &crate::server::GameServer, player: &crate::world::PlayerRef) {
        let mut c = player.data();
        crime::add_own_buff(
            server,
            &mut c,
            buff::WANTED,
            3_600_000,
            crate::world::now_ms(),
        );
    }

    #[tokio::test]
    async fn places_a_gm_funded_pledge_and_updates_the_total() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Crook", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().money = 1_000;
        let mut crook = harness::pc(&server, 2, "Crook", 110.0, 100.0);
        make_wanted(&server, &crook.p);
        crook.frames();

        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Crook 100").await;
        assert_eq!(
            out,
            [harness::normal(
                "set_bounty_money",
                "Placed a 100 copper bounty on Crook (total 100)."
            )]
        );
        assert_eq!(gm.p.data().money, 900);
        assert_eq!(server.systems.social.bounties.active_total(2), 100);
        let frames = crook.packets();
        assert!(frames
            .iter()
            .any(|(op, _)| *op == crate::opcodes::sc::SCUnitBountyMoneyPacket));

        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Crook 50").await;
        assert_eq!(
            out,
            [harness::normal(
                "set_bounty_money",
                "Placed a 50 copper bounty on Crook (total 150)."
            )]
        );
        assert_eq!(gm.p.data().money, 850);
        assert_eq!(server.systems.social.bounties.active_total(2), 150);
    }

    #[tokio::test]
    async fn refuses_unwanted_offline_and_broke_targets() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Plain", 1002);
        server.names.add_character(3, "Offline", 1003);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().money = 10;
        let plain = harness::pc(&server, 2, "Plain", 110.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Plain 100").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "The target is not wanted."
            )]
        );
        assert_eq!(gm.p.data().money, 10);

        make_wanted(&server, &plain.p);
        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Plain 100").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "You do not have enough money."
            )]
        );
        assert_eq!(server.systems.social.bounties.active_total(2), 0);

        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Offline 10").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "Character Offline is not online."
            )]
        );
    }

    #[tokio::test]
    async fn argument_and_target_errors() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money").await;
        assert!(out[0].starts_with(
            "Help for |cFFFFFFFF/set_bounty_money|r |cFFEEEEAA<character> <amount>|r"
        ));
        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Gm").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "Usage: set_bounty_money <character> <amount>"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Gm abc").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "Invalid bounty amount: abc"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Gm 0").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "The bounty amount must be positive."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Gm 10").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "You cannot place a bounty on yourself."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "set_bounty_money Nobody 10").await;
        assert_eq!(
            out,
            [harness::error(
                "set_bounty_money",
                "Character Nobody was not found."
            )]
        );
    }
}
