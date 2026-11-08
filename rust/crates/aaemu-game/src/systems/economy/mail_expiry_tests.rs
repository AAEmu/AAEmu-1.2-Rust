//! Mail lifecycle regressions. No wall-clock sleeps, fake manager, or wire round trips.
//! The windows are the 1.2 client's `mail_guide` (`ui_texts` 4465): normal mail 5 days read /
//! 30 days unread, system mail 5 days read / 90 days unread; mail with unclaimed attachments
//! keeps the unread deadline. The commercial mailbox (`ui_texts` 4464) has its own rules.
//! Native 1.2 evidence: decomp/x2game12.c builders 393CCCD0 (money), 393CD1D0
//! (delete), serializer 397BA450 (return), header/body at 1995740-42/1995902-09,
//! and ui/scripts/x2ui/mailbox/mail/read_mail.lua:129-139
//! (receipt vs send date).

use super::*;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::systems::economy::mail_character;
use crate::systems::economy::mail_packets::CSTakeAttachmentSequentially;
use crate::systems::economy::tests::{give, world, World, SINGLE};

fn normal_unread_secs(server: &GameServer) -> i64 {
    server.systems.economy.mail.retention().unread_secs
}

fn normal_read_secs(server: &GameServer) -> i64 {
    server.systems.economy.mail.retention().read_secs
}

fn system_unread_secs(server: &GameServer) -> i64 {
    server.systems.economy.mail.retention().system_unread_secs
}

fn system_read_secs(server: &GameServer) -> i64 {
    server.systems.economy.mail.retention().system_read_secs
}

fn bob_mail_ids(server: &GameServer) -> Vec<i64> {
    server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(_, m)| m.header.receiver_id == 2)
        .map(|(k, _)| k)
        .collect()
}

fn mail_world() -> World {
    let w = world();
    w.server.names.add_character(1, "Alice", 1001);
    w.server.names.add_character(2, "Bob", 1002);
    for p in [&w.pa, &w.pb] {
        for (_, container) in Inv::of(p).containers {
            w.server.items.add_container(container);
        }
    }
    w
}

fn send_player_mail(w: &World, kind: u8, count: i32, copper: i32) -> (i64, Vec<u64>) {
    w.pa.data().money = 10_000;
    let ids: Vec<_> = (0..count)
        .map(|slot| give(w, &w.pa, slot_type::INVENTORY, SINGLE, 1, slot))
        .collect();
    let slots: Vec<_> = (0..count)
        .map(|slot| (slot_type::INVENTORY, slot as u8))
        .collect();
    let mut txn = ItemTxn::begin(&w.server);
    assert_eq!(
        mail_character::send_mail_to_player(
            &mut txn, &w.pa, kind, "Bob", "expiry", "original", 0, copper, 0, 0, 0, &slots,
        ),
        mail_result::SUCCESS
    );
    let id = w.server.systems.economy.mail.state().all.keys()[0];
    (id, ids)
}

fn tick(server: &GameServer, time: UnixTime) {
    let mut txn = ItemTxn::begin(server);
    check_all_mail_timings_at(&mut txn, time);
}

fn frame(opcode: u16, id: i64) -> Vec<u8> {
    let mut frame = vec![0xdd, 1, 0, 0];
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&id.to_le_bytes());
    frame
}

#[tokio::test]
async fn mail_retention_defaults_are_the_client_windows() {
    let r = MailRetention::default();
    assert_eq!(
        (
            r.read_secs,
            r.unread_secs,
            r.system_read_secs,
            r.system_unread_secs
        ),
        (
            5 * SECONDS_PER_DAY,
            30 * SECONDS_PER_DAY,
            5 * SECONDS_PER_DAY,
            90 * SECONDS_PER_DAY
        )
    );
    assert!(!r.delete_system_mail_with_attachments);
    assert_eq!(r.grace_start_unix, 0);
    let cfg = crate::systems::economy::config::MailConfig::default();
    assert_eq!(MailRetention::from_config(&cfg), r);
    let configured = crate::systems::economy::config::MailConfig {
        delete_system_mail_with_attachments: true,
        retention_grace_start_unix: 1_700_000_000,
        ..cfg.clone()
    };
    let configured = MailRetention::from_config(&configured);
    assert!(configured.delete_system_mail_with_attachments);
    assert_eq!(configured.grace_start_unix, 1_700_000_000);
    // Negative input clamps to 0 instead of extending the window.
    let cfg = crate::systems::economy::config::MailConfig {
        read_retention_days: -1,
        ..cfg
    };
    assert_eq!(MailRetention::from_config(&cfg).read_secs, 0);
}

#[tokio::test]
async fn normal_unread_mail_is_kept_thirty_days_not_fourteen() {
    let mut w = mail_world();
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 0, 0);
    let recv = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(id)
        .unwrap()
        .body
        .recv_date;
    w.a.drain();
    w.b.drain();
    assert_eq!(normal_unread_secs(&w.server), 30 * SECONDS_PER_DAY);
    tick(&w.server, recv + 14 * SECONDS_PER_DAY);
    tick(&w.server, recv + normal_unread_secs(&w.server) - 1);
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_some(),
        "normal unread mail survives 14 days and the last second of its 30-day window"
    );
    tick(&w.server, recv + normal_unread_secs(&w.server));
    let returned = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert!(returned.header.returned);
    assert_eq!(
        (returned.header.receiver_id, returned.header.sender_id),
        (1, 2)
    );
}

