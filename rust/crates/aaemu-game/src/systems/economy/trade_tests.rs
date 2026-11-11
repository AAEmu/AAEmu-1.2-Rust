//! Player trade: byte-exact packets, C->S decoding and two-player flows through the real
//! packet handlers (no MySQL).

use aaemu_commons::{Marshal, PacketStream};

use crate::models::container::slot_type;
use crate::models::item_task::{task_type, ItemTask};
use crate::net::packet::{encode, PacketDef};
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::tests::{
    give, item_at, log_item_events, run, world, world_with, World, STACK,
};
use crate::systems::economy::trade::*;

fn body_of<P: Marshal>(p: &P) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

fn bc(v: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(v);
    s.into_bytes()
}

fn canceled(reason: i32, me: bool) -> (u16, Vec<u8>) {
    let mut s = PacketStream::new();
    s.write_i32(reason).write_bool(me);
    (sc::SCTradeCanceledPacket, s.into_bytes())
}

fn pair(op: u16, a: bool, b: bool) -> (u16, Vec<u8>) {
    (op, vec![u8::from(a), u8::from(b)])
}

fn i32_frame(op: u16, v: i32) -> (u16, Vec<u8>) {
    (op, v.to_le_bytes().to_vec())
}

/// Alice (0x100) asks Bob (0x101), Bob accepts: Alice is the owner.
fn start(w: &mut World) {
    run::<CSCanStartTradePacket>(w, 0, &bc(0x101));
    assert!(w.a.drain().is_empty());
    assert_eq!(w.b.drain(), vec![(sc::SCCanStartTradePacket, bc(0x100))]);
    run::<CSStartTradePacket>(w, 1, &bc(0x100));
    assert_eq!(w.a.drain(), vec![(sc::SCTradeStartedPacket, bc(0x101))]);
    assert_eq!(w.b.drain(), vec![(sc::SCTradeStartedPacket, bc(0x100))]);
    let t = w.server.systems.economy.trade.trades();
    assert_eq!(t.len(), 1);
    assert_eq!((t[0].owner_obj_id, t[0].target_obj_id), (0x100, 0x101));
}

fn putup_item(w: &mut World, who: u8, st: u8, slot: u8, amount: i32) {
    let mut s = PacketStream::new();
    s.write_u8(st).write_u8(slot).write_i32(amount);
    run::<CSPutupTradeItemPacket>(w, who, s.as_bytes());
}

fn putup_money(w: &mut World, who: u8, amount: i32) {
    run::<CSPutupTradeMoneyPacket>(w, who, &amount.to_le_bytes());
}

fn lock(w: &mut World, who: u8, l: bool) {
    run::<CSTradeLockPacket>(w, who, &[u8::from(l)]);
}

fn ok(w: &mut World, who: u8) {
    run::<CSTradeOkPacket>(w, who, &[]);
}

/// Two factions whose mothers (148 / 149) are hostile, the test players' own faction 101 being
/// friendly to itself.
fn hostile_world() -> World {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::faction::{relation_state, FactionRelations};
    let mut data = crate::systems::economy::tests::test_data();
    for (id, mother) in [(101, 148), (102, 149), (148, 0), (149, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..SystemFaction::default()
        });
    }
    // `player_relation` reads the runtime table (`FactionManager.Load`), not the raw rows.
    data.systems.social.relations =
        FactionRelations::from_rows(&[(148, 149, relation_state::HOSTILE)]).expect("relations");
    world_with(data)
}

/// `SCErrorMsgPacket::new(code, 0, true)` body, the code the 1.2 client shows for the same
/// trade check itself (e.g. `FUN_394fddf0` -> `FUN_39428370(0x9a)` for `too_far_away`).
fn error_frame(code: i16) -> (u16, Vec<u8>) {
    let mut s = PacketStream::new();
    s.write_i16(code)
        .write_i16(code)
        .write_u32(0)
        .write_bool(true);
    (sc::SCErrorMsgPacket, s.into_bytes())
}

/// `SCCannotStartTradePacket(other, 0)`, the client's generic refusal (it shows error 538).
fn cannot_trade(other: u32) -> (u16, Vec<u8>) {
    let mut s = PacketStream::new();
    s.write_bc(other).write_i32(0);
    (sc::SCCannotStartTradePacket, s.into_bytes())
}

