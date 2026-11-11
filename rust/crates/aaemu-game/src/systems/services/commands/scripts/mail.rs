//! `/mail admin` (Rust-only): send an `ADMIN` (5) system mail to a character by name, online or
//! offline, with optional copper and item stacks.
//!
//! The C# server had no such command; the audit's WP-I4 asked for it because `ADMIN` is one of
//! the mail types the 1.2 client lists (`mail/common.alb`) and there is no other way to produce
//! an admin letter. It goes through the existing mail store (`mail::send_system_mail`), so the
//! receiver gets the same `SCGotMailPacket` / list / attachment behaviour as any system mail.

use super::prelude::*;
use crate::systems::economy::inventory::ItemTxn;
use crate::systems::economy::mail_system;

/// `Mail`.
pub struct MailCommand;

impl MailCommand {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for MailCommand {
    fn command_names(&self) -> Vec<String> {
        names(&["mail"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let Some(action) = args.first() else {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        };
        let action = action.to_lowercase();
        if action != "admin" {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        }
        // admin <receiver> <title> <text> [money] [[itemId count] ...]
        if args.len() < 4 {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        }
        let receiver = &args[1];
        let title = &args[2];
        let text = &args[3];
        let rest = &args[4..];
        // Trailings are money plus item pairs (odd) or only item pairs (even). This keeps the
        // common `/mail admin Bob Title Body` form working with no trailing arguments.
        let (money, item_args) = if rest.len() % 2 == 1 {
            let Some(money) = try_parse::<i32>(&rest[0]) else {
                CommandManager::send_error_text(
                    self,
                    out,
                    &format!("Invalid money amount: {}", rest[0]),
                );
                return Ok(());
            };
            (money, &rest[1..])
        } else {
            (0, rest)
        };
        let mut items = Vec::with_capacity(item_args.len() / 2);
        for pair in item_args.as_chunks::<2>().0 {
            let Some(template_id) = try_parse::<u32>(&pair[0]) else {
                CommandManager::send_error_text(
                    self,
                    out,
                    &format!("Invalid item id: {}", pair[0]),
                );
                return Ok(());
            };
            let Some(count) = try_parse::<i32>(&pair[1]) else {
                CommandManager::send_error_text(
                    self,
                    out,
                    &format!("Invalid item count: {}", pair[1]),
                );
                return Ok(());
            };
            items.push((template_id, count));
        }

        let server = ctx.server()?;
        let mut txn = ItemTxn::begin(server);
        match mail_system::send_admin_mail(&mut txn, receiver, title, text, money, &items) {
            Ok(()) => CommandManager::send_normal_text(
                self,
                out,
                &format!(
                    "Sent admin mail to {receiver} ({money} copper, {} item stack(s))",
                    items.len()
                ),
            ),
            Err(e) => CommandManager::send_error_text(self, out, &e.to_string()),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "admin <receiver> <title> <text> [money] [[itemId count] ...]".into()
    }

    fn get_command_help_text(&self) -> String {
        format!(
            "Sends a system (Admin type 5) mail to a character by name, online or offline.\n\
             [money] is copper; when it is present it is the first trailing argument and the\n\
             remaining arguments are item template id / count pairs (max 10 stacks).\n\
             Example: {}mail admin Bob Welcome \"Have a gift\" 250 8000 3",
            COMMAND_PREFIX
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::data::items::ItemTemplate;
    use crate::data::GameData;
    use crate::models::container::slot_type;
    use crate::opcodes::sc;
    use crate::server::GameServer;
    use crate::systems::economy::inventory::{Inv, ItemTxn};
    use crate::systems::economy::mail::{self, mail_type};
    use crate::systems::economy::mail_system::ADMIN_MAIL_SENDER;

    const ITEM: u32 = 8000;

    fn data() -> GameData {
        let mut data = GameData::empty();
        let mut t = ItemTemplate::plain(ITEM, 10);
        t.fixed_grade = -1;
        data.items.templates.insert(t.id, t);
        data
    }

    fn mail_for(server: &GameServer, receiver_id: u32) -> (i64, mail::BaseMail) {
        server
            .systems
            .economy
            .mail
            .state()
            .all
            .iter()
            .find(|(_, m)| m.header.receiver_id == receiver_id)
            .map(|(id, m)| (id, m.clone()))
            .expect("mail stored")
    }

    #[tokio::test]
    async fn admin_mail_delivers_money_and_one_attachment_to_an_online_player() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Bob", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let mut bob = harness::pc(&server, 2, "Bob", 101.0, 100.0);
        bob.p.data().money = 0;
        // The item manager must know the players' containers (production registers them at
        // character load); a mail container the manager cannot see cannot receive attachments.
        for p in [&gm.p, &bob.p] {
            for (_, c) in Inv::of(p).containers {
                server.items.add_container(c);
            }
        }

        let (handled, out) = harness::run(
            &server,
            &gm.p,
            "mail admin Bob Welcome \"Enjoy your gift\" 250 8000 3",
        )
        .await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "mail",
                "Sent admin mail to Bob (250 copper, 1 item stack(s))"
            )]
        );

        let (id, m) = mail_for(&server, 2);
        assert_eq!(m.mail_type, mail_type::ADMIN);
        assert_eq!(m.header.sender_id, 0);
        assert_eq!(m.header.sender_name, ADMIN_MAIL_SENDER);
        assert_eq!((m.header.receiver_id, m.receiver_name.as_str()), (2, "Bob"));
        assert_eq!(
            (m.title.as_str(), m.body.text.as_str()),
            ("Welcome", "Enjoy your gift")
        );
        assert_eq!((m.body.copper_coins, m.header.attachments), (250, 2));
        assert_eq!(m.body.attachments.len(), 1);
        let item_id = m.body.attachments[0];
        let item = server.items.item_by_id(item_id).expect("attachment item");
        assert_eq!(
            (item.template_id, item.count, item.owner_id, item.slot_type),
            (ITEM, 3, 2, slot_type::MAIL)
        );
        // The attachment exists exactly once, in Bob's mail container.
        let mail_box = Inv::of(&bob.p)
            .get(slot_type::MAIL)
            .expect("Bob's mail container");
        assert_eq!(
            mail_box
                .lock()
                .unwrap()
                .items
                .iter()
                .filter(|i| i.id == item_id)
                .count(),
            1
        );
        assert!(bob
            .packets()
            .iter()
            .any(|(op, _)| *op == sc::SCGotMailPacket));
        assert_eq!(bob.p.data().unread_mail.received, 1);

        // Taking the attachment moves the same item into the bag and credits the copper.
        let mut txn = ItemTxn::begin(&server);
        assert_eq!(
            crate::systems::economy::mail_character::get_attached(
                &mut txn, &bob.p, id, true, true, true, 0
            ),
            Ok(true)
        );
        drop(txn);
        assert_eq!(bob.p.data().money, 250);
        let bag = Inv::of(&bob.p).bag();
        let taken = bag
            .lock()
            .unwrap()
            .items
            .iter()
            .find(|i| i.id == item_id)
            .cloned()
            .expect("attachment in the bag");
        assert_eq!((taken.template_id, taken.count), (ITEM, 3));
        assert_eq!(
            mail_box
                .lock()
                .unwrap()
                .items
                .iter()
                .filter(|i| i.id == item_id)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn admin_mail_accepts_a_quoted_title_and_text() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(2, "Bob", 1002);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let bob = harness::pc(&server, 2, "Bob", 101.0, 100.0);
        for p in [&gm.p, &bob.p] {
            for (_, c) in Inv::of(p).containers {
                server.items.add_container(c);
            }
        }

        let (handled, out) = harness::run(
            &server,
            &gm.p,
            "mail admin Bob \"Audit title\" \"Audit body\" 100",
        )
        .await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "mail",
                "Sent admin mail to Bob (100 copper, 0 item stack(s))"
            )]
        );
        let (_, m) = mail_for(&server, 2);
        assert_eq!(
            (m.title.as_str(), m.body.text.as_str()),
            ("Audit title", "Audit body")
        );
        // `header.attachments` counts the copper as one entry (as in the first test).
        assert_eq!(m.header.attachments, 1);
    }