#[tokio::test]
async fn normal_read_mail_expires_five_days_after_reading() {
    let mut w = mail_world();
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 0, 0);
    {
        let mut txn = ItemTxn::begin(&w.server);
        mail_character::read_mail(&mut txn, &w.pb, false, id);
    }
    let opened = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert_eq!(opened.header.status, mail_status::READ);
    assert!(opened.open_date > 0);
    w.a.drain();
    w.b.drain();
    tick(
        &w.server,
        opened.open_date + normal_read_secs(&w.server) - 1,
    );
    assert!(w.server.systems.economy.mail.get_mail_by_id(id).is_some());
    tick(&w.server, opened.open_date + normal_read_secs(&w.server));
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_none(),
        "read mail is deleted after the client's 5 days, not after 14"
    );
    assert_eq!(w.b.drain_ops(), vec![sc::SCMailDeletedPacket]);
}

#[tokio::test]
async fn read_mail_with_unclaimed_attachments_is_returned_at_the_unread_deadline() {
    let mut w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 1, 0);
    {
        let mut txn = ItemTxn::begin(&w.server);
        mail_character::read_mail(&mut txn, &w.pb, false, id);
    }
    let opened = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert_eq!(opened.header.status, mail_status::READ);
    w.a.drain();
    w.b.drain();
    tick(
        &w.server,
        opened.open_date + normal_read_secs(&w.server) + 1,
    );
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_some(),
        "an unclaimed attachment keeps the unread deadline (mail_guide)"
    );
    // At the unread deadline the read player letter goes back to its sender; its attachment
    // moves exactly once and is never destroyed.
    tick(
        &w.server,
        opened.body.recv_date + normal_unread_secs(&w.server),
    );
    let returned = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert!(returned.header.returned);
    assert_eq!(
        (returned.header.receiver_id, returned.header.sender_id),
        (1, 2)
    );
    assert_eq!(returned.body.attachments, ids);
    let item = w.server.items.item_by_id(ids[0]).unwrap();
    assert_eq!((item.owner_id, item.slot_type), (1, slot_type::MAIL));
    // A second tick must not return it again or duplicate the item.
    tick(
        &w.server,
        opened.body.recv_date + normal_unread_secs(&w.server) + 1,
    );
    let again = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert_eq!(again, returned);
    assert_eq!(w.server.items.item_by_id(ids[0]).unwrap().owner_id, 1);
    assert_eq!(
        w.server
            .items
            .removed_items()
            .iter()
            .filter(|&&item| item == ids[0])
            .count(),
        0
    );
}

#[tokio::test]
async fn system_mail_has_ninety_day_unread_and_five_day_read_windows() {
    let mut w = mail_world();
    let mut txn = ItemTxn::begin(&w.server);
    assert!(send_system_mail(
        &mut txn,
        mail_type::SYS_EXPRESS,
        ".questReward",
        2,
        "system",
        "",
        0,
        vec![]
    ));
    assert!(send_system_mail(
        &mut txn,
        mail_type::SYS_SELL_BACKPACK,
        ".sellBackpack",
        2,
        "read me",
        "",
        0,
        vec![]
    ));
    drop(txn);
    let ids = bob_mail_ids(&w.server);
    assert_eq!(ids.len(), 2);
    let (unread_id, read_id) = (ids[0], ids[1]);
    {
        let mut txn = ItemTxn::begin(&w.server);
        mail_character::read_mail(&mut txn, &w.pb, false, read_id);
    }
    let read_open = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(read_id)
        .unwrap()
        .open_date;
    let recv = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(unread_id)
        .unwrap()
        .body
        .recv_date;
    w.a.drain();
    w.b.drain();
    assert_eq!(system_unread_secs(&w.server), 90 * SECONDS_PER_DAY);
    assert_eq!(system_read_secs(&w.server), 5 * SECONDS_PER_DAY);
    // Read system mail uses the same 5-day read window as normal mail.
    tick(&w.server, read_open + system_read_secs(&w.server) - 1);
    assert!(w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(read_id)
        .is_some());
    tick(&w.server, read_open + system_read_secs(&w.server));
    assert!(w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(read_id)
        .is_none());
    // Unread system mail outlives the normal 30-day window.
    tick(&w.server, recv + normal_unread_secs(&w.server) + 1);
    assert!(w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(unread_id)
        .is_some());
    tick(&w.server, recv + system_unread_secs(&w.server));
    assert!(w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(unread_id)
        .is_none());
}