#[tokio::test]
async fn hostile_players_cannot_trade_but_the_same_faction_can() {
    let mut w = hostile_world();
    // Alice (101) and Bob (101) are friendly: the normal handshake works with faction data.
    start(&mut w);
    w.a.drain();
    w.b.drain();
    cancel_trade(&w.server, w.pa.obj_id, 0, 0);
    w.a.drain();
    w.b.drain();
    // Bob joins the hostile mother faction: the request is refused before it reaches him.
    w.pb.data().unit.faction_id = 102;
    assert_eq!(
        crate::systems::social::faction::player_relation(&w.server, &w.pa, &w.pb),
        crate::systems::social::faction::relation_state::HOSTILE
    );
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.a.drain(), vec![cannot_trade(0x101)]);
    assert!(w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
    // And no invitation is left behind: accepting now does nothing.
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn a_faction_change_between_request_and_accept_refuses_the_trade() {
    let mut w = hostile_world();
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.b.drain(), vec![(sc::SCCanStartTradePacket, bc(0x100))]);
    w.pb.data().unit.faction_id = 102;
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert_eq!(w.a.drain(), vec![cannot_trade(0x101)]);
    assert!(w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn out_of_range_trade_request_and_accept_are_refused() {
    // 100 m apart: the client constant 25.0 (sqrt = 5 m) refuses.
    let mut w = world();
    w.pb.data().unit.transform.set_xyz(200.0, 100.0, 100.0);
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(
        w.a.drain(),
        vec![error_frame(super::error_message::TOO_FAR_AWAY)]
    );
    assert!(w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
    // Back in range the invitation is issued; Bob walks away before accepting.
    w.pb.data().unit.transform.set_xyz(101.0, 100.0, 100.0);
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.b.drain(), vec![(sc::SCCanStartTradePacket, bc(0x100))]);
    w.pb.data().unit.transform.set_xyz(200.0, 100.0, 100.0);
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert_eq!(
        w.a.drain(),
        vec![error_frame(super::error_message::TOO_FAR_AWAY)]
    );
    assert!(w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
    // The client boundary is `distance < 5` (`FUN_39093860` refuses `>= sqrt(25)`): 5.0 m is
    // already out of range, 4.9 m is not.
    w.pb.data().unit.transform.set_xyz(104.9, 100.0, 100.0);
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.b.drain(), vec![(sc::SCCanStartTradePacket, bc(0x100))]);
    cancel_trade(&w.server, w.pa.obj_id, 0, 0);
    w.a.drain();
    w.b.drain();
    w.pb.data().unit.transform.set_xyz(105.0, 100.0, 100.0);
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(
        w.a.drain(),
        vec![error_frame(super::error_message::TOO_FAR_AWAY)]
    );
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn dead_player_cannot_request_or_accept_a_trade() {
    let mut w = world();
    // Dead target: silently ignored, as the 1.2 client does (`FUN_394fddf0` returns before
    // sending when `IsDead`); no trade and no invitation.
    w.pb.data().unit.hp = 0;
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
    // Dead requester: the request is ignored as well.
    w.pa.data().unit.hp = 0;
    w.pb.data().unit.hp = 100;
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    // Alive again: invitation; the target dies before accepting, so the accept is refused.
    w.pa.data().unit.hp = 100;
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.b.drain(), vec![(sc::SCCanStartTradePacket, bc(0x100))]);
    w.pb.data().unit.hp = 0;
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn trade_departure_cancels_peer_and_reconnect_starts_without_stale_cleanup() {
    use crate::test_support::{spawn_player, test_character, TestClient};
    use crate::world::session;
    for disconnect in [false, true] {
        let mut w = world();
        start(&mut w);
        let item_id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 2, 0);
        w.pa.data().money = 100;
        w.pb.data().money = 200;
        putup_item(&mut w, 0, slot_type::INVENTORY, 0, 2);
        putup_money(&mut w, 0, 50);
        lock(&mut w, 0, true);
        lock(&mut w, 1, true);
        let trade = &w.server.systems.economy.trade.trades()[0];
        assert!(trade.lock_owner && trade.lock_target);
        w.a.drain();
        w.b.drain();
        let departed = w.pa.clone();
        if disconnect {
            session::on_disconnect(&w.server, &mut w.a.conn).await;
        } else {
            session::leave_world_task(&w.server, Some(&mut w.a.conn), 1, Some(departed.clone()))
                .await;
        }
        assert!(w.server.systems.economy.trade.trades().is_empty());
        let cancellations = |frames: Vec<(u16, Vec<u8>)>| {
            frames
                .into_iter()
                .filter(|(op, _)| *op == sc::SCTradeCanceledPacket)
                .collect::<Vec<_>>()
        };
        assert_eq!(cancellations(w.a.drain()), vec![canceled(1, true)]);
        assert_eq!(cancellations(w.b.drain()), vec![canceled(1, false)]);
        assert_eq!(
            item_at(&departed, slot_type::INVENTORY, 0).unwrap().id,
            item_id
        );
        assert_eq!((departed.data().money, w.pb.data().money), (100, 200));
        assert!(w
            .server
            .world
            .character_by_obj_id(departed.obj_id)
            .is_none());
        let mut reconnected = TestClient::new(&w.server, 3, 1001);
        let mut alice = test_character(1, "Alice", 100.0, 100.0);
        alice.unit.hp = 100;
        alice.unit.max_hp = 100;
        let player = spawn_player(&w.server, &reconnected, alice, departed.obj_id);
        reconnected.conn.active_char = Some(player.clone());
        w.a = reconnected;
        w.pa = player;
        w.a.drain();
        w.b.drain();
        start(&mut w); // The first fresh invitation/acceptance succeeds without a priming cancel.
        assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
        on_player_departed(&w.server, &departed); // Late old-session cleanup cannot cancel the new actor's trade.
        assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
        assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    }
}

#[tokio::test]
async fn departure_prunes_invites_without_touching_unrelated_players_trade() {
    use crate::test_support::{spawn_player, test_character, TestClient};
    for departing_recipient in [false, true] {
        let mut w = world();
        run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
        w.b.drain();
        let departing = if departing_recipient { &w.pb } else { &w.pa };
        on_player_departed(&w.server, departing);
        run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
        assert!(w.server.systems.economy.trade.trades().is_empty());
        assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
        start(&mut w);
        let outsider = TestClient::new(&w.server, 3, 1003);
        let third = spawn_player(
            &w.server,
            &outsider,
            test_character(3, "Third", 100.0, 100.0),
            0x102,
        );
        w.a.drain();
        w.b.drain();
        let before = w.server.systems.economy.trade.trades();
        on_player_departed(&w.server, &third);
        assert_eq!(w.server.systems.economy.trade.trades(), before);
        assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
        cancel_trade(&w.server, w.pa.obj_id, 0, 0);
        w.a.drain();
        w.b.drain();
        run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
        w.b.drain();
        on_player_departed(&w.server, &third);
        run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
        assert_eq!(w.a.drain(), vec![(sc::SCTradeStartedPacket, bc(0x101))]);
        assert_eq!(w.b.drain(), vec![(sc::SCTradeStartedPacket, bc(0x100))]);
        assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
    }
}

#[tokio::test]
async fn trade_invitation_decline_notifies_requester_and_cannot_be_replayed() {
    let mut w = world();
    // Accepting an unsolicited invitation cannot open a trade on another player.
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.b.drain(), vec![(sc::SCCanStartTradePacket, bc(0x100))]);
    let decline = |id| {
        let mut s = PacketStream::new();
        s.write_bc(id).write_i32(3);
        s.into_bytes()
    };
    // The requester cannot masquerade as the recipient, even using a valid peer id.
    run::<CSCannotStartTradePacket>(&mut w, 0, &decline(0x101));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    run::<CSCannotStartTradePacket>(&mut w, 1, &decline(0x100));
    let mut expected = PacketStream::new();
    expected.write_bc(0x101).write_i32(3);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCCannotStartTradePacket, expected.into_bytes())]
    );
    assert!(w.b.drain().is_empty());
    run::<CSCannotStartTradePacket>(&mut w, 1, &decline(0x100));
    run::<CSStartTradePacket>(&mut w, 1, &bc(0x100));
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert!(w.server.systems.economy.trade.trades().is_empty());
    // A fresh invitation still follows the normal handshake.
    start(&mut w);
}

