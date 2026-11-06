//! Retail `/delay_tax_due_date` (WP-SL6b): postpone the tax due date of every taxable house
//! of a character (online or offline).
//!
//! The 1.2 client's GM console builds `DelayTaxDueDate(%s)` (character name only, evidence
//! `siege-social-2.md` section 1.5), so the number of days is an assumed extension:
//! `delay_tax_due_date <character> [days]`, default one tax week (7 days).

use super::prelude::*;
use super::retail;
use crate::world::HouseRef;

const DEFAULT_DAYS: i32 = 7;
const MAX_DAYS: i32 = 3650;
const DAY: i64 = 24 * 3600;

/// `delay_tax_due_date`.
pub struct DelayTaxDueDate;

impl DelayTaxDueDate {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

/// Ordinary tax properties only: forts and claimed lodestones follow the dominion timers
/// (`HousingManager.UpdateTaxInfo` skips them), so a tax delay must not touch them.
fn taxable(server: &GameServer, house: &HouseRef) -> bool {
    let is_infrastructure = crate::systems::housing::manager::template_of(server, house)
        .is_some_and(|template| template.guard_tower_setting_id != 0)
        || crate::systems::housing::dominion::fort::is_fort(server, house);
    !is_infrastructure
}

impl Command for DelayTaxDueDate {
    fn command_names(&self) -> Vec<String> {
        names(&["delay_tax_due_date"])
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
        if args.len() > 2 {
            CommandManager::send_error_text(
                self,
                out,
                "Usage: delay_tax_due_date <character> [days]",
            );
            return Ok(());
        }
        let days = match args.get(1) {
            None => DEFAULT_DAYS,
            Some(text) => match try_parse::<i32>(text) {
                Some(days) => days,
                None => {
                    CommandManager::send_error_text(
                        self,
                        out,
                        &format!("Invalid number of days: {text}"),
                    );
                    return Ok(());
                }
            },
        };
        if !(1..=MAX_DAYS).contains(&days) {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Days must be between 1 and {MAX_DAYS}."),
            );
            return Ok(());
        }

        let server = ctx.server()?;
        let Some(owner_id) = retail::character_id(server, name) else {
            CommandManager::send_error_text(self, out, &format!("Character {name} was not found."));
            return Ok(());
        };
        let houses: Vec<HouseRef> = server
            .systems
            .housing
            .manager
            .by_character(owner_id)
            .into_iter()
            .filter(|house| taxable(server, house))
            .collect();
        if houses.is_empty() {
            CommandManager::send_error_text(
                self,
                out,
                &format!(
                    "Character {} owns no taxable houses.",
                    retail::character_name(server, owner_id)
                ),
            );
            return Ok(());
        }

        let extend = i64::from(days) * DAY;
        for house in &houses {
            // One house lock at a time; the save manager writes the new date because
            // `is_dirty` is the same marker `PayWeeklyTax` sets for a gameplay extension.
            let mut data = house.data();
            data.protection_end_date = data.protection_end_date.saturating_add(extend);
            data.is_dirty = true;
        }
        CommandManager::send_normal_text(
            self,
            out,
            &format!(
                "Delayed the tax due date of {} house(s) of {} by {days} day(s).",
                houses.len(),
                retail::character_name(server, owner_id)
            ),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character> [days]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Postpones the tax due date of every taxable house owned by an online or offline\n\
         character by <days> (default 7, the weekly tax interval). The retail console sends\n\
         only the name (DelayTaxDueDate), so the day count is an assumed optional argument."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{harness, test_house};
    use crate::server::GameServer;
    use crate::systems::housing::manager as housing;
    use crate::test_support::TEST_ZONE;
    use crate::world::HouseRef;

    fn offline_house(server: &GameServer, owner_id: u32, account_id: u32) -> HouseRef {
        let world = server.world.world(0).unwrap();
        let house =
            housing::create(server, test_house::fixture::DESIGN, 101, &world, 0, 0).unwrap();
        {
            let mut h = house.data();
            h.id = 71;
            h.account_id = account_id;
            h.unit.owner_id = owner_id;
            h.unit.name = "Cottage".into();
            h.unit.transform.set_xyz(200.0, 200.0, 50.0);
            h.unit.transform.zone_id = TEST_ZONE;
            h.place_date = housing::now();
            h.protection_end_date = h.place_date + 14 * 24 * 3600;
        }
        assert!(server.systems.housing.manager.insert(&house));
        house
    }

    #[tokio::test]
    async fn delays_an_offline_owners_house_and_marks_it_for_save() {
        let server = harness::server_with(test_house::fixture::data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let house = offline_house(&server, 2, 1002);
        let before = house.data().protection_end_date;
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (handled, out) = harness::run(&server, &gm.p, "delay_tax_due_date Owner 10").await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "delay_tax_due_date",
                "Delayed the tax due date of 1 house(s) of Owner by 10 day(s)."
            )]
        );
        let after = house.data().protection_end_date;
        assert_eq!(after, before + 10 * 24 * 3600);
        assert!(house.data().is_dirty);
        let row = housing::save_row(&house).expect("dirty house is saved");
        assert_eq!(row.protected_until, after);
    }

    #[tokio::test]
    async fn default_and_argument_errors() {
        let server = harness::server_with(test_house::fixture::data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let house = offline_house(&server, 2, 1002);
        let before = house.data().protection_end_date;
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date Owner").await;
        assert_eq!(
            out,
            [harness::normal(
                "delay_tax_due_date",
                "Delayed the tax due date of 1 house(s) of Owner by 7 day(s)."
            )]
        );
        assert_eq!(house.data().protection_end_date, before + 7 * 24 * 3600);

        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date Owner abc").await;
        assert_eq!(
            out,
            [harness::error(
                "delay_tax_due_date",
                "Invalid number of days: abc"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date Owner 0").await;
        assert_eq!(
            out,
            [harness::error(
                "delay_tax_due_date",
                "Days must be between 1 and 3650."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date Nobody").await;
        assert_eq!(
            out,
            [harness::error(
                "delay_tax_due_date",
                "Character Nobody was not found."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date Owner 1 2").await;
        assert_eq!(
            out,
            [harness::error(
                "delay_tax_due_date",
                "Usage: delay_tax_due_date <character> [days]"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date").await;
        assert!(out[0].starts_with(
            "Help for |cFFFFFFFF/delay_tax_due_date|r |cFFEEEEAA<character> [days]|r"
        ));
    }

    #[tokio::test]
    async fn an_owner_without_houses_is_refused() {
        let server = harness::server_with(test_house::fixture::data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let (_, out) = harness::run(&server, &gm.p, "delay_tax_due_date Owner").await;
        assert_eq!(
            out,
            [harness::error(
                "delay_tax_due_date",
                "Character Owner owns no taxable houses."
            )]
        );
    }
}