#[tokio::test]
async fn system_mail_with_unclaimed_attachments_is_kept_past_its_window() {
    let mut w = mail_world();
    let mut txn = ItemTxn::begin(&w.server);
    assert!(send_system_mail(
        &mut txn,
        mail_type::SYS_EXPRESS,
        ".questReward",
        2,
        "old with money",
        "",
        125,
        vec![]
    ));
    assert!(send_system_mail(
        &mut txn,
        mail_type::SYS_EXPRESS,
        ".questReward",
        2,
        "old empty",
        "",
        0,
        vec![]
    ));
    drop(txn);
    let ids = bob_mail_ids(&w.server);
    assert_eq!(ids.len(), 2);
    let (attached_id, empty_id) = (ids[0], ids[1]);
    // Both rows existed before the retention update and are older than the 90-day window.
    for id in [attached_id, empty_id] {
        w.server
            .systems
            .economy
            .mail
            .with_mail_mut(id, |m| m.body.recv_date = now() - 120 * SECONDS_PER_DAY);
    }
    let bob_money = w.pb.data().money;
    w.a.drain();
    w.b.drain();
    tick(&w.server, now());
    let attached = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(attached_id)
        .expect("system mail with unclaimed attachments is kept");
    assert_eq!(attached.body.copper_coins, 125);
    assert!(
        w.server
            .systems
            .economy
            .mail
            .get_mail_by_id(empty_id)
            .is_none(),
        "only attachment-free system mail follows the 90-day window"
    );
    // The receiver can still claim the kept escrow; that is what starts its read window.
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert_eq!(
            mail_character::get_attached(&mut txn, &w.pb, attached_id, true, true, true, 0),
            Ok(true)
        );
    }
    assert_eq!(w.pb.data().money, bob_money + 125);
    let claimed = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(attached_id)
        .expect("claimed letter");
    assert!(claimed.open_date > 0);
    assert_eq!(claimed.header.attachments, 0);
    tick(&w.server, claimed.open_date + system_read_secs(&w.server));
    assert!(
        w.server
            .systems
            .economy
            .mail
            .get_mail_by_id(attached_id)
            .is_none(),
        "an attachment-free read system mail follows the 5-day window"
    );
}

#[tokio::test]
async fn strict_system_mail_deletion_is_opt_in() {
    let mut w = mail_world();
    let mut txn = ItemTxn::begin(&w.server);
    assert!(send_system_mail(
        &mut txn,
        mail_type::SYS_EXPRESS,
        ".questReward",
        2,
        "old",
        "",
        125,
        vec![]
    ));
    drop(txn);
    let id = bob_mail_ids(&w.server)[0];
    w.server.systems.economy.mail.set_retention(MailRetention {
        delete_system_mail_with_attachments: true,
        ..MailRetention::default()
    });
    w.server
        .systems
        .economy
        .mail
        .with_mail_mut(id, |m| m.body.recv_date = now() - 120 * SECONDS_PER_DAY);
    w.a.drain();
    w.b.drain();
    tick(&w.server, now());
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_none(),
        "DeleteSystemMailWithAttachments=true follows the guide's deletion"
    );
    assert!(w.server.systems.economy.mail.state().deleted == vec![id]);
}

#[tokio::test]
async fn retention_upgrade_grace_gives_old_mail_one_window() {
    let mut w = mail_world();
    let mut txn = ItemTxn::begin(&w.server);
    assert!(send_system_mail(
        &mut txn,
        mail_type::SYS_EXPRESS,
        ".questReward",
        2,
        "old",
        "",
        0,
        vec![]
    ));
    drop(txn);
    let id = bob_mail_ids(&w.server)[0];
    let start = now();
    w.server.systems.economy.mail.set_retention(MailRetention {
        grace_start_unix: start,
        ..MailRetention::default()
    });
    w.server
        .systems
        .economy
        .mail
        .with_mail_mut(id, |m| m.body.recv_date = start - 120 * SECONDS_PER_DAY);
    w.a.drain();
    w.b.drain();
    let deadline = start + system_unread_secs(&w.server);
    tick(&w.server, deadline - 1);
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_some(),
        "mail that existed at the upgrade gets one full window from the first start"
    );
    tick(&w.server, deadline);
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_none(),
        "the grace is one window, not an endless extension"
    );
}

#[tokio::test]
async fn read_mail_without_open_date_uses_the_receipt_date() {
    let mut w = mail_world();
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 0, 0);
    w.server.systems.economy.mail.with_mail_mut(id, |m| {
        m.header.status = mail_status::READ;
        m.open_date = 0;
        m.body.recv_date = now() - 6 * SECONDS_PER_DAY;
    });
    let recv = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(id)
        .unwrap()
        .body
        .recv_date;
    w.a.drain();
    w.b.drain();
    tick(&w.server, recv + normal_read_secs(&w.server) - 1);
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_some(),
        "a legacy read row without open_date uses the receipt date"
    );
    tick(&w.server, recv + normal_read_secs(&w.server));
    assert!(w.server.systems.economy.mail.get_mail_by_id(id).is_none());
}

#[tokio::test]
async fn first_attachment_claim_starts_the_read_clock() {
    let mut w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 1, 0);
    // Ten days old: if `open_date` were not written on the claim, the receipt-date fallback
    // would put the 5-day read deadline in the past and delete the letter at the next tick.
    w.server
        .systems
        .economy
        .mail
        .with_mail_mut(id, |m| m.body.recv_date = now() - 10 * SECONDS_PER_DAY);
    w.a.drain();
    w.b.drain();
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert_eq!(
            mail_character::get_attached(&mut txn, &w.pb, id, true, true, true, 0),
            Ok(true)
        );
    }
    let claimed = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert_eq!(claimed.header.status, mail_status::READ);
    assert!(
        claimed.open_date > claimed.body.recv_date,
        "the first attachment claim writes open_date"
    );
    assert!(w.server.items.item_by_id(ids[0]).is_some());
    let opened = claimed.open_date;
    tick(&w.server, opened + normal_read_secs(&w.server) - 1);
    assert!(w.server.systems.economy.mail.get_mail_by_id(id).is_some());
    tick(&w.server, opened + normal_read_secs(&w.server));
    assert!(w.server.systems.economy.mail.get_mail_by_id(id).is_none());
}