#[tokio::test]
async fn packets_layout() {
    let check = |f: Vec<u8>, op: u16, body: &[u8]| {
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), op);
        assert_eq!(&f[6..], body);
    };
    check(
        encode(&SCCanStartTradePacket { obj_id: 0x0a0b0c }),
        sc::SCCanStartTradePacket,
        &[0x0c, 0x0b, 0x0a],
    );
    check(
        encode(&SCCannotStartTradePacket {
            obj_id: 0x0a0b0c,
            reason: 7,
        }),
        sc::SCCannotStartTradePacket,
        &[0x0c, 0x0b, 0x0a, 7, 0, 0, 0],
    );
    check(
        encode(&SCTradeStartedPacket { obj_id: 0x0102 }),
        sc::SCTradeStartedPacket,
        &[2, 1, 0],
    );
    check(
        encode(&SCTradeCanceledPacket {
            reason: -1,
            caused_by_me: true,
        }),
        sc::SCTradeCanceledPacket,
        &[0xff, 0xff, 0xff, 0xff, 1],
    );
    check(
        encode(&SCTradeItemPutupPacket {
            slot_type: 2,
            slot: 5,
            amount: 3,
        }),
        sc::SCTradeItemPutupPacket,
        &[2, 5, 3, 0, 0, 0],
    );
    check(
        encode(&SCTradeItemTookdownPacket {
            slot_type: 2,
            slot: 5,
        }),
        sc::SCTradeItemTookdownPacket,
        &[2, 5],
    );
    check(
        encode(&SCTradeMoneyPutupPacket {
            money_amount: 0x01020304,
        }),
        sc::SCTradeMoneyPutupPacket,
        &[4, 3, 2, 1],
    );
    check(
        encode(&SCOtherTradeMoneyPutupPacket { money_amount: 5 }),
        sc::SCOtherTradeMoneyPutupPacket,
        &[5, 0, 0, 0],
    );
    check(
        encode(&SCTradeLockUpdatePacket {
            my_lock: true,
            other_lock: false,
        }),
        sc::SCTradeLockUpdatePacket,
        &[1, 0],
    );
    check(
        encode(&SCTradeOkUpdatePacket {
            my_ok: false,
            other_ok: true,
        }),
        sc::SCTradeOkUpdatePacket,
        &[0, 1],
    );
    // Items are written with `Item.Write`.
    let w = world();
    let pa = w.pa.clone();
    give(&w, &pa, slot_type::INVENTORY, STACK, 3, 4);
    let item = item_at(&pa, 2, 4).unwrap();
    let mut want = PacketStream::new();
    item.write(&mut want);
    check(
        encode(&SCOtherTradeItemPutupPacket { item: item.clone() }),
        sc::SCOtherTradeItemPutupPacket,
        want.as_bytes(),
    );
    check(
        encode(&SCOtherTradeItemTookdownPacket { item: item.clone() }),
        sc::SCOtherTradeItemTookdownPacket,
        want.as_bytes(),
    );
    // SCTradeMadePacket: action, count, tasks, forceRemove count + ids, u32 0.
    let p = SCTradeMadePacket {
        action: task_type::TRADE,
        tasks: vec![
            ItemTask::MoneyChange(-5),
            ItemTask::ItemRemove {
                slot_type: 2,
                slot: 4,
                item_id: 9,
                template_id: 10,
            },
        ],
        force_remove: vec![11],
    };
    let mut w = PacketStream::new();
    w.write_u8(48)
        .write_u8(2)
        .write_u8(1)
        .write_i32(-5)
        .write_u8(7)
        .write_u8(2)
        .write_u8(4)
        .write_u64(9)
        .write_u32(10)
        .write_u8(1)
        .write_u64(11)
        .write_u32(0);
    check(encode(&p), sc::SCTradeMadePacket, w.as_bytes());
    assert_eq!(SCTradeMadePacket::OPCODE, 0x16a);
}

