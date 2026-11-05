//! `/aapoint`: Rust-only GM command (WP-AA1). The 1.2 client only shows the two AA-point
//! balances and spends them from the cash shop, and the data has no player-reachable
//! acquisition path (`quest_act_supply_aa_points` has 0 rows), so a debug source is needed.
//!
//! Usage: `/aapoint <add|set|show> [bank] <amount>`. `add` accepts a negative amount to take
//! points (refused when the purse is short); `set` is absolute; `bank` selects the bank purse.

use super::prelude::*;
use crate::models::item_task::task_type;
use crate::systems::economy::inventory::{self, Out};

pub struct AaPoint;

impl AaPoint {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for AaPoint {
    fn command_names(&self) -> Vec<String> {
        names(&["aapoint", "aa_point"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.is_empty() {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        }
        let server = ctx.server()?;
        let character = ctx.player()?;

        let action = args[0].to_ascii_lowercase();
        let mut at = 1usize;
        let bank = args
            .get(at)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("bank"));
        if bank {
            at += 1;
        }
        if action == "show" {
            let (aa_point, bank_aa_point) = {
                let c = character.data();
                (c.aa_point, c.bank_aa_point)
            };
            CommandManager::send_normal_text(
                self,
                out,
                &format!("AA points: {aa_point} (bank: {bank_aa_point})"),
            );
            return Ok(());
        }
        if action != "add" && action != "set" {
            CommandManager::send_error_text(
                self,
                out,
                "Usage: /aapoint <add|set|show> [bank] <amount>",
            );
            return Ok(());
        }
        let Some(amount) = args.get(at).and_then(|arg| try_parse::<i32>(arg)) else {
            CommandManager::send_error_text(self, out, "No valid amount provided ...");
            return Ok(());
        };
        let mut wallet_out = Out::Direct(server);
        let changed = if action == "add" {
            if amount < 0 {
                match amount.checked_neg() {
                    Some(remove) if bank => inventory::try_take_bank_aa_point(
                        &mut wallet_out,
                        &character,
                        remove,
                        task_type::INVALID,
                    ),
                    Some(remove) => inventory::try_take_aa_point(
                        &mut wallet_out,
                        &character,
                        remove,
                        task_type::INVALID,
                    ),
                    None => false,
                }
            } else if bank {
                inventory::add_bank_aa_point(
                    &mut wallet_out,
                    &character,
                    amount,
                    task_type::INVALID,
                )
            } else {
                inventory::add_aa_point(&mut wallet_out, &character, amount, task_type::INVALID)
            }
        } else if amount < 0 {
            CommandManager::send_error_text(self, out, "AA points cannot be negative.");
            return Ok(());
        } else if bank {
            inventory::set_bank_aa_point(&mut wallet_out, &character, amount, task_type::INVALID)
        } else {
            inventory::set_aa_point(&mut wallet_out, &character, amount, task_type::INVALID)
        };
        if !changed {
            CommandManager::send_error_text(self, out, "AA point change failed.");
            return Ok(());
        }
        let (aa_point, bank_aa_point) = {
            let c = character.data();
            (c.aa_point, c.bank_aa_point)
        };
        CommandManager::send_normal_text(
            self,
            out,
            &format!("AA points: {aa_point} (bank: {bank_aa_point})"),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<add|set|show> [bank] <amount>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Shows or changes the character's AA points; add accepts a negative amount.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcodes::sc;

    use super::super::harness;

    /// `SCItemTaskSuccessPacket(ItemTaskType.Invalid, [task], [])`.
    fn task_body(task: crate::models::item_task::ItemTask) -> Vec<u8> {
        let mut s = aaemu_commons::PacketStream::new();
        s.write_u8(task_type::INVALID).write_u8(1);
        aaemu_commons::Marshal::write(&task, &mut s);
        s.write_u8(0).write_u32(0);
        s.into_bytes()
    }

    fn aa_task_body(delta: i32) -> Vec<u8> {
        task_body(crate::models::item_task::ItemTask::AaPointUpdate(delta))
    }

    fn bank_aa_task_body(delta: i32) -> Vec<u8> {
        task_body(crate::models::item_task::ItemTask::ChangeBankAaPoint(delta))
    }

    #[tokio::test]
    async fn grant_show_and_take() {
        let server = harness::server();
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.p.data().aa_point = 0;
        gm.p.data().bank_aa_point = 0;

        let (handled, out) = harness::run(&server, &gm.p, "aapoint add 300").await;
        assert!(handled);
        assert_eq!(
            out,
            vec![harness::normal("aapoint", "AA points: 300 (bank: 0)")]
        );
        assert_eq!(gm.p.data().aa_point, 300);
        assert_eq!(
            gm.packets(),
            vec![(sc::SCItemTaskSuccessPacket, aa_task_body(300))]
        );

        let (_, out) = harness::run(&server, &gm.p, "aapoint add bank 40").await;
        assert_eq!(
            out,
            vec![harness::normal("aapoint", "AA points: 300 (bank: 40)")]
        );
        assert_eq!(
            gm.packets(),
            vec![(sc::SCItemTaskSuccessPacket, bank_aa_task_body(40))]
        );

        let (_, out) = harness::run(&server, &gm.p, "aapoint show").await;
        assert_eq!(
            out,
            vec![harness::normal("aapoint", "AA points: 300 (bank: 40)")]
        );

        let (_, out) = harness::run(&server, &gm.p, "aapoint add -100").await;
        assert_eq!(
            out,
            vec![harness::normal("aapoint", "AA points: 200 (bank: 40)")]
        );
        assert_eq!(gm.p.data().aa_point, 200);
        assert_eq!(
            gm.packets(),
            vec![(sc::SCItemTaskSuccessPacket, aa_task_body(-100))]
        );

        // Taking more than the purse holds is refused and changes nothing.
        let (_, out) = harness::run(&server, &gm.p, "aapoint add -500").await;
        assert_eq!(
            out,
            vec![harness::error("aapoint", "AA point change failed.")]
        );
        assert_eq!(gm.p.data().aa_point, 200);
        let frames = gm.packets();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, sc::SCErrorMsgPacket);

        let (_, out) = harness::run(&server, &gm.p, "aapoint set bank 7").await;
        assert_eq!(
            out,
            vec![harness::normal("aapoint", "AA points: 200 (bank: 7)")]
        );
        assert_eq!(gm.p.data().bank_aa_point, 7);
        assert_eq!(
            gm.packets(),
            vec![(sc::SCItemTaskSuccessPacket, bank_aa_task_body(-33))]
        );

        let (_, out) = harness::run(&server, &gm.p, "aapoint").await;
        assert_eq!(
            out,
            vec![
                "Help for |cFFFFFFFF/aapoint|r |cFFEEEEAA<add|set|show> [bank] <amount>|r\n\
                 |cFF888888Shows or changes the character's AA points; add accepts a negative amount.|r"
            ]
        );
        let (_, out) = harness::run(&server, &gm.p, "aapoint bogus 1").await;
        assert_eq!(
            out,
            vec![harness::error(
                "aapoint",
                "Usage: /aapoint <add|set|show> [bank] <amount>"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "aapoint set -1").await;
        assert_eq!(
            out,
            vec![harness::error("aapoint", "AA points cannot be negative.")]
        );
    }
}