#[tokio::test]
async fn commercial_mail_is_not_on_the_ordinary_mail_retention_timer() {
    let mut w = mail_world();
    let mut txn = ItemTxn::begin(&w.server);
    assert!(send_system_mail(
        &mut txn,
        mail_type::CHARGED,
        ".inGameShop",
        2,
        "shop",
        "",
        0,
        vec![]
    ));
    drop(txn);
    let id = bob_mail_ids(&w.server)[0];
    w.server
        .systems
        .economy
        .mail
        .with_mail_mut(id, |m| m.body.recv_date = now() - 365 * SECONDS_PER_DAY);
    w.a.drain();
    w.b.drain();
    tick(&w.server, now());
    assert!(
        w.server.systems.economy.mail.get_mail_by_id(id).is_some(),
        "the commercial mailbox has its own guide (ui_texts 4464)"
    );
}

#[tokio::test]
async fn expiry_receipt_boundary_returns_zero_one_ten_attachments_once() {
    for count in [0, 1, 10] {
        for kind in [mail_type::NORMAL, mail_type::EXPRESS] {
            let mut w = mail_world();
            let (id, ids) = send_player_mail(&w, kind, count, 125);
            let original = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
            let money = (w.pa.data().money, w.pb.data().money);
            let due = original.body.recv_date + normal_unread_secs(&w.server);
            tick(&w.server, due - 1);
            assert!(
                !w.server
                    .systems
                    .economy
                    .mail
                    .get_mail_by_id(id)
                    .unwrap()
                    .header
                    .returned
            );
            if kind == mail_type::NORMAL {
                assert!((NORMAL_MAIL_DELAY_SECS..=NORMAL_MAIL_DELAY_SECS + 2)
                    .contains(&(original.body.recv_date - original.body.send_date)));
            }
            w.a.drain();
            w.b.drain();
            tick(&w.server, due);
            let returned = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
            assert!(returned.header.returned && returned.is_dirty && returned.is_delivered);
            assert_eq!(
                (returned.header.receiver_id, returned.header.sender_id),
                (1, 2)
            );
            assert_eq!(returned.body.recv_date, due);
            assert_eq!(returned.body.send_date, original.body.send_date);
            assert_eq!(returned.body.attachments, ids);
            assert_eq!(returned.body.copper_coins, 125);
            assert_eq!((w.pa.data().money, w.pb.data().money), money);
            assert_eq!(
                (
                    w.pa.data().unread_mail.received,
                    w.pb.data().unread_mail.received
                ),
                (1, 0)
            );
            assert_eq!(w.a.drain_ops(), vec![sc::SCGotMailPacket]);
            assert_eq!(
                w.b.drain_ops(),
                vec![sc::SCMailReturnedPacket, sc::SCCountUnreadMailPacket]
            );
            for &item in &ids {
                let item = w.server.items.item_by_id(item).unwrap();
                assert_eq!((item.owner_id, item.slot_type), (1, slot_type::MAIL));
            }
            tick(&w.server, due);
            tick(&w.server, due + normal_unread_secs(&w.server) - 1);
            assert_eq!(
                w.server.systems.economy.mail.get_mail_by_id(id).unwrap(),
                returned
            );
            assert!(w.a.drain().is_empty() && w.b.drain().is_empty());

            // A returned letter gets one fresh window. Every case here still has the 125
            // copper attachment, so the returned letter is kept forever: it never bounces
            // back and the expiry timer never destroys its escrow.
            tick(&w.server, due + normal_unread_secs(&w.server));
            tick(&w.server, due + normal_unread_secs(&w.server) + 1);
            assert_eq!((w.pa.data().money, w.pb.data().money), money);
            let kept = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
            assert!(kept.header.returned);
            assert_eq!(kept.body.attachments, ids);
            assert_eq!(kept.body.copper_coins, 125);
            assert_eq!(w.pa.data().unread_mail.received, 1);
            assert_eq!(w.pb.data().unread_mail.received, 0);
            assert!(w.server.systems.economy.mail.state().deleted.is_empty());
            for &item in &ids {
                let item = w.server.items.item_by_id(item).unwrap();
                assert_eq!((item.owner_id, item.slot_type), (1, slot_type::MAIL));
                assert_eq!(
                    w.server
                        .items
                        .removed_items()
                        .iter()
                        .filter(|&&removed| removed == item.id)
                        .count(),
                    0
                );
            }
        }
    }
}

#[tokio::test]
async fn expiry_returns_read_player_mail_and_keeps_registered_self_and_deleted_sender() {
    for case in 0..4 {
        let mut w = mail_world();
        let kind = if case == 1 {
            mail_type::REGISTERED
        } else {
            mail_type::EXPRESS
        };
        let (id, ids) = send_player_mail(&w, kind, 10, 50);
        if case == 0 {
            let mut txn = ItemTxn::begin(&w.server);
            mail_character::read_mail(&mut txn, &w.pb, false, id);
        }
        if case == 1 {
            // Reconnect resets counters before the reconstructed delivery flags.
            w.pb.data().unread_mail.reset_received();
        }
        w.server.systems.economy.mail.with_mail_mut(id, |m| {
            if case == 2 {
                m.header.sender_id = 2;
                m.header.sender_name = "Bob".into();
            }
            if case == 3 {
                m.header.sender_id = 999;
            }
        });
        let money = (w.pa.data().money, w.pb.data().money);
        let due = w
            .server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .body
            .recv_date
            + normal_unread_secs(&w.server);
        w.a.drain();
        w.b.drain();
        tick(&w.server, due);
        tick(&w.server, due + 1);
        let kept = w
            .server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .expect("mail with unclaimed attachments is never destroyed");
        assert_eq!((w.pa.data().money, w.pb.data().money), money);
        assert!(w.server.systems.economy.mail.state().deleted.is_empty());
        if case == 0 {
            // Read player mail: returned to the sender at the unread deadline.
            assert!(kept.header.returned);
            assert_eq!((kept.header.receiver_id, kept.header.sender_id), (1, 2));
            for item in ids {
                let item = w.server.items.item_by_id(item).unwrap();
                assert_eq!((item.owner_id, item.slot_type), (1, slot_type::MAIL));
            }
        } else {
            // REGISTERED, self-addressed and a deleted sender keep the escrow in place.
            assert!(!kept.header.returned);
            assert_eq!(kept.header.receiver_id, 2);
            for item in ids {
                let item = w.server.items.item_by_id(item).unwrap();
                assert_eq!((item.owner_id, item.slot_type), (2, slot_type::MAIL));
            }
        }
    }
}