#[test]
fn client_packets_decode() {
    let mut s = PacketStream::from_bytes(vec![0x0c, 0x0b, 0x0a, 3, 0, 0, 0]);
    assert_eq!(
        CSCannotStartTradePacket::read(&mut s).unwrap(),
        CSCannotStartTradePacket {
            obj_id: 0x0a0b0c,
            reason: 3
        }
    );
    let mut s = PacketStream::from_bytes(vec![2, 7, 9, 0, 0, 0]);
    assert_eq!(
        CSPutupTradeItemPacket::read(&mut s).unwrap(),
        CSPutupTradeItemPacket {
            slot_type: 2,
            slot: 7,
            amount: 9
        }
    );
    let mut s = PacketStream::from_bytes(vec![1, 4]);
    assert_eq!(
        CSTakedownTradeItemPacket::read(&mut s).unwrap(),
        CSTakedownTradeItemPacket {
            slot_type: 1,
            slot: 4
        }
    );
    let mut s = PacketStream::from_bytes(vec![0xfe, 0xff, 0xff, 0xff]);
    assert_eq!(
        CSCancelTradePacket::read(&mut s).unwrap(),
        CSCancelTradePacket { reason: -2 }
    );
    let mut s = PacketStream::from_bytes(vec![1]);
    assert!(CSTradeLockPacket::read(&mut s).unwrap().lock);
    let mut s = PacketStream::from_bytes(vec![2, 1, 0]);
    assert_eq!(
        CSStartTradePacket::read(&mut s).unwrap(),
        CSStartTradePacket { obj_id: 0x102 }
    );
}

#[tokio::test]
async fn full_trade_items_and_money() {
    let mut w = world();
    let pa = w.pa.clone();
    let pb = w.pb.clone();
    pa.data().money = 1000;
    pb.data().money = 500;
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 5, 3);
    let bob_stack = give(&w, &pb, slot_type::INVENTORY, STACK, 2, 0);
    start(&mut w);

    // Alice puts up 2 of her 5 (the peer sees only the offered amount)
    let item = item_at(&pa, 2, 3).unwrap();
    let mut shown = item.clone();
    shown.count = 2;
    putup_item(&mut w, 0, 2, 3, 2);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCTradeItemPutupPacket, vec![2, 3, 2, 0, 0, 0])]
    );
    assert_eq!(
        w.b.drain(),
        vec![(sc::SCOtherTradeItemPutupPacket, body_of(&shown))]
    );
    // Bob puts up money and an item, then takes the item down again
    putup_money(&mut w, 1, 100);
    assert_eq!(
        w.a.drain(),
        vec![i32_frame(sc::SCOtherTradeMoneyPutupPacket, 100)]
    );
    assert_eq!(
        w.b.drain(),
        vec![i32_frame(sc::SCTradeMoneyPutupPacket, 100)]
    );
    putup_item(&mut w, 1, 2, 0, 1);
    w.a.drain();
    w.b.drain();
    run::<CSTakedownTradeItemPacket>(&mut w, 1, &[2, 0]);
    // The peer sees the same offered amount (1 of 2) as in the put-up.
    let mut bob_item = item_at(&pb, 2, 0).unwrap();
    bob_item.count = 1;
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCOtherTradeItemTookdownPacket, body_of(&bob_item))]
    );
    assert_eq!(
        w.b.drain(),
        vec![(sc::SCTradeItemTookdownPacket, vec![2, 0])]
    );
    assert!(w.server.systems.economy.trade.trades()[0]
        .target_items
        .is_empty());

    // Lock: each side sees (mine, other)
    lock(&mut w, 0, true);
    assert_eq!(
        w.a.drain(),
        vec![pair(sc::SCTradeLockUpdatePacket, true, false)]
    );
    assert_eq!(
        w.b.drain(),
        vec![pair(sc::SCTradeLockUpdatePacket, false, true)]
    );
    lock(&mut w, 0, true); // already locked: nothing
    assert!(w.a.drain().is_empty());
    lock(&mut w, 1, true);
    assert_eq!(
        w.a.drain(),
        vec![pair(sc::SCTradeLockUpdatePacket, true, true)]
    );
    assert_eq!(
        w.b.drain(),
        vec![pair(sc::SCTradeLockUpdatePacket, true, true)]
    );

    ok(&mut w, 0);
    assert_eq!(
        w.a.drain(),
        vec![pair(sc::SCTradeOkUpdatePacket, true, false)]
    );
    assert_eq!(
        w.b.drain(),
        vec![pair(sc::SCTradeOkUpdatePacket, false, true)]
    );
    ok(&mut w, 1);

    // Exchanged: only the put-up amount moved; Alice keeps the rest of her stack.
    assert_eq!(pa.data().money, 1100);
    assert_eq!(pb.data().money, 400);
    let kept = item_at(&pa, 2, 3).expect("alice keeps the remainder");
    assert_eq!((kept.id, kept.count), (id, 3));
    let moved = Inv::of(&pb)
        .item_at(slot_type::INVENTORY, 1)
        .expect("bob has the split stack");
    assert_ne!(moved.id, id);
    assert_eq!(moved.slot_type, slot_type::INVENTORY);
    assert_eq!(moved.slot, 1); // slot 0 holds Bob's own stack
    assert_eq!(moved.owner_id, 2);
    assert_eq!(moved.count, 2);
    assert_eq!(item_at(&pb, 2, 0).unwrap().id, bob_stack);

    let fa = w.a.drain();
    let fb = w.b.drain();
    assert_eq!(fa.len(), 2);
    assert_eq!(fa[0], pair(sc::SCTradeOkUpdatePacket, true, true));
    assert_eq!(fb[0], pair(sc::SCTradeOkUpdatePacket, true, true));
    // Alice's count update names her own slot; Bob receives a new stack.
    let made = |tasks: Vec<ItemTask>| {
        body_of(&SCTradeMadePacket {
            action: task_type::TRADE,
            tasks,
            force_remove: Vec::new(),
        })
    };
    assert_eq!(fa[1].0, sc::SCTradeMadePacket);
    assert_eq!(
        fa[1].1,
        made(vec![
            ItemTask::MoneyChange(100),
            ItemTask::ItemCountUpdate {
                slot_type: 2,
                slot: 3,
                item_id: id,
                count: -2,
                template_id: STACK
            }
        ])
    );
    assert_eq!(fb.len(), 2);
    assert_eq!(fb[1].0, sc::SCTradeMadePacket);
    assert_eq!(
        fb[1].1,
        made(vec![ItemTask::MoneyChange(-100), ItemTask::add(&moved)])
    );
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_eq!(item.id, id);
}