    #[tokio::test]
    async fn admin_mail_to_an_offline_character_is_stored_and_delivered_later() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        server.names.add_character(7, "Offline", 1007);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

        // No money: the two trailing arguments are an item id / count pair.
        let (handled, out) = harness::run(
            &server,
            &gm.p,
            "mail admin Offline Report \"Come back soon\" 8000 2",
        )
        .await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "mail",
                "Sent admin mail to Offline (0 copper, 1 item stack(s))"
            )]
        );
        let (id, m) = mail_for(&server, 7);
        assert!(!m.is_delivered);
        assert_eq!(m.header.receiver_id, 7);
        let item_id = m.body.attachments[0];
        let item = server.items.item_by_id(item_id).expect("attachment item");
        assert_eq!(
            (item.owner_id, item.slot_type, item.count),
            (7, slot_type::MAIL, 2)
        );
        assert!(server.world.character_by_id(7).is_none());

        // A later login / delivery tick finds the stored mail and sends SCGotMailPacket.
        let mut client = crate::test_support::TestClient::new(&server, 7, 1007);
        let p = crate::test_support::spawn_player(
            &server,
            &client,
            crate::test_support::test_character(7, "Offline", 100.0, 100.0),
            0x107,
        );
        client.conn.active_char = Some(p.clone());
        client.drain();
        let mut txn = ItemTxn::begin(&server);
        mail::check_all_mail_timings(&mut txn);
        drop(txn);
        assert!(client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCGotMailPacket));
        assert_eq!(p.data().unread_mail.received, 1);
        assert!(server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .is_some_and(|m| m.is_delivered));
    }

    #[tokio::test]
    async fn admin_mail_rejects_unknown_receivers_and_bad_items_without_side_effects() {
        let server = harness::server_with(data());
        server.names.add_character(1, "Gm", 1001);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let next = server.ids.item.get_next_id().unwrap();
        server.ids.item.release_id(next);

        let (_, out) = harness::run(&server, &gm.p, "mail admin Nobody T B").await;
        assert_eq!(out, [harness::error("mail", "Character not found: Nobody")]);
        let (_, out) = harness::run(&server, &gm.p, "mail admin Gm T B -5").await;
        assert_eq!(out, [harness::error("mail", "Money must not be negative")]);
        let (_, out) = harness::run(&server, &gm.p, "mail admin Gm T B 9999 1").await;
        assert_eq!(
            out,
            [harness::error("mail", "Item template 9999 does not exist")]
        );
        let (_, out) = harness::run(&server, &gm.p, "mail admin Gm T B 8000 0").await;
        assert_eq!(
            out,
            [harness::error(
                "mail",
                "Item 8000 needs a count of at least 1"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "mail admin Gm T B notanumber").await;
        assert_eq!(
            out,
            [harness::error("mail", "Invalid money amount: notanumber")]
        );
        // Nothing was stored and no item id was consumed by the failed attempts.
        assert!(server.systems.economy.mail.state().all.is_empty());
        assert_eq!(server.ids.item.get_next_id().unwrap(), next);
    }
}