#[tokio::test]
async fn expiry_zero_receipt_and_clock_rollback_obey_the_same_boundary() {
    let w = mail_world();
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 0, 0);
    w.server.systems.economy.mail.with_mail_mut(id, |m| {
        m.body.send_date = 0;
        m.body.recv_date = 0;
    });
    for time in [-1, 0, normal_unread_secs(&w.server) - 1] {
        tick(&w.server, time);
        assert!(
            !w.server
                .systems
                .economy
                .mail
                .get_mail_by_id(id)
                .unwrap()
                .header
                .returned
        );
    }
    tick(&w.server, normal_unread_secs(&w.server));
    assert!(
        w.server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .header
            .returned
    );
}

#[tokio::test]
async fn expiry_keeps_loose_escrow_and_a_stale_reference_to_taken_item() {
    let w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 2, 50);
    let container = mail_container(&w.pb).unwrap();
    let loose = container.lock().unwrap().remove_item(ids[0]).unwrap();
    w.server.items.add_loose(loose);
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(add_or_move_existing_item(
            &mut txn,
            &Inv::of(&w.pb).bag(),
            task_type::INVALID,
            ids[1],
            -1
        ));
    }
    w.server
        .systems
        .economy
        .mail
        .with_mail_mut(id, |m| m.header.returned = true);
    let released = Arc::new(Mutex::new(Vec::new()));
    let item_releases = released.clone();
    w.server
        .ids
        .item
        .install_release_hook(move |id| item_releases.lock().unwrap().push(id));
    let mail_released = Arc::new(Mutex::new(Vec::new()));
    let mail_releases = mail_released.clone();
    w.server
        .ids
        .mail
        .install_release_hook(move |id| mail_releases.lock().unwrap().push(id));
    let due = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(id)
        .unwrap()
        .body
        .recv_date
        + normal_unread_secs(&w.server);
    tick(&w.server, due);
    tick(&w.server, due + 1);
    // The letter is already returned once, so it cannot bounce again. It keeps both the loose
    // escrow item and the stale reference to the item taken into the bag; nothing is destroyed
    // and no id is released.
    assert_eq!(
        w.server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .body
            .attachments,
        ids
    );
    assert!(w.server.items.item_by_id(ids[0]).is_some());
    assert!(w.server.items.holding_container(ids[0]).is_none());
    assert_eq!(
        w.server.items.item_by_id(ids[1]).unwrap().slot_type,
        slot_type::INVENTORY
    );
    assert!(released.lock().unwrap().is_empty());
    assert!(mail_released.lock().unwrap().is_empty());
    assert!(w.server.items.removed_items().is_empty());
}

#[tokio::test]
async fn expiry_offline_return_reload_and_reconnect_do_not_repeat_refund() {
    let mut w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 1, 75);
    w.server.systems.economy.mail.with_mail_mut(id, |m| {
        m.body.recv_date = now() - normal_unread_secs(&w.server) - 10;
    });
    w.server.world.world(0).unwrap().delete(&w.pa);
    w.server.world.world(0).unwrap().delete(&w.pb);
    assert!(w.server.world.try_remove_character(w.pa.obj_id));
    assert!(w.server.world.try_remove_character(w.pb.obj_id));
    assert!(w.server.world.character_by_id(1).is_none());
    assert!(w.server.world.character_by_id(2).is_none());
    let due = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(id)
        .unwrap()
        .body
        .recv_date
        + normal_unread_secs(&w.server);
    w.a.drain();
    w.b.drain();
    tick(&w.server, due);
    let returned = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert!(returned.header.returned && !returned.is_delivered);
    assert_eq!(w.server.items.item_by_id(ids[0]).unwrap().owner_id, 1);
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    // Production row loader has no persisted is_delivered flag; returned + receipt date
    // must alone prevent another refund after a restart.
    let rows = vec![row(&returned)];
    w.server
        .systems
        .economy
        .mail
        .load_rows(&w.server, rows, due);
    tick(&w.server, due);
    assert_eq!(w.server.systems.economy.mail.count(), 1);
    assert_eq!(
        w.server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .header
            .receiver_id,
        1
    );
    w.pa.data().unread_mail.reset_received();
    assert!(w.server.world.try_add_character(&w.pa));
    w.server
        .world
        .world(0)
        .unwrap()
        .spawn(&w.pa, &w.server.view_ctx());
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert_eq!(get_current_mail_list(&mut txn, 1), vec![id]);
    }
    assert_eq!(w.pa.data().unread_mail.received, 1);
    let before = w.pa.data().money;
    let mut txn = ItemTxn::begin(&w.server);
    assert_eq!(
        mail_character::get_attached(&mut txn, &w.pa, id, true, true, true, 0),
        Ok(true)
    );
    assert_eq!(
        mail_character::get_attached(&mut txn, &w.pa, id, true, true, true, 0),
        Ok(true)
    );
    assert_eq!(w.pa.data().money, before + 75);
    assert_eq!(
        w.server.items.item_by_id(ids[0]).unwrap().slot_type,
        slot_type::INVENTORY
    );
}