/// The giving side of a trade gets a Consumed event (deviation from C#: only the receiver's
/// Acquired was raised), so its `ItemGather` objective drops with the traded items.
#[tokio::test]
async fn trade_raises_consumed_for_the_giver_and_acquired_for_the_receiver() {
    let mut w = world();
    let log = log_item_events(&w);
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 5, 3);
    start(&mut w);
    putup_item(&mut w, 0, 2, 3, 5);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    ok(&mut w, 1);
    assert!(Inv::of(&w.pb).item_by_id(id).is_some());
    let mut events = log.0.lock().unwrap().clone();
    events.sort_unstable();
    assert_eq!(events, vec![(1, STACK, -5), (2, STACK, 5)]);
}

#[tokio::test]
async fn changes_unlock_and_ok_needs_a_lock() {
    let mut w = world();
    w.pa.data().money = 50;
    start(&mut w);
    // Ok without any lock: ignored
    ok(&mut w, 0);
    assert!(w.a.drain().is_empty());
    lock(&mut w, 0, true);
    w.a.drain();
    w.b.drain();
    // Money change opens the lockers
    putup_money(&mut w, 0, 10);
    assert_eq!(
        w.a.drain(),
        vec![
            i32_frame(sc::SCTradeMoneyPutupPacket, 10),
            pair(sc::SCTradeLockUpdatePacket, false, false)
        ]
    );
    assert_eq!(
        w.b.drain(),
        vec![
            i32_frame(sc::SCOtherTradeMoneyPutupPacket, 10),
            pair(sc::SCTradeLockUpdatePacket, false, false)
        ]
    );
    // Unlock request clears both
    lock(&mut w, 1, true);
    w.a.drain();
    w.b.drain();
    lock(&mut w, 0, false);
    assert_eq!(
        w.a.drain(),
        vec![pair(sc::SCTradeLockUpdatePacket, false, false)]
    );
    // More money than owned: the trade is canceled (caused by Alice)
    w.b.drain();
    putup_money(&mut w, 0, 51);
    assert_eq!(w.a.drain(), vec![canceled(0, true)]);
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn cancel_paths() {
    let mut w = world();
    start(&mut w);
    // Bob cancels: Alice (owner) gets causedByMe = false
    run::<CSCancelTradePacket>(&mut w, 1, &5i32.to_le_bytes());
    assert_eq!(w.a.drain(), vec![canceled(5, false)]);
    assert_eq!(w.b.drain(), vec![canceled(5, true)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());

    // Without a trade: only the sender is told
    run::<CSCancelTradePacket>(&mut w, 0, &1i32.to_le_bytes());
    assert_eq!(w.a.drain(), vec![canceled(1, true)]);
    assert!(w.b.drain().is_empty());
    putup_money(&mut w, 1, 0);
    assert_eq!(w.b.drain(), vec![canceled(0, true)]);
    ok(&mut w, 1);
    assert_eq!(w.b.drain(), vec![canceled(0, true)]);

    // Asking again while trading cancels the running trade (IsTrading) and stops
    start(&mut w);
    run::<CSCanStartTradePacket>(&mut w, 0, &bc(0x101));
    assert_eq!(w.a.drain(), vec![canceled(0, true)]);
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());

    // Putting up an item the player does not have: amount check fails -> cancel
    start(&mut w);
    let pa = w.pa.clone();
    give(&w, &pa, slot_type::INVENTORY, STACK, 2, 0);
    putup_item(&mut w, 0, 2, 0, 3);
    assert_eq!(w.a.drain(), vec![canceled(0, true)]);
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
    // Empty slot with a running trade: the C# throws, nothing happens
    start(&mut w);
    putup_item(&mut w, 0, 2, 9, 1);
    assert!(w.a.drain().is_empty());
    assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
    // Public cancel (for leave-world wiring)
    cancel_trade(&w.server, 0x100, 0, 0);
    assert_eq!(w.a.drain(), vec![canceled(0, true)]);
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
}

#[tokio::test]
async fn moving_out_of_range_cancels_with_the_client_checks_reason() {
    let mut w = world();
    start(&mut w);
    w.a.drain();
    w.b.drain();
    // `X2::GameClient::UpdateTradeChecks` (FUN_394fd9a0) sends `CSCancelTrade(2)` at
    // `>= sqrt(25) = 5` m; the server applies the same boundary from a move.
    w.pb.data().unit.transform.set_xyz(104.9, 100.0, 100.0);
    crate::systems::social::session::on_moved(&w.server, &w.pb);
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
    w.pb.data().unit.transform.set_xyz(105.0, 100.0, 100.0);
    crate::systems::social::session::on_moved(&w.server, &w.pb);
    assert_eq!(w.a.drain(), vec![canceled(2, false)]);
    assert_eq!(w.b.drain(), vec![canceled(2, true)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn a_death_cancels_a_running_trade_for_both_players() {
    let mut w = world();
    start(&mut w);
    w.a.drain();
    w.b.drain();
    let victim = crate::world::WorldObject::from(w.pa.clone());
    let killer = crate::world::WorldObject::from(w.pb.clone());
    crate::systems::skills::combat::do_die(
        &w.server,
        &victim,
        Some(&killer),
        crate::systems::skills::types::KillReason::Damage,
    );
    let canceled_only = |frames: Vec<(u16, Vec<u8>)>| {
        frames
            .into_iter()
            .filter(|(op, _)| *op == sc::SCTradeCanceledPacket)
            .collect::<Vec<_>>()
    };
    // The dead player is the canceller; the client shows ui_text 569 for reason 0.
    assert_eq!(canceled_only(w.a.drain()), vec![canceled(0, true)]);
    assert_eq!(canceled_only(w.b.drain()), vec![canceled(0, false)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn crossing_a_zone_group_border_keeps_a_running_trade() {
    use crate::data::world_data::{Zone, ZoneGroup};
    use crate::systems::economy::tests::{test_data, world_with};
    let mut data = test_data();
    for (id, name) in [(1, "Group One"), (2, "Group Two")] {
        data.zones.groups.insert(
            id,
            ZoneGroup {
                id,
                name: name.into(),
                ..Default::default()
            },
        );
    }
    for (key, group) in [(1000, 1), (2000, 2)] {
        data.zones.zones.insert(
            key,
            Zone {
                id: key,
                name: format!("Zone {key}"),
                zone_key: key,
                group_id: group,
                ..Default::default()
            },
        );
    }
    let mut w = world_with(data);
    w.pa.data().unit.transform.zone_id = 1000;
    w.pb.data().unit.transform.zone_id = 1000;
    // 2 m apart: still in trade range while the border is crossed on foot.
    w.pb.data().unit.transform.set_xyz(102.0, 100.0, 100.0);
    start(&mut w);
    w.a.drain();
    w.b.drain();
    // `handle_zone_changes` fires for plain movement; a zone-group border crossing is not a
    // client load and keeps its trade UI, so the server must keep the trade too.
    crate::world::session::on_zone_change(&w.server, &w.pa, 1000, 2000);
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
}

#[tokio::test]
async fn a_same_group_teleport_cancels_a_running_trade_at_once() {
    use crate::data::world_data::{Zone, ZoneGroup};
    use crate::models::transform::Transform;
    use crate::systems::economy::tests::{test_data, world_with};
    let mut data = test_data();
    data.zones.groups.insert(
        1,
        ZoneGroup {
            id: 1,
            name: "Group One".into(),
            ..Default::default()
        },
    );
    for key in [1000, 1001] {
        data.zones.zones.insert(
            key,
            Zone {
                id: key,
                name: format!("Zone {key}"),
                zone_key: key,
                group_id: 1,
                ..Default::default()
            },
        );
    }
    let mut w = world_with(data);
    w.pa.data().unit.transform.zone_id = 1000;
    w.pb.data().unit.transform.zone_id = 1000;
    w.pb.data().unit.transform.set_xyz(102.0, 100.0, 100.0);
    start(&mut w);
    w.a.drain();
    w.b.drain();
    let instance_id = w.pa.data().unit.transform.instance_id;
    // A portal / GM move inside one zone group is still a client load; the client clears the
    // trade UI without CSCancelTrade, so the teleport itself must end the trade.
    assert!(crate::world::session::teleport_character(
        &w.server,
        &w.pa,
        Transform {
            instance_id,
            zone_id: 1001,
            x: 102.0,
            y: 100.0,
            z: 100.0,
            ..Default::default()
        },
    ));
    assert_eq!(w.a.drain(), vec![canceled(0, true)]);
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn lifecycle_hooks_without_a_trade_send_no_cancel_packet() {
    let mut w = world();
    w.a.drain();
    w.b.drain();
    let canceled_only = |frames: Vec<(u16, Vec<u8>)>| {
        frames
            .into_iter()
            .filter(|(op, _)| *op == sc::SCTradeCanceledPacket)
            .collect::<Vec<_>>()
    };
    crate::systems::social::session::on_moved(&w.server, &w.pb);
    assert!(canceled_only(w.a.drain()).is_empty());
    assert!(canceled_only(w.b.drain()).is_empty());
    let victim = crate::world::WorldObject::from(w.pb.clone());
    let killer = crate::world::WorldObject::from(w.pa.clone());
    crate::systems::skills::combat::do_die(
        &w.server,
        &victim,
        Some(&killer),
        crate::systems::skills::types::KillReason::Damage,
    );
    assert!(canceled_only(w.a.drain()).is_empty());
    assert!(canceled_only(w.b.drain()).is_empty());
}

#[tokio::test]
async fn bag_full_cancels() {
    let mut w = world();
    let pa = w.pa.clone();
    let pb = w.pb.clone();
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 1, 0);
    for slot in 0..50 {
        give(&w, &pb, slot_type::INVENTORY, STACK, 20, slot);
    }
    assert_eq!(Inv::of(&pb).free_slot_count(slot_type::INVENTORY), 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 1);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    // Bob (target) has no room: canceled on his behalf
    assert_eq!(
        w.a.drain(),
        vec![
            pair(sc::SCTradeOkUpdatePacket, true, true),
            canceled(0, false)
        ]
    );
    assert_eq!(
        w.b.drain(),
        vec![
            pair(sc::SCTradeOkUpdatePacket, true, true),
            canceled(0, true)
        ]
    );
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, id);
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn ok_requires_both_sides_locked() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 4, 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 4);
    lock(&mut w, 0, true);
    w.a.drain();
    w.b.drain();
    // Only Alice locked: neither side can confirm.
    ok(&mut w, 0);
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    // Bob confirms without locking: ignored as well.
    ok(&mut w, 1);
    assert!(w.a.drain().is_empty() && w.b.drain().is_empty());
    assert_eq!(Inv::of(&w.pa).item_by_id(id).unwrap().count, 4);
    // Both locked, then both confirm: the trade is made.
    lock(&mut w, 1, true);
    w.a.drain();
    w.b.drain();
    ok(&mut w, 0);
    ok(&mut w, 1);
    assert!(Inv::of(&w.pb).item_by_id(id).is_some());
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn negative_money_putup_cancels_and_leaves_money_untouched() {
    let mut w = world();
    w.pa.data().money = 100;
    start(&mut w);
    putup_money(&mut w, 0, -5);
    assert_eq!(w.a.drain(), vec![canceled(0, true)]);
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_eq!(w.pa.data().money, 100);
    assert_eq!(w.pb.data().money, 0);
}

#[tokio::test]
async fn trade_ids_are_released_on_cancel_and_finish() {
    let mut w = world();
    start(&mut w);
    let first = w.server.systems.economy.trade.trades()[0].id;
    run::<CSCancelTradePacket>(&mut w, 0, &0i32.to_le_bytes());
    w.a.drain();
    w.b.drain();
    start(&mut w);
    assert_eq!(w.server.systems.economy.trade.trades()[0].id, first);
    // Finish releases it too.
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    ok(&mut w, 1);
    w.a.drain();
    w.b.drain();
    start(&mut w);
    assert_eq!(w.server.systems.economy.trade.trades()[0].id, first);
}

#[tokio::test]
async fn money_spent_after_putup_cancels() {
    let mut w = world();
    let pa = w.pa.clone();
    pa.data().money = 100;
    start(&mut w);
    putup_money(&mut w, 0, 100);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    pa.data().money = 50;
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    assert_eq!(
        w.a.drain(),
        vec![
            pair(sc::SCTradeOkUpdatePacket, true, true),
            canceled(0, true)
        ]
    );
    assert_eq!(
        w.b.drain(),
        vec![
            pair(sc::SCTradeOkUpdatePacket, true, true),
            canceled(0, false)
        ]
    );
    assert_eq!(pa.data().money, 50);
    assert_eq!(w.pb.data().money, 0);
}

/// Money, bag and stack state that must be unchanged after an aborted trade.
fn assert_untouched(w: &World, id: u64, count: i32, money: (i64, i64)) {
    assert_eq!(Inv::of(&w.pa).item_by_id(id).unwrap().count, count);
    assert!(Inv::of(&w.pb).item_by_id(id).is_none());
    assert_eq!(Inv::of(&w.pb).bag().lock().unwrap().items.len(), 0);
    assert_eq!((w.pa.data().money, w.pb.data().money), money);
}

#[tokio::test]
async fn item_shrunk_after_lock_aborts_the_trade_without_paying() {
    let mut w = world();
    w.pa.data().money = 100;
    w.pb.data().money = 500;
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 4, 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 4);
    putup_money(&mut w, 1, 300);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    // Alice gets rid of part of the stack after both locked.
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .count = 2;
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    let fa = w.a.drain();
    let fb = w.b.drain();
    // Nobody got a SCTradeMade; the trade is canceled for both.
    assert_eq!(fa.len(), 2);
    assert_eq!(fa[1], canceled(0, true));
    assert_eq!(fb[1], canceled(0, false));
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_untouched(&w, id, 2, (100, 500));
}

#[tokio::test]
async fn item_gone_after_lock_aborts_the_trade_without_paying() {
    let mut w = world();
    w.pa.data().money = 100;
    w.pb.data().money = 500;
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 4, 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 1);
    putup_money(&mut w, 1, 300);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    {
        let bag = Inv::of(&w.pa).bag();
        let mut bag = bag.lock().unwrap();
        bag.items.retain(|i| i.id != id);
        bag.update_free_slot_count();
    }
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    assert!(w
        .a
        .drain()
        .iter()
        .all(|(op, _)| *op != sc::SCTradeMadePacket));
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_eq!((w.pa.data().money, w.pb.data().money), (100, 500));
    assert_eq!(Inv::of(&w.pb).bag().lock().unwrap().items.len(), 0);
}

#[tokio::test]
async fn repeated_putups_of_one_stack_cannot_exceed_it() {
    let mut w = world();
    w.pb.data().money = 500;
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 4, 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 3);
    putup_item(&mut w, 0, 2, 0, 3); // 6 of 4
    putup_money(&mut w, 1, 300);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    ok(&mut w, 1);
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_untouched(&w, id, 4, (0, 500));
}

#[tokio::test]
async fn takedown_removes_only_the_requested_item() {
    let mut w = world();
    let a = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 2, 0);
    let b = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 3, 1);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 2);
    // Taking down an item that was never offered leaves the offer alone.
    run::<CSTakedownTradeItemPacket>(&mut w, 0, &[2, 1]);
    assert_eq!(
        w.server.systems.economy.trade.trades()[0].owner_items,
        vec![(a, 2)]
    );
    putup_item(&mut w, 0, 2, 1, 3);
    run::<CSTakedownTradeItemPacket>(&mut w, 0, &[2, 0]);
    assert_eq!(
        w.server.systems.economy.trade.trades()[0].owner_items,
        vec![(b, 3)]
    );
    run::<CSTakedownTradeItemPacket>(&mut w, 0, &[2, 1]);
    assert!(w.server.systems.economy.trade.trades()[0]
        .owner_items
        .is_empty());
}

