//! Retail `/resend_house_tax_mail` (WP-SL6b): restore the current tax bill for taxable houses
//! of a character (online or offline).
//!
//! The 1.2 client's GM console builds `ResendHouseTaxMail(%s)` (character name only, evidence
//! `siege-social-2.md` section 1.5). Reconcile through the same schedule and bill handling
//! as the housing tax timer.

use super::prelude::*;
use super::retail;
use crate::world::HouseRef;

/// `resend_house_tax_mail`.
pub struct ResendHouseTaxMail;

impl ResendHouseTaxMail {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

fn taxable(server: &GameServer, house: &HouseRef) -> bool {
    let is_infrastructure = crate::systems::housing::manager::template_of(server, house)
        .is_some_and(|template| template.guard_tower_setting_id != 0)
        || crate::systems::housing::dominion::fort::is_fort(server, house);
    !is_infrastructure
}

impl Command for ResendHouseTaxMail {
    fn command_names(&self) -> Vec<String> {
        names(&["resend_house_tax_mail"])
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
            CommandManager::send_error_text(self, out, "Usage: resend_house_tax_mail <character>");
            return Ok(());
        }

        let server = ctx.server()?;
        let Some(owner_id) = retail::character_id(server, name) else {
            CommandManager::send_error_text(self, out, &format!("Character {name} was not found."));
            return Ok(());
        };
        let owner_name = retail::character_name(server, owner_id);
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
                &format!("Character {owner_name} owns no taxable houses."),
            );
            return Ok(());
        }

        let bills = houses
            .iter()
            .filter(|house| crate::systems::housing::manager::update_tax_info(server, house))
            .count();
        if bills == 0 {
            CommandManager::send_error_text(
                self,
                out,
                &format!("No current tax bills to refresh for {owner_name}."),
            );
            return Ok(());
        }
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Refreshed {bills} current tax bill(s) for {owner_name}."),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Refreshes the due tax bills of an online or offline character. Issues a bill if\n\
         one is missing and updates the existing bill if it is already in the mailbox."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{harness, test_house};
    use crate::server::GameServer;
    use crate::systems::economy::mail::mail_type;
    use crate::systems::housing::manager as housing;
    use crate::test_support::TEST_ZONE;
    use crate::world::HouseRef;

    fn offline_house(server: &GameServer, owner_id: u32, account_id: u32) -> HouseRef {
        let world = server.world.world(0).unwrap();
        let house =
            housing::create(server, test_house::fixture::DESIGN, 101, &world, 0, 0).unwrap();
        {
            let mut h = house.data();
            h.id = 72;
            h.account_id = account_id;
            h.unit.owner_id = owner_id;
            h.unit.name = "Cottage".into();
            h.unit.transform.set_xyz(210.0, 210.0, 50.0);
            h.unit.transform.zone_id = TEST_ZONE;
            h.place_date = housing::now();
            h.protection_end_date = h.place_date + 14 * 24 * 3600;
        }
        assert!(server.systems.housing.manager.insert(&house));
        house
    }

    #[tokio::test]
    async fn resends_a_tax_mail_to_an_offline_owner() {
        let server = harness::server_with(test_house::fixture::data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let house = offline_house(&server, 2, 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "resend_house_tax_mail Owner").await;
        assert_eq!(
            out,
            [harness::error(
                "resend_house_tax_mail",
                "No current tax bills to refresh for Owner."
            )]
        );
        assert!(server
            .systems
            .economy
            .mail
            .get_my_house_mails(72)
            .is_empty());

        house.data().protection_end_date = housing::now() + 4 * 24 * 3600;
        let (handled, out) = harness::run(&server, &gm.p, "resend_house_tax_mail Owner").await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "resend_house_tax_mail",
                "Refreshed 1 current tax bill(s) for Owner."
            )]
        );
        harness::run(&server, &gm.p, "resend_house_tax_mail Owner").await;
        let mails: Vec<_> = server
            .systems
            .economy
            .mail
            .state()
            .all
            .iter()
            .map(|(_, m)| {
                (
                    m.mail_type,
                    m.header.receiver_id,
                    m.header.sender_name.clone(),
                    m.receiver_name.clone(),
                )
            })
            .collect();
        assert_eq!(
            mails,
            vec![(
                mail_type::BILLING,
                2,
                ".houseTax".to_string(),
                "Owner".into()
            )]
        );
    }

    #[tokio::test]
    async fn argument_and_target_errors() {
        let server = harness::server_with(test_house::fixture::data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Owner", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, "resend_house_tax_mail").await;
        assert!(out[0]
            .starts_with("Help for |cFFFFFFFF/resend_house_tax_mail|r |cFFEEEEAA<character>|r"));
        let (_, out) = harness::run(&server, &gm.p, "resend_house_tax_mail Nobody").await;
        assert_eq!(
            out,
            [harness::error(
                "resend_house_tax_mail",
                "Character Nobody was not found."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "resend_house_tax_mail Owner").await;
        assert_eq!(
            out,
            [harness::error(
                "resend_house_tax_mail",
                "Character Owner owns no taxable houses."
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "resend_house_tax_mail Owner extra").await;
        assert_eq!(
            out,
            [harness::error(
                "resend_house_tax_mail",
                "Usage: resend_house_tax_mail <character>"
            )]
        );
    }
}