#[tokio::test]
async fn expiry_packet_edges_enforce_current_owner_before_timer_and_return_auth() {
    for opcode in [0x09eu16, 0x0a1, 0x0a3, 0x09c] {
        let mut w = mail_world();
        let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 1, 25);
        let past = now() - normal_unread_secs(&w.server) - 1;
        w.server
            .systems
            .economy
            .mail
            .with_mail_mut(id, |m| m.body.recv_date = past);
        let original = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
        let mut request = frame(opcode, id);
        if opcode == 0x0a1 {
            request.push(0);
        }
        if opcode == 0x09c {
            request = vec![0xdd, 1, 0, 0, 0x9c, 0, 0];
            request.extend_from_slice(&id.to_le_bytes());
        }
        // Sender may read their sent copy, but cannot operate the recipient's copy.
        w.server
            .handle_frame(&mut w.a.conn, &request)
            .await
            .unwrap();
        assert_eq!(
            w.server.systems.economy.mail.get_mail_by_id(id).unwrap(),
            original
        );
        let money = (w.pa.data().money, w.pb.data().money);
        w.server
            .handle_frame(&mut w.b.conn, &request)
            .await
            .unwrap();
        let returned = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
        assert!(returned.header.returned);
        assert_eq!(returned.header.receiver_id, 1);
        assert_eq!(returned.body.copper_coins, 25);
        assert_eq!(w.server.items.item_by_id(ids[0]).unwrap().owner_id, 1);
        assert_eq!((w.pa.data().money, w.pb.data().money), money);
        // Replay by the former receiver cannot take or delete the returned escrow.
        w.server
            .handle_frame(&mut w.b.conn, &request)
            .await
            .unwrap();
        assert_eq!(
            w.server.systems.economy.mail.get_mail_by_id(id).unwrap(),
            returned
        );
    }
}

#[tokio::test]
async fn expiry_serializes_with_take_delete_and_send() {
    for take_first in [false, true] {
        let w = mail_world();
        let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 1, 100);
        let due = w
            .server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .body
            .recv_date
            + normal_unread_secs(&w.server);
        let sender_money = w.pa.data().money;
        let receiver_money = w.pb.data().money;
        // Force either lock acquisition order, then let a second tick, take, delete and
        // send contend for the real economy lock on separate OS threads.
        if take_first {
            let mut txn = ItemTxn::begin(&w.server);
            mail_character::get_attached(&mut txn, &w.pb, id, true, true, true, 0).unwrap();
        } else {
            tick(&w.server, due);
        }
        let new_id = w.server.systems.economy.mail.get_new_mail_id(&w.server);
        let server = w.server.clone();
        let receiver = w.pb.clone();
        crate::test_support::finishes_in_time("mail expiry/take/delete/send", move || {
            let runtime = tokio::runtime::Handle::current();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let _runtime = runtime.enter();
                    tick(&server, due);
                });
                scope.spawn(|| {
                    let _runtime = runtime.enter();
                    let mut txn = ItemTxn::begin(&server);
                    mail_character::get_attached(&mut txn, &receiver, id, true, true, true, 0)
                        .unwrap();
                    mail_character::delete_mail(&mut txn, &receiver, id, false);
                });
                scope.spawn(|| {
                    let _runtime = runtime.enter();
                    let mut txn = ItemTxn::begin(&server);
                    let mut reward = BaseMail {
                        id: i64::from(new_id),
                        mail_type: mail_type::NORMAL,
                        receiver_name: "Bob".into(),
                        title: "reward".into(),
                        ..BaseMail::default()
                    };
                    reward.header.receiver_id = 2;
                    reward.body.recv_date = now();
                    assert_eq!(send_mail(&mut txn, reward), Some(i64::from(new_id)));
                });
            });
        });
        let item = w.server.items.item_by_id(ids[0]).unwrap();
        if take_first {
            assert_eq!((item.owner_id, item.slot_type), (2, slot_type::INVENTORY));
            assert_eq!(w.pb.data().money, receiver_money + 100);
            assert!(w.server.systems.economy.mail.get_mail_by_id(id).is_none());
        } else {
            assert_eq!((item.owner_id, item.slot_type), (1, slot_type::MAIL));
            assert_eq!(w.pb.data().money, receiver_money);
            assert_eq!(
                w.server
                    .systems
                    .economy
                    .mail
                    .get_mail_by_id(id)
                    .unwrap()
                    .body
                    .copper_coins,
                100
            );
        }
        assert_eq!(w.pa.data().money, sender_money);
    }
}