#[tokio::test]
async fn unlocking_undoes_both_confirmations() {
    let mut w = world();
    start(&mut w);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    assert!(w.server.systems.economy.trade.trades()[0].ok_owner);
    lock(&mut w, 1, false);
    let t = w.server.systems.economy.trade.trades()[0].clone();
    assert!(!t.lock_owner && !t.lock_target && !t.ok_owner && !t.ok_target);
    // Locking again does not bring Alice's old confirmation back: Bob's OK alone finishes nothing.
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    assert_eq!(w.server.systems.economy.trade.trades().len(), 1);
    assert_eq!(
        w.a.drain(),
        vec![pair(sc::SCTradeOkUpdatePacket, false, true)]
    );
}

#[tokio::test]
async fn soul_bound_putup_cancels_the_trade() {
    use crate::models::item::ITEM_FLAG_SOUL_BOUND;
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 2, 0);
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .flags |= ITEM_FLAG_SOUL_BOUND;
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 1);
    let mut err = PacketStream::new();
    err.write_i16(super::error_message::TRADE_SOUL_BOUND_ITEM)
        .write_i16(super::error_message::TRADE_SOUL_BOUND_ITEM)
        .write_u32(0)
        .write_bool(true);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err.into_bytes()), canceled(0, true)]
    );
    assert_eq!(w.b.drain(), vec![canceled(0, false)]);
    assert!(w.server.systems.economy.trade.trades().is_empty());
}

