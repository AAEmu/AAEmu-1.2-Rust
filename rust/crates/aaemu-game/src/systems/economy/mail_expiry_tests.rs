//! Ordinary-mail lifecycle regressions. No wall-clock sleeps, fake manager, or wire round trips.
//! Native 1.2 evidence: decomp/x2game12.c builders 393CCCD0 (money), 393CD1D0
//! (delete), serializer 397BA450 (return), header/body at 1995740-42/1995902-09,
//! and ui/scripts/x2ui/mailbox/mail/read_mail.lua:129-139
//! (receipt vs send date). No ordinary-mail expiry field/TTL is exposed there: the
//! 14-day receipt-time retention is an explicit server policy using the existing constant.

use super::*;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::systems::economy::mail_character;
use crate::systems::economy::mail_packets::CSTakeAttachmentSequentially;
use crate::systems::economy::tests::{give, world, World, SINGLE};

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
async fn expiry_receipt_boundary_returns_zero_one_ten_attachments_once() {
    for count in [0, 1, 10] {
        for kind in [mail_type::NORMAL, mail_type::EXPRESS] {
            let mut w = mail_world();
            let (id, ids) = send_player_mail(&w, kind, count, 125);
            let original = w.server.systems.economy.mail.get_mail_by_id(id).unwrap();
            let money = (w.pa.data().money, w.pb.data().money);
            let due = original.body.recv_date + MAIL_EXPIRE_DELAY_SECS;
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
            tick(&w.server, due + MAIL_EXPIRE_DELAY_SECS - 1);
            assert_eq!(
                w.server.systems.economy.mail.get_mail_by_id(id).unwrap(),
                returned
            );
            assert!(w.a.drain().is_empty() && w.b.drain().is_empty());

            // A returned letter has one last retention period; it never bounces back.
            tick(&w.server, due + MAIL_EXPIRE_DELAY_SECS);
            tick(&w.server, due + MAIL_EXPIRE_DELAY_SECS + 1);
            assert_eq!(w.server.systems.economy.mail.count(), 0);
            assert_eq!(w.pa.data().unread_mail.received, 0);
            assert_eq!(w.pb.data().unread_mail.received, 0);
            assert_eq!((w.pa.data().money, w.pb.data().money), money);
            for item in ids {
                assert!(w.server.items.item_by_id(item).is_none());
                assert!(w.server.items.holding_container(item).is_none());
                assert_eq!(
                    w.server
                        .items
                        .removed_items()
                        .iter()
                        .filter(|&&id| id == item)
                        .count(),
                    1
                );
            }
            assert_eq!(w.server.systems.economy.mail.state().deleted, vec![id]);
        }
    }
}

#[tokio::test]
async fn expiry_read_registered_self_and_deleted_sender_are_terminal() {
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
            + MAIL_EXPIRE_DELAY_SECS;
        w.a.drain();
        w.b.drain();
        tick(&w.server, due);
        tick(&w.server, due + 1);
        assert!(w.server.systems.economy.mail.get_mail_by_id(id).is_none());
        assert_eq!(w.pb.data().unread_mail.received, 0);
        assert_eq!((w.pa.data().money, w.pb.data().money), money);
        assert_eq!(w.b.drain_ops(), vec![sc::SCMailDeletedPacket]);
        for id in ids {
            assert!(w.server.items.item_by_id(id).is_none());
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
    for time in [-1, 0, MAIL_EXPIRE_DELAY_SECS - 1] {
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
    tick(&w.server, MAIL_EXPIRE_DELAY_SECS);
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
async fn expiry_disposes_loose_escrow_but_preserves_a_stale_reference_to_taken_item() {
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
        + MAIL_EXPIRE_DELAY_SECS;
    tick(&w.server, due);
    tick(&w.server, due + 1);
    assert!(w.server.items.item_by_id(ids[0]).is_none());
    assert!(w.server.items.holding_container(ids[0]).is_none());
    assert_eq!(
        w.server.items.item_by_id(ids[1]).unwrap().slot_type,
        slot_type::INVENTORY
    );
    assert_eq!(*released.lock().unwrap(), vec![ids[0] as u32]);
    assert_eq!(*mail_released.lock().unwrap(), vec![id as u32]);
    assert_eq!(w.server.items.removed_items(), vec![ids[0]]);
}

#[tokio::test]
async fn expiry_offline_return_reload_and_reconnect_do_not_repeat_refund() {
    let mut w = mail_world();
    let (id, ids) = send_player_mail(&w, mail_type::EXPRESS, 1, 75);
    w.server.systems.economy.mail.with_mail_mut(id, |m| {
        m.body.recv_date = now() - MAIL_EXPIRE_DELAY_SECS - 10;
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
        + MAIL_EXPIRE_DELAY_SECS;
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
        let past = now() - MAIL_EXPIRE_DELAY_SECS - 1;
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
            + MAIL_EXPIRE_DELAY_SECS;
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
    let due = original.body.recv_date + MAIL_EXPIRE_DELAY_SECS;
    {
        let mut txn = ItemTxn::begin(&w.server);
        for kind in [
            mail_type::NORMAL,
            mail_type::EXPRESS,
            mail_type::BILLING,
            mail_type::AUC_OFF_FAIL,
            mail_type::AUC_OFF_SUCCESS,
            mail_type::AUC_BID_WIN,
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
    tick(&w.server, due + MAIL_EXPIRE_DELAY_SECS);
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
    let due = original.body.recv_date + MAIL_EXPIRE_DELAY_SECS;
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
    let (id, _) = send_player_mail(&w, mail_type::EXPRESS, 1, 50);
    let mails = &w.server.systems.economy.mail;
    let before = mails.prepare_save();
    let due = mails.get_mail_by_id(id).unwrap().body.recv_date + MAIL_EXPIRE_DELAY_SECS;
    tick(&w.server, due);
    mails.acknowledge_committed(before);
    assert!(mails.get_mail_by_id(id).unwrap().is_dirty);
    let returned = mails.prepare_save();
    assert_eq!(returned.dirty.len(), 1);
    tick(&w.server, due + MAIL_EXPIRE_DELAY_SECS);
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
async fn private_mysql_expiry_return_rollback_retry_reload_and_disposal() {
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
        m.body.recv_date = due - MAIL_EXPIRE_DELAY_SECS;
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

    tick(&restarted, due + MAIL_EXPIRE_DELAY_SECS);
    persist(&restarted, &pool, false).await;
    assert_eq!(restarted.systems.economy.mail.state().deleted, vec![id]);
    assert!(restarted.items.removed_items().contains(&item_id));
    persist(&restarted, &pool, true).await;
    let remaining: i64 =
        sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM mails) + (SELECT COUNT(*) FROM items)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
    assert!(restarted.systems.economy.mail.state().deleted.is_empty());
    assert!(restarted.items.removed_items().is_empty());
}