#[tokio::test]
async fn expiry_preserves_system_tax_auction_quest_and_legacy_billing_metadata() {
    let w = mail_world();
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 0, 25);
    // New P2P sending rejects money1/2. Historical demand/point fields are opaque:
    // neither an expiry nor a return may convert these into copper.
    w.server
        .systems
        .economy
        .mail
        .with_mail_mut(id, |m| m.attach_money(25, 5000, 7000));
    let original = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    let due = original.body.recv_date + normal_unread_secs(&w.server);
    {
        let mut txn = ItemTxn::begin(&w.server);
        // Normal-type system mail (sender id 0) follows the normal 30-day window; the
        // system types below keep the 90-day one.
        for kind in [
            mail_type::BILLING,
            mail_type::AUC_OFF_FAIL,
            mail_type::AUC_OFF_SUCCESS,
            mail_type::AUC_BID_WIN,
            mail_type::AUC_BID_FAIL,
            mail_type::SYS_EXPRESS,
            mail_type::CHARGED,
            mail_type::SYS_SELL_BACKPACK,
        ] {
            assert!(send_system_mail(
                &mut txn,
                kind,
                "__QUEST__",
                2,
                "owned elsewhere",
                "",
                10,
                vec![]
            ));
        }
    }
    let protected: Vec<_> = w
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(key, _)| *key != id)
        .map(|(_, m)| m.clone())
        .collect();
    let money = (w.pa.data().money, w.pb.data().money);
    tick(&w.server, due);
    let returned = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert_eq!(
        (
            returned.body.copper_coins,
            returned.body.billing_amount,
            returned.body.money_amount2
        ),
        (25, 5000, 7000)
    );
    tick(&w.server, due + normal_unread_secs(&w.server));
    assert_eq!((w.pa.data().money, w.pb.data().money), money);
    for m in protected {
        assert_eq!(
            w.server.systems.economy.mail.get_mail_by_id(m.id).unwrap(),
            m
        );
    }
}

#[tokio::test]
async fn expiry_failed_return_does_not_publish_owner_or_success_then_retries() {
    let mut w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 2, 100);
    let original = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
    let item = w.server.items.item_by_id(ids[0]).unwrap();
    // A mismatched escrow must not steal an inventory item or announce a refund.
    mutate_item(&w.server, ids[0], |i| i.slot_type = slot_type::INVENTORY);
    w.a.drain();
    w.b.drain();
    let due = original.body.recv_date + normal_unread_secs(&w.server);
    tick(&w.server, due);
    assert_eq!(
        w.server.systems.economy.mail.get_mail_by_id(id).unwrap(),
        original
    );
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    mutate_item(&w.server, ids[0], |i| *i = item);
    let target = mail_container(&w.pa).unwrap();
    target.lock().unwrap().set_container_size(1);
    // First attachment moves, second cannot fit. The original row and every item's
    // ownership must survive rollback before a later retry can publish the return.
    tick(&w.server, due);
    assert_eq!(
        w.server.systems.economy.mail.get_mail_by_id(id).unwrap(),
        original
    );
    for &item in &ids {
        assert_eq!(w.server.items.item_by_id(item).unwrap().owner_id, 2);
    }
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    target.lock().unwrap().set_container_size(-1);
    tick(&w.server, due);
    assert_eq!(
        w.server
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .header
            .receiver_id,
        1
    );
}

fn row(m: &BaseMail) -> MailRow {
    let mut attachment_ids = [0; MAX_MAIL_ATTACHMENTS];
    attachment_ids[..m.body.attachments.len()].copy_from_slice(&m.body.attachments);
    MailRow {
        id: m.id,
        mail_type: i64::from(m.mail_type),
        status: i64::from(m.header.status),
        title: m.title.clone(),
        text: m.body.text.clone(),
        sender_id: i64::from(m.header.sender_id),
        sender_name: m.header.sender_name.clone(),
        attachment_count: i64::from(m.header.attachments),
        receiver_id: i64::from(m.header.receiver_id),
        receiver_name: m.receiver_name.clone(),
        open_date: m.open_date,
        send_date: m.body.send_date,
        received_date: m.body.recv_date,
        returned: i64::from(m.header.returned),
        extra: m.header.extra,
        money1: i64::from(m.body.copper_coins),
        money2: i64::from(m.body.billing_amount),
        money3: i64::from(m.body.money_amount2),
        attachment_ids,
    }
}

#[tokio::test]
async fn expiry_old_save_ack_preserves_new_return_and_terminal_deletion() {
    let w = mail_world();
    // No attachments: a returned letter with attachments is kept (never destroyed), so this
    // save-ack test uses the attachment-free terminal-deletion path.
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 0, 0);
    let mails = &w.server.systems.economy.mail;
    let before = mails.prepare_save();
    let due = mails.get_mail_by_id(id).unwrap().body.recv_date + normal_unread_secs(&w.server);
    tick(&w.server, due);
    mails.acknowledge_committed(before);
    assert!(mails.get_mail_by_id(id).unwrap().is_dirty);
    let returned = mails.prepare_save();
    assert_eq!(returned.dirty.len(), 1);
    tick(&w.server, due + normal_unread_secs(&w.server));
    mails.acknowledge_committed(returned);
    assert_eq!(mails.state().deleted, vec![id]);
    assert!(mails.prepare_save().dirty.is_empty());
    let deletion = mails.prepare_save();
    mails.acknowledge_committed(deletion);
    assert!(mails.state().deleted.is_empty());
}