#[tokio::test]
async fn item_bound_after_lock_aborts_the_trade_without_paying() {
    use crate::models::item::ITEM_FLAG_SOUL_BOUND;
    let mut w = world();
    w.pa.data().money = 100;
    w.pb.data().money = 500;
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 4, 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 2);
    putup_money(&mut w, 1, 300);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    // The offered stack becomes bound after both locked.
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .flags |= ITEM_FLAG_SOUL_BOUND;
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    assert!(w
        .a
        .drain()
        .iter()
        .all(|(op, _)| *op != sc::SCTradeMadePacket));
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_untouched(&w, id, 4, (100, 500));
}

#[tokio::test]
async fn item_locked_after_lock_aborts_the_trade_without_paying() {
    use crate::models::item::ITEM_FLAG_SECURE;
    let mut w = world();
    w.pa.data().money = 100;
    w.pb.data().money = 500;
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 4, 0);
    start(&mut w);
    putup_item(&mut w, 0, 2, 0, 2);
    putup_money(&mut w, 1, 300);
    lock(&mut w, 0, true);
    lock(&mut w, 1, true);
    ok(&mut w, 0);
    // The offered stack becomes locked after both sides locked.
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .flags |= ITEM_FLAG_SECURE;
    w.a.drain();
    w.b.drain();
    ok(&mut w, 1);
    assert!(w
        .a
        .drain()
        .iter()
        .all(|(op, _)| *op != sc::SCTradeMadePacket));
    assert!(w.server.systems.economy.trade.trades().is_empty());
    assert_untouched(&w, id, 4, (100, 500));
}