#[tokio::test]
async fn expiry_sequential_packet_claims_returned_money_and_items_only_once() {
    let mut w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 10, 100);
    // Return with a past (but unexpired) new receipt date so normal packet clocks apply.
    let mut txn = ItemTxn::begin(&w.server);
    assert!(return_to_sender_at(&mut txn, id, now() - 1));
    drop(txn);
    let money = w.pa.data().money;
    for _ in 0..2 {
        CSTakeAttachmentSequentially { mail_id: id }
            .handle(&w.server, &mut w.a.conn)
            .await
            .unwrap();
        tick(&w.server, now());
    }
    assert_eq!(w.pa.data().money, money + 100);
    for id in ids {
        let item = w.server.items.item_by_id(id).unwrap();
        assert_eq!((item.owner_id, item.slot_type), (1, slot_type::INVENTORY));
    }
}

#[tokio::test]
#[ignore = "private MySQL3317 via AAEMU_TEST_MYSQL; connection-local temporary tables only"]
async fn private_mysql_expiry_return_rollback_retry_reload_and_keep() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let url = std::env::var("AAEMU_TEST_MYSQL").expect("AAEMU_TEST_MYSQL");
    let options = MySqlConnectOptions::from_str(&url).unwrap();
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for table in ["mails", "items", "item_containers"] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1))
            .execute(&pool)
            .await
            .unwrap();
    }
    fn server(pool: &sqlx::MySqlPool) -> Arc<GameServer> {
        let mut server =
            crate::test_support::test_server_with(crate::systems::economy::tests::test_data());
        server.db = pool.clone();
        let server = Arc::new(server);
        server.set_self();
        server.names.add_character(1, "Alice", 1001);
        server.names.add_character(2, "Bob", 1002);
        server
    }
    async fn persist(server: &GameServer, pool: &sqlx::MySqlPool, commit: bool) {
        let _save = server.save.transaction_lock().await;
        let (mails, items) = {
            let _economy = server.systems.economy.ops_lock();
            (
                server.systems.economy.mail.prepare_save(),
                server.items.prepare_save(),
            )
        };
        let mut tx = pool.begin().await.unwrap();
        server
            .systems
            .economy
            .mail
            .write_prepared(&mut tx, mails)
            .await
            .unwrap();
        server.items.write_prepared(&mut tx, items).await.unwrap();
        if commit {
            tx.commit().await.unwrap();
            server.systems.economy.mail.commit_save();
            server.items.commit_save();
        } else {
            tx.rollback().await.unwrap();
        }
    }
    let original = server(&pool);
    let item = original
        .items
        .create(
            SINGLE,
            1,
            0,
            1,
            &original.game_data.items,
            &original.ids.item,
            now(),
        )
        .unwrap();
    let item_id = item.id;
    original.items.add_loose(item);
    let due = now() - 1;
    let id = {
        let mut m = BaseMail {
            mail_type: mail_type::EXPRESS,
            receiver_name: "Bob".into(),
            ..BaseMail::default()
        };
        m.header.sender_id = 1;
        m.header.sender_name = "Alice".into();
        m.header.receiver_id = 2;
        m.body.recv_date = due - normal_unread_secs(&original);
        m.body.send_date = m.body.recv_date;
        m.body.attachments.push(item_id);
        m.attach_money(125, 0, 0);
        let mut txn = ItemTxn::begin(&original);
        send_mail(&mut txn, m).unwrap()
    };
    persist(&original, &pool, true).await;
    tick(&original, due);
    persist(&original, &pool, false).await;
    let receiver: i32 = sqlx::query_scalar("SELECT receiver_id FROM mails WHERE id=?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receiver, 2);
    assert!(
        original
            .systems
            .economy
            .mail
            .get_mail_by_id(id)
            .unwrap()
            .is_dirty
    );
    persist(&original, &pool, true).await;

    let restarted = server(&pool);
    restarted
        .items
        .load_user_items(
            &pool,
            &restarted.game_data.items,
            1,
            &restarted.ids.container,
            &restarted.ids.item,
        )
        .await
        .unwrap();
    restarted
        .systems
        .economy
        .mail
        .load(&restarted)
        .await
        .unwrap();
    let m = restarted.systems.economy.mail.get_mail_by_id(id).unwrap();
    assert!(m.header.returned && !m.is_dirty);
    assert_eq!(
        (m.header.receiver_id, m.body.copper_coins, m.body.recv_date),
        (1, 125, due)
    );
    let item = restarted.items.item_by_id(item_id).unwrap();
    assert_eq!((item.owner_id, item.slot_type), (1, slot_type::MAIL));
    tick(&restarted, due);
    assert_eq!(
        restarted.systems.economy.mail.get_mail_by_id(id).unwrap(),
        m
    );

    // A returned letter with unclaimed attachments is kept: it never bounces back and the
    // expiry timer never destroys its escrow.
    tick(&restarted, due + normal_unread_secs(&restarted));
    persist(&restarted, &pool, false).await;
    assert_eq!(
        restarted.systems.economy.mail.get_mail_by_id(id).unwrap(),
        m
    );
    assert!(restarted.systems.economy.mail.state().deleted.is_empty());
    assert!(restarted.items.removed_items().is_empty());
    assert_eq!(
        restarted
            .items
            .item_by_id(item_id)
            .map(|i| (i.owner_id, i.slot_type)),
        Some((1, slot_type::MAIL))
    );
    persist(&restarted, &pool, true).await;
    let remaining: i64 =
        sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM mails) + (SELECT COUNT(*) FROM items)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 2, "the kept letter and its attachment");
}
