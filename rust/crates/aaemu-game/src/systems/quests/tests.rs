//! Quest subsystem tests: byte-exact packets, state machine tests per act type (on
//! hand-built quest data), the packet / event flow through a test client, the real
//! compact.sqlite3 loader validation and the port of `AAEmu.IntegrationTests` `QuestTests`.

use std::sync::{Arc, Mutex};

use aaemu_commons::PacketStream;

use super::data::{DbSphere, QuestData};
use super::engine::{Ctx, QuestEnv, UnitSnapshot};
use super::events::{self, QuestEvent};
use super::hooks::QuestHooks;
use super::model::{
    acceptor, detail, kind, status, ItemCreationDefinition, ObjectiveStatus, QuestSupplies,
};
use super::packets::*;
use super::spheres::{load_sphere_areas, SphereArea, SphereQuest};
use super::state::QuestHandle;
use super::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
use crate::data::items::{ItemData, ItemTemplate};
use crate::data::GameData;
use crate::models::char_data::{ActiveQuest, CompletedQuest};
use crate::models::character::Character;
use crate::models::item::{Item, ItemClass};
use crate::net::packet::{encode, ServerPacket};
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::social::packets::duel::SCAreaChatBubblePacket;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, PlayerRef, WorldObject};

// ----- recording hooks -----

#[derive(Default)]
pub(super) struct Rec {
    calls: Mutex<Vec<String>>,
    /// Template ids `is_auto_equip_trade_pack` answers true for.
    trade_packs: Vec<u32>,
    /// Failed mails reported by `send_quest_reward_mails`.
    failed_mails: usize,
    team: Mutex<Vec<PlayerRef>>,
}

impl Rec {
    fn push(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl QuestHooks for Rec {
    fn add_exp(&self, _: &GameServer, _: Option<&PlayerRef>, c: &mut Character, exp: i32, a: bool) {
        c.experience += exp;
        self.push(format!("exp {exp} {a}"));
    }
    fn change_money(&self, _: &GameServer, _: Option<&PlayerRef>, c: &mut Character, amount: i32) {
        c.money += i64::from(amount);
        self.push(format!("money {amount}"));
    }
    fn change_game_points(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        kind: u8,
        amount: i32,
    ) {
        self.push(format!("points {kind} {amount}"));
    }
    fn change_labor(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        amount: i16,
        a: i32,
    ) {
        self.push(format!("labor {amount} {a}"));
    }
    fn add_crime(&self, _: &GameServer, _: Option<&PlayerRef>, _: &mut Character, points: i16) {
        self.push(format!("crime {points}"));
    }
    fn acquire_default_item(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        c: &mut Character,
        task: u8,
        template_id: u32,
        count: i32,
        grade: u8,
    ) -> bool {
        give_item(c, template_id, count);
        self.push(format!("item {task} {template_id} {count} {grade}"));
        true
    }
    fn try_equip_new_backpack(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        task: u8,
        template_id: u32,
        count: i32,
        grade: u8,
    ) -> bool {
        self.push(format!("backpack {task} {template_id} {count} {grade}"));
        true
    }
    fn consume_item(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        c: &mut Character,
        task: u8,
        template_id: u32,
        count: i32,
        preferred: Option<u64>,
    ) -> i32 {
        if let Some(id) = preferred {
            let bag = c.inventory.bag().clone();
            let mut g = bag.lock().unwrap();
            if let Some(pos) = g.items.iter().position(|i| i.id == id) {
                g.items.remove(pos);
                drop(g);
                self.push(format!("consume {task} {template_id} {count}"));
                return 1;
            }
        }
        let removed = take_items(c, template_id, count);
        self.push(format!("consume {task} {template_id} {count}"));
        removed
    }
    fn send_quest_reward_mails(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        quest: u32,
        items: &[ItemCreationDefinition],
        copper: i32,
    ) -> usize {
        self.push(format!("mail {quest} {} {copper}", items.len()));
        self.failed_mails
    }
    fn is_auto_equip_trade_pack(&self, _: &GameServer, template_id: u32) -> bool {
        self.trade_packs.contains(&template_id)
    }
    fn use_skill(&self, _: &GameServer, caster: &WorldObject, skill_id: u32, target: &WorldObject) {
        self.push(format!(
            "skill {} {skill_id} {}",
            caster.obj_id(),
            target.obj_id()
        ));
    }
    fn set_npc_faction(&self, _: &GameServer, npc: &crate::world::NpcRef, faction_id: u32) {
        self.push(format!("faction {} {faction_id}", npc.obj_id));
    }
    fn team_members(&self, _: &GameServer, _: &PlayerRef) -> Vec<PlayerRef> {
        self.team.lock().unwrap().clone()
    }
}

static NEXT_ITEM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn give_item(c: &mut Character, template_id: u32, count: i32) {
    let t = ItemTemplate::plain(template_id, 1000);
    let id = NEXT_ITEM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut item = Item::create(id, &t, count, 0, 1, &ItemData::default(), 0);
    let bag = c.inventory.bag().clone();
    let mut g = bag.lock().unwrap();
    item.slot = g.get_unused_slot(-1);
    g.items.push(item);
    g.update_free_slot_count();
}

/// A `SummonMate` item at `level` (the template's class has to be forced here: the hand-built
/// item data has no `item_summon_mates` rows).
fn give_summon_mate(c: &mut Character, template_id: u32, level: u8) {
    let t = ItemTemplate::plain(template_id, 1000);
    let id = NEXT_ITEM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut item = Item::create(id, &t, 1, 0, 1, &ItemData::default(), 0);
    item.class = ItemClass::SummonMate;
    item.details.mate_level = level;
    let bag = c.inventory.bag().clone();
    let mut g = bag.lock().unwrap();
    item.slot = g.get_unused_slot(-1);
    g.items.push(item);
    g.update_free_slot_count();
}

fn set_summon_mate_level(c: &mut Character, template_id: u32, level: u8) {
    let bag = c.inventory.bag().clone();
    let mut g = bag.lock().unwrap();
    for item in g
        .items
        .iter_mut()
        .filter(|i| i.template_id == template_id && i.class == ItemClass::SummonMate)
    {
        item.details.mate_level = level;
    }
}

fn take_items(c: &mut Character, template_id: u32, count: i32) -> i32 {
    let bag = c.inventory.bag().clone();
    let mut g = bag.lock().unwrap();
    let mut left = count;
    for i in g.items.iter_mut().filter(|i| i.template_id == template_id) {
        let n = left.min(i.count);
        i.count -= n;
        left -= n;
    }
    g.items.retain(|i| i.count > 0);
    g.update_free_slot_count();
    count - left
}

// ----- hand-built quest data -----

fn quest(id: u32) -> QuestTemplate {
    QuestTemplate {
        id,
        detail_id: detail::NORMAL,
        ..QuestTemplate::default()
    }
}

fn component(id: u32, quest_id: u32, kind_id: u8) -> QuestComponentTemplate {
    QuestComponentTemplate {
        id,
        quest_id,
        kind_id,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    }
}

/// Quest 100: Start (accept from NPC 500) -> Supply (item 7000 x1) -> Progress (hunt NPC 600
/// x2) -> Ready (report to NPC 500) -> Reward (exp 300, copper 50, item 9000).
fn main_quest(d: &mut QuestData) {
    d.add_template(QuestTemplate {
        level: 10,
        ..quest(100)
    });
    d.add_component(component(1001, 100, kind::START));
    d.add_act(1001, 1, 1, 0, ActKind::ConAcceptNpc { npc_id: 500 });
    d.add_component(component(1002, 100, kind::SUPPLY));
    d.add_act(
        1002,
        2,
        1,
        1,
        ActKind::SupplyItem {
            item_id: 7000,
            grade_id: 0,
            show_action_bar: true,
            cleanup: true,
            drop_when_destroy: true,
            destroy_when_drop: true,
        },
    );
    d.add_component(component(1003, 100, kind::PROGRESS));
    d.add_act(
        1003,
        3,
        1,
        2,
        ActKind::ObjMonsterHunt {
            npc_id: 600,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    d.add_component(component(1004, 100, kind::READY));
    d.add_act(
        1004,
        4,
        1,
        0,
        ActKind::ConReportNpc {
            npc_id: 500,
            use_alias: false,
            alias_id: 0,
        },
    );
    d.add_component(component(1005, 100, kind::REWARD));
    d.add_act(1005, 5, 1, 0, ActKind::SupplyExp { exp: 300 });
    d.add_act(1005, 6, 1, 0, ActKind::SupplyCopper { amount: 50 });
    d.add_act(
        1005,
        7,
        2,
        1,
        ActKind::SupplyItem {
            item_id: 9000,
            grade_id: 1,
            show_action_bar: false,
            cleanup: false,
            drop_when_destroy: false,
            destroy_when_drop: false,
        },
    );
    d.supplies.insert(
        10,
        QuestSupplies {
            id: 10,
            level: 10,
            exp: 980,
            copper: 152,
        },
    );
}

pub(super) fn server_with(build: impl FnOnce(&mut QuestData)) -> (GameServer, Arc<Rec>) {
    let mut d = QuestData::default();
    build(&mut d);
    d.finish();
    let mut data = GameData::empty();
    // The hand-built quest data references item ids that the fixtures never loaded. Seed them
    // like the real item catalog so the reward filter (unknown templates are skipped) sees the
    // same shape as production.
    for act in &d.acts {
        let item_id = match &act.kind {
            ActKind::ConAcceptItem { item_id, .. }
            | ActKind::ConAcceptItemEquip { item_id }
            | ActKind::ConAcceptItemGain { item_id }
            | ActKind::EtcItemObtain { item_id, .. }
            | ActKind::ObjItemGather { item_id, .. }
            | ActKind::ObjItemUse { item_id, .. }
            | ActKind::ObjMateLevel { item_id, .. }
            | ActKind::SupplyItem { item_id, .. }
            | ActKind::SupplyRemoveItem { item_id }
            | ActKind::SupplySelectiveItem { item_id, .. } => *item_id,
            _ => 0,
        };
        if item_id > 0 {
            data.items
                .templates
                .entry(item_id)
                .or_insert_with(|| ItemTemplate::plain(item_id, 1000));
        }
    }
    data.systems.quests = d;
    let server = test_server_with(data);
    let rec = Arc::new(Rec::default());
    server.systems.quests.hooks.install(rec.clone());
    (server, rec)
}

/// Runs `f` on a data-level context (no player: packets land in `ctx.sent`, evaluations in
/// the local queue).
pub(super) fn with_ctx<R>(
    server: &GameServer,
    c: &mut Character,
    env: QuestEnv,
    f: impl FnOnce(&mut Ctx<'_>) -> R,
) -> R {
    let mut st = std::mem::take(&mut c.quest_state);
    st.loaded = true;
    let r = {
        let mut ctx = Ctx {
            server,
            data: &server.game_data.systems.quests,
            hooks: server.systems.quests.hooks(),
            player: None,
            c,
            st: &mut st,
            env,
            now_ms: 1_000_000,
            deferred: Vec::new(),
            buffs_changed: false,
            sent: Vec::new(),
            report_reply: None,
            report_refused: Vec::new(),
            answer_reports: true,
            answer_start_refusals: false,
        };
        let r = f(&mut ctx);
        ctx.sync_rows();
        r
    };
    c.quest_state = st;
    r
}

/// `DoQueuedEvaluations` for the local queue.
fn drain(ctx: &mut Ctx<'_>) {
    while !ctx.st.local_queue.is_empty() {
        let h = ctx.st.local_queue.remove(0);
        if ctx.st.quest(h).is_none() {
            continue;
        }
        ctx.starting_evaluation(h);
        ctx.run_current_step(h);
    }
}

fn ops(ctx: &mut Ctx<'_>) -> Vec<u16> {
    std::mem::take(&mut ctx.sent)
        .into_iter()
        .map(|p| p.0)
        .collect()
}

fn active(ctx: &Ctx<'_>, quest_id: u32) -> QuestHandle {
    ctx.st.active_handle(quest_id).expect("quest active")
}

pub(super) fn character() -> Character {
    test_character(1, "Tester", 100.0, 100.0)
}

// ----- byte-exact packets -----

fn row() -> ActiveQuest {
    ActiveQuest {
        id: 0x11,
        template_id: 100,
        status: status::PROGRESS,
        objectives: [1, 2, 0, 0, 0],
        step: kind::PROGRESS,
        acceptor_type: acceptor::NPC,
        component_id: 1003,
        acceptor_id: 500,
        time_ms: ActiveQuest::DATETIME_MIN_MS,
    }
}

/// `Quest.Write` spelled out.
fn quest_bytes(s: &mut PacketStream, q: &ActiveQuest, left_time: i32, now_ms: i64) {
    s.write_i64(i64::from(q.id))
        .write_u32(q.template_id)
        .write_u8(q.status);
    for o in q.objectives {
        s.write_i32(o);
    }
    s.write_bool(false)
        .write_bc(0)
        .write_u32(0)
        .write_bc(0)
        .write_bc(0)
        .write_i32(left_time)
        .write_u32(if left_time == -1 { 0 } else { q.component_id })
        .write_i64(0)
        .write_i64(now_ms / 1000)
        .write_u8(q.acceptor_type)
        .write_u32(q.acceptor_id);
}

fn body(p: &dyn ServerPacket) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write_body(&mut s);
    s.into_bytes()
}

#[test]
fn sc_quest_context_packets_bytes() {
    let now = 1_700_000_000_123;
    let p = SCQuestContextStartedPacket {
        quest: row(),
        component_id: 1001,
        now_ms: now,
    };
    let mut s = PacketStream::new();
    quest_bytes(&mut s, &row(), -1, now);
    s.write_u32(1001);
    assert_eq!(body(&p), s.into_bytes());
    assert_eq!(p.opcode(), sc::SCQuestContextStartedPacket);
    // framing: level 1
    let f = encode(&p);
    assert_eq!(f[1], 1);

    // Updated with a running timer: LeftTime + ComponentId
    let mut q = row();
    q.time_ms = now + 60_000;
    let p = SCQuestContextUpdatedPacket::new(q.clone(), 1003, now);
    let mut s = PacketStream::new();
    quest_bytes(&mut s, &q, 60_000, now);
    s.write_u32(1003)
        .write_i32(0)
        .write_i32(0)
        .write_i32(0)
        .write_i32(0);
    assert_eq!(body(&p), s.into_bytes());
    assert_eq!(p.opcode(), sc::SCQuestContextUpdatedPacket);

    let body8 = [1, 2, 3, 4, 5, 6, 7, 8];
    let p = SCQuestContextCompletedPacket {
        quest_id: 100,
        body: body8,
        component_id: 0,
    };
    let mut s = PacketStream::new();
    s.write_u32(100).write_raw(&body8).write_u32(0);
    assert_eq!(body(&p), s.into_bytes());
    assert_eq!(p.opcode(), sc::SCQuestContextCompletedPacket);

    let p = SCQuestContextResetPacket {
        quest_id: 130,
        body: body8,
        component_id: 2,
    };
    let mut s = PacketStream::new();
    s.write_u32(130).write_raw(&body8).write_u32(2);
    assert_eq!(body(&p), s.into_bytes());
    assert_eq!(p.opcode(), sc::SCQuestContextResetPacket);

    // SCQuestContextFailedPacket: reader FUN_397b5c90 reads "type" as u32 then "err" as
    // one byte (vtable 399b3a5c, the class registered with opcode 0x146).
    let p = SCQuestContextFailedPacket {
        quest_id: 4781,
        err: 12,
    };
    assert_eq!(body(&p), vec![0xad, 0x12, 0, 0, 12]);
    assert_eq!(p.opcode(), sc::SCQuestContextFailedPacket);
    assert_eq!(p.opcode(), 0x146);
}

#[test]
fn sc_small_quest_packets_bytes() {
    // Reader FUN_397b5ec0: bool "success" (one byte), then the u32 quest id.
    let p = SCQuestRewardedByMailPacket {
        success: true,
        quest_id: 100,
    };
    assert_eq!(body(&p), vec![1, 100, 0, 0, 0]);
    assert_eq!(p.opcode(), sc::SCQuestRewardedByMailPacket);
    let p = SCQuestRewardedByMailPacket {
        success: false,
        quest_id: 0x01020304,
    };
    assert_eq!(body(&p), vec![0, 4, 3, 2, 1]);
    assert_eq!(p.opcode(), 0x14e);
    // SCQuestMailSentPacket: reader FUN_397b5f00 (wrapper FUN_391b19b0, opcode 0x150) reads one
    // u32 through vtable slot 0x3c; the body is exactly four bytes.
    let p = SCQuestMailSentPacket { quest_mail_id: 1 };
    assert_eq!(body(&p), vec![1, 0, 0, 0]);
    assert_eq!(p.opcode(), sc::SCQuestMailSentPacket);
    assert_eq!(p.opcode(), 0x150);
    let p = SCQuestMailSentPacket {
        quest_mail_id: 0x01020304,
    };
    assert_eq!(body(&p), vec![4, 3, 2, 1]);
    let p = SCQuestMailSentPacket {
        quest_mail_id: u32::MAX,
    };
    assert_eq!(body(&p), vec![0xff, 0xff, 0xff, 0xff]);
    let p = SCDoodadQuestAcceptPacket {
        doodad_obj_id: 0x030201,
        quest_id: 7,
    };
    assert_eq!(body(&p), vec![1, 2, 3, 7, 0, 0, 0]);
    assert_eq!(p.opcode(), sc::SCDoodadQuestAcceptPacket);
    // Reader FUN_397b5e50: 3-byte bc doodad object id, then the u32 quest id.
    let p = SCDoodadCompleteQuestPacket {
        doodad_obj_id: 0x030201,
        quest_id: 7,
    };
    assert_eq!(body(&p), vec![1, 2, 3, 7, 0, 0, 0]);
    assert_eq!(p.opcode(), sc::SCDoodadCompleteQuestPacket);
    assert_eq!(p.opcode(), 0x14d);
    let p = SCTutorialSavedPacket {
        id: 5,
        body: [0x20, 0, 0, 0, 0, 0, 0, 0],
    };
    assert_eq!(body(&p), vec![5, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(p.opcode(), sc::SCTutorialSavedPacket);
    let p = SCAppellationGainedPacket { appellation_id: 9 };
    assert_eq!(body(&p), vec![9, 0, 0, 0]);
    assert_eq!(p.opcode(), sc::SCAppellationGainedPacket);
}

#[test]
fn sc_quest_unit_req_failed_writes_the_flag_bag() {
    // Reader FUN_397b5d00: u32 quest, 3-byte bc unit object id, then the FUN_397b1ef0 bag:
    // one flag byte and only the set parts, in order kind (u8), message id (u16), value (u32).
    // Every combination is spelled out as independent literals.
    let quest = [4, 3, 2, 1]; // 0x01020304
    let unit = [0x0c, 0x0b, 0x0a]; // bc 0x0a0b0c
    let head = |flags: &[u8]| -> Vec<u8> {
        let mut v = quest.to_vec();
        v.extend_from_slice(&unit);
        v.extend_from_slice(flags);
        v
    };

    let p = SCQuestUnitReqFailedPacket {
        quest_id: 0x01020304,
        unit_obj_id: 0x0a0b0c,
        kind: 0,
        message_id: 0,
        value: 0,
    };
    assert_eq!(body(&p), head(&[0x00]));
    assert_eq!(p.opcode(), sc::SCQuestUnitReqFailedPacket);
    assert_eq!(p.opcode(), 0x148);

    let p = SCQuestUnitReqFailedPacket { kind: 0x16, ..p };
    let mut expected = head(&[0x01]);
    expected.push(0x16);
    assert_eq!(body(&p), expected);

    let p = SCQuestUnitReqFailedPacket {
        kind: 0,
        message_id: 0x2233,
        ..p
    };
    let mut expected = head(&[0x02]);
    expected.extend_from_slice(&[0x33, 0x22]);
    assert_eq!(body(&p), expected);

    let p = SCQuestUnitReqFailedPacket {
        message_id: 0,
        value: 0xaabbccdd,
        ..p
    };
    let mut expected = head(&[0x04]);
    expected.extend_from_slice(&[0xdd, 0xcc, 0xbb, 0xaa]);
    assert_eq!(body(&p), expected);

    let p = SCQuestUnitReqFailedPacket {
        kind: 0x24,
        message_id: 0x1234,
        value: 0,
        ..p
    };
    let mut expected = head(&[0x03]);
    expected.extend_from_slice(&[0x24, 0x34, 0x12]);
    assert_eq!(body(&p), expected);

    let p = SCQuestUnitReqFailedPacket {
        kind: 0x01,
        message_id: 0,
        value: 0x01020304,
        ..p
    };
    let mut expected = head(&[0x05]);
    expected.extend_from_slice(&[0x01, 4, 3, 2, 1]);
    assert_eq!(body(&p), expected);

    let p = SCQuestUnitReqFailedPacket {
        kind: 0,
        message_id: 0x0001,
        value: 0x11223344,
        ..p
    };
    let mut expected = head(&[0x06]);
    expected.extend_from_slice(&[1, 0, 0x44, 0x33, 0x22, 0x11]);
    assert_eq!(body(&p), expected);

    let p = SCQuestUnitReqFailedPacket {
        kind: 0x31,
        message_id: 0xabcd,
        value: 0xdeadbeef,
        ..p
    };
    let mut expected = head(&[0x07]);
    expected.extend_from_slice(&[0x31, 0xcd, 0xab, 0xef, 0xbe, 0xad, 0xde]);
    assert_eq!(body(&p), expected);
}

#[test]
fn sc_quests_packet_writes_long_ids() {
    let now = 1_700_000_000_000;
    let p = crate::packets::world::SCQuestsPacket {
        quests: vec![row()],
        now_ms: now,
    };
    let mut s = PacketStream::new();
    s.write_i32(1);
    quest_bytes(&mut s, &row(), -1, now);
    assert_eq!(body(&p), s.into_bytes());
}

#[test]
fn cs_quest_packets_decode() {
    use crate::net::registry::ClientPacket;
    let mut s = PacketStream::new();
    s.write_u32(100).write_bc(0x200).write_bc(0).write_u32(0);
    let p = CSStartQuestContextPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        p,
        CSStartQuestContextPacket {
            quest_context_id: 100,
            npc_obj_id: 0x200,
            doodad_obj_id: 0,
            sphere_id: 0
        }
    );
    let mut s = PacketStream::new();
    s.write_u32(100)
        .write_bc(0x200)
        .write_bc(0x300)
        .write_i32(-1);
    let p =
        CSCompleteQuestContextPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        (p.npc_obj_id, p.doodad_obj_id, p.selected),
        (0x200, 0x300, -1)
    );
    let mut s = PacketStream::new();
    s.write_bc(0x200)
        .write_u32(100)
        .write_u32(1003)
        .write_u32(3);
    let p = CSQuestTalkMadePacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        (
            p.npc_obj_id,
            p.quest_context_id,
            p.quest_comp_id,
            p.quest_act_id
        ),
        (0x200, 100, 1003, 3)
    );
    let mut s = PacketStream::new();
    s.write_u32(5).write_bc(9).write_i32(2);
    let p =
        CSTryQuestCompleteAsLetItDonePacket::read(&mut PacketStream::from_bytes(s.into_bytes()))
            .unwrap();
    assert_eq!((p.id, p.obj_id, p.selected), (5, 9, 2));
    let p =
        CSDropQuestContextPacket::read(&mut PacketStream::from_bytes(vec![1, 0, 0, 0])).unwrap();
    assert_eq!(p.quest_id, 1);
    let p = CSSaveTutorialPacket::read(&mut PacketStream::from_bytes(vec![2, 0, 0, 0])).unwrap();
    assert_eq!(p.id, 2);
    let p = CSQuestStartWithPacket::read(&mut PacketStream::from_bytes(vec![
        1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 2, 10,
    ]))
    .unwrap();
    assert_eq!((p.quest, p.component, p.act), (1, 2, 3));
    assert_eq!((p.kind, p.tail), (2, QuestStartWithTail::Level(10)));
    assert!(CSStartedCinemaPacket::read(&mut PacketStream::new()).is_ok());
}

#[test]
fn cs_quest_start_with_reads_each_tail_type() {
    use crate::net::registry::ClientPacket;
    let read = |tail: &[u8]| {
        let mut body = vec![0x2c, 0x01, 0, 0, 0x2d, 0x01, 0, 0, 0x2e, 0x01, 0, 0];
        body.extend_from_slice(tail);
        CSQuestStartWithPacket::read(&mut PacketStream::from_bytes(body)).unwrap()
    };

    // Type 1: six u32.
    let p = read(&[
        1, 10, 0, 0, 0, 11, 0, 0, 0, 12, 0, 0, 0, 13, 0, 0, 0, 14, 0, 0, 0, 15, 0, 0, 0,
    ]);
    assert_eq!((p.quest, p.component, p.act, p.kind), (300, 301, 302, 1));
    assert_eq!(p.tail, QuestStartWithTail::SixU32([10, 11, 12, 13, 14, 15]));

    // Type 2: the level the player reached.
    let p = read(&[2, 10]);
    assert_eq!((p.quest, p.component, p.act, p.kind), (300, 301, 302, 2));
    assert_eq!(p.tail, QuestStartWithTail::Level(10));

    // Type 3: two u32.
    let p = read(&[3, 21, 0, 0, 0, 22, 0, 0, 0]);
    assert_eq!(p.tail, QuestStartWithTail::TwoU32([21, 22]));

    // Type 4: one u32.
    let p = read(&[4, 31, 0, 0, 0]);
    assert_eq!(p.tail, QuestStartWithTail::OneU32(31));

    // An unknown type has no tail (the 1.2 client sends 1-4 only).
    let p = read(&[9]);
    assert_eq!((p.kind, p.tail), (9, QuestStartWithTail::Unknown));
}

// ----- state machine -----

#[tokio::test]
async fn completing_a_quest_credits_the_quest_achievements() {
    use crate::systems::social::achievement::char_record_kind;
    use crate::systems::social::achievement_flow_tests::{
        achievement_ops, one_achievement, player_of,
    };
    let mut d = QuestData::default();
    main_quest(&mut d);
    d.finish();
    let mut data = GameData::empty();
    data.systems.quests = d;
    // Quest 100 completed once completes achievement 1 (COMPLETE_QUEST_TYPE, value1 = quest id).
    data.systems.social.achievements =
        one_achievement(1, 1, 0, 10, char_record_kind::COMPLETE_QUEST_TYPE, 100, 0);
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let rec = Arc::new(Rec::default());
    server.systems.quests.hooks.install(rec.clone());
    crate::systems::social::install_hooks(&server);
    let (mut client, player) = player_of(&server);
    let env = || QuestEnv {
        target: Some(UnitSnapshot {
            obj_id: 0x200,
            npc_template_id: Some(500),
            is_unit: true,
            base_unit_type: 1,
            ..UnitSnapshot::default()
        }),
        ..QuestEnv::default()
    };
    player.data().unit.current_target = 0x200;
    let quests = &server.systems.quests;

    // With a player the evaluations go through the manager's queue.
    events::with_quests(&server, &player, Some(env()), |ctx| {
        assert!(ctx.add_quest(100, false, acceptor::NPC, 500));
    })
    .expect("not nested");
    quests.run_queued_evaluations(&server);
    events::with_quests(&server, &player, Some(env()), |ctx| {
        for npc_id in [600, 601, 600] {
            ctx.dispatch(&QuestEvent::MonsterHunt { npc_id, count: 1 })
                .unwrap();
        }
    })
    .expect("not nested");
    quests.run_queued_evaluations(&server);
    // Nothing is credited before the quest is turned in.
    assert!(achievement_ops(client.drain_ops()).is_empty());

    events::with_quests(&server, &player, Some(env()), |ctx| {
        ctx.dispatch(&QuestEvent::ReportNpc {
            quest_id: 100,
            npc_id: 500,
            selected: 1,
        })
        .unwrap();
    })
    .expect("not nested");
    quests.run_queued_evaluations(&server);
    assert!(player.data().quests.completed.iter().any(|c| c.id == 1));

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn full_quest_lifecycle_on_data() {
    let (server, rec) = server_with(main_quest);
    let mut c = character();
    c.unit.current_target = 0x200;
    let env = QuestEnv {
        target: Some(UnitSnapshot {
            obj_id: 0x200,
            npc_template_id: Some(500),
            is_unit: true,
            base_unit_type: 1,
            ..UnitSnapshot::default()
        }),
        ..QuestEnv::default()
    };
    with_ctx(&server, &mut c, env, |ctx| {
        assert!(ctx.add_quest(100, false, acceptor::NPC, 500));
        // Keep targeting the giver: reaching Ready must still wait for an explicit report.
        let h = active(ctx, 100);
        // AddQuest: SCQuestContextStarted (StartQuest), then RunCurrentStep: Start -> None ->
        // Supply (step changes request an evaluation that is queued after QuestInitialized)
        assert_eq!(
            ops(ctx),
            vec![
                sc::SCQuestContextStartedPacket,
                sc::SCQuestContextUpdatedPacket
            ]
        );
        assert_eq!(ctx.st.quest(h).unwrap().step, kind::SUPPLY);
        // Supply step: the item goes into the reward pool and is distributed
        drain(ctx);
        assert_eq!(rec.take(), vec!["item 38 7000 1 0"]);
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.step, q.status), (kind::PROGRESS, status::PROGRESS));
        // Progress: two kills of NPC 600
        ctx.dispatch(&QuestEvent::MonsterHunt {
            npc_id: 600,
            count: 1,
        })
        .unwrap();
        ctx.dispatch(&QuestEvent::MonsterHunt {
            npc_id: 601,
            count: 1,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 1);
        assert_eq!(ctx.objective_status(h), ObjectiveStatus::NotReady);
        ctx.dispatch(&QuestEvent::MonsterHunt {
            npc_id: 600,
            count: 1,
        })
        .unwrap();
        drain(ctx);
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.step, q.status), (kind::READY, status::READY));
        assert_eq!(q.objectives[0], 2);
        ops(ctx);
        // Report at NPC 500
        ctx.dispatch(&QuestEvent::ReportNpc {
            quest_id: 100,
            npc_id: 500,
            selected: 1,
        })
        .unwrap();
        drain(ctx);
        // Reward: acts (exp, copper -> pool, item -> pool); the Reward step's own exp / copper
        // acts replace the level based supplies (PORTING.md), so only act rewards are paid.
        assert_eq!(
            rec.take(),
            vec![
                "exp 300 true",
                "item 38 9000 1 1",
                "money 50",
                // DropQuest after the reward: QuestCleanup (cleanup) and QuestDropped
                // (destroy_when_drop) both consume the supply item
                "consume 39 7000 1"
            ]
        );
        assert!(!ctx.has_quest(100));
        assert!(ctx.has_quest_completed(100));
        let o = ops(ctx);
        assert!(o.contains(&sc::SCQuestContextCompletedPacket));
        assert_eq!(*o.last().unwrap(), sc::SCQuestContextCompletedPacket);
    });
    assert_eq!(c.quests.removed, vec![100]);
    assert!(c.quests.active.is_empty());
    assert_eq!(
        c.quests.completed,
        vec![CompletedQuest {
            id: 1,
            body: vec![0, 0, 0, 0, 0x10, 0, 0, 0]
        }]
    );
}

#[tokio::test]
async fn reward_supply_acts_replace_level_supplies() {
    let (server, rec) = server_with(|d| {
        d.supplies.insert(
            10,
            QuestSupplies {
                id: 10,
                level: 10,
                exp: 980,
                copper: 152,
            },
        );
        // Start (auto) -> Reward, no other steps: the quest completes on add.
        for id in [110u32, 111, 112, 113] {
            d.add_template(QuestTemplate {
                level: 10,
                ..quest(id)
            });
            d.add_component(component(id * 10 + 1, id, kind::START));
            d.add_act(
                id * 10 + 1,
                id * 10 + 1,
                id * 10 + 1,
                0,
                ActKind::ConAutoComplete,
            );
            d.add_component(component(id * 10 + 2, id, kind::REWARD));
        }
        // 110: both acts; 111: neither; 112: exp only; 113: copper only.
        d.add_act(1102, 1102, 1102, 0, ActKind::SupplyExp { exp: 300 });
        d.add_act(1102, 1103, 1103, 0, ActKind::SupplyCopper { amount: 50 });
        d.add_act(1122, 1122, 1122, 0, ActKind::SupplyExp { exp: 300 });
        d.add_act(1132, 1132, 1132, 0, ActKind::SupplyCopper { amount: 50 });
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        for id in [110u32, 111, 112, 113] {
            assert!(ctx.add_quest(id, false, 0, 0), "quest {id} accepted");
            drain(ctx);
            assert!(!ctx.has_quest(id), "quest {id} completed");
        }
    });
    assert_eq!(
        rec.take(),
        vec![
            // 110: the exp / copper acts replace both level supplies.
            "exp 300 true",
            "money 50",
            // 111: no supply act -> the level based supply (exp and copper).
            "exp 980 true",
            "money 152",
            // 112: exp act replaces only the exp supply, copper still comes from the level.
            "exp 300 true",
            "money 152",
            // 113: copper act replaces only the copper supply, exp still comes from the level.
            "exp 980 true",
            "money 50",
        ]
    );
}

/// Start (auto-complete) -> Reward with no supply acts: the reward is the level-based
/// `quest_supplies` exp / copper.
fn fallback_reward_quest(d: &mut QuestData, id: u32, level: u8, degree: i32) {
    d.add_template(QuestTemplate {
        level,
        degree,
        ..quest(id)
    });
    d.add_component(component(id * 10 + 1, id, kind::START));
    d.add_act(
        id * 10 + 1,
        id * 10 + 1,
        id * 10 + 1,
        0,
        ActKind::ConAutoComplete,
    );
    d.add_component(component(id * 10 + 2, id, kind::REWARD));
}

#[tokio::test]
async fn reward_scaling_off_pays_the_unscaled_supply() {
    let (server, rec) = server_with(|d| {
        d.supplies.insert(
            10,
            QuestSupplies {
                id: 10,
                level: 10,
                exp: 980,
                copper: 152,
            },
        );
        // Degree 2, player level 10: the flag off keeps today's unscaled payout.
        fallback_reward_quest(d, 120, 10, 2);
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(120, false, 0, 0));
        drain(ctx);
        assert!(!ctx.has_quest(120));
    });
    assert_eq!(rec.take(), vec!["exp 980 true", "money 152"]);
}

#[tokio::test]
async fn reward_scaling_on_degree_two_supply_pays_seventy_five_percent() {
    let (server, rec) = server_with(|d| {
        d.supplies.insert(
            10,
            QuestSupplies {
                id: 10,
                level: 10,
                exp: 980,
                copper: 152,
            },
        );
        fallback_reward_quest(d, 120, 10, 2);
    });
    server.config.update(|cfg| cfg.quest.reward_scaling = true);
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(120, false, 0, 0));
        drain(ctx);
        assert!(!ctx.has_quest(120));
    });
    // (int)(980 * 0.75) and (int)(152 * 0.75); player level 10 / quest level 10 has no gap.
    assert_eq!(rec.take(), vec!["exp 735 true", "money 114"]);
}

#[tokio::test]
async fn reward_scaling_on_level_gap_reduces_supply_exp_but_not_copper() {
    let (server, rec) = server_with(|d| {
        d.supplies.insert(
            2,
            QuestSupplies {
                id: 2,
                level: 2,
                exp: 1000,
                copper: 100,
            },
        );
        // Player level 10, quest level 2: d = -3 -> 70 % exp; degree 3 leaves the supply as is.
        fallback_reward_quest(d, 120, 2, 3);
    });
    server.config.update(|cfg| cfg.quest.reward_scaling = true);
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(120, false, 0, 0));
        drain(ctx);
        assert!(!ctx.has_quest(120));
    });
    assert_eq!(rec.take(), vec!["exp 700 true", "money 100"]);
}

#[tokio::test]
async fn reward_scaling_on_gap_applies_to_the_exp_act_and_leaves_copper_act_alone() {
    let (server, rec) = server_with(|d| {
        fallback_reward_quest(d, 120, 2, 3);
        // The Reward component's own acts replace the level supply: exp 1000 is gap-reduced,
        // copper 50 has no gap factor.
        d.add_act(1202, 1202, 1202, 0, ActKind::SupplyExp { exp: 1000 });
        d.add_act(1202, 1203, 1203, 0, ActKind::SupplyCopper { amount: 50 });
    });
    server.config.update(|cfg| cfg.quest.reward_scaling = true);
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(120, false, 0, 0));
        drain(ctx);
        assert!(!ctx.has_quest(120));
    });
    assert_eq!(rec.take(), vec!["exp 700 true", "money 50"]);
}

#[tokio::test]
async fn reward_scaling_on_applies_the_degree_factor_to_the_copper_fallback_only() {
    let (server, rec) = server_with(|d| {
        d.supplies.insert(
            10,
            QuestSupplies {
                id: 10,
                level: 10,
                exp: 980,
                copper: 152,
            },
        );
        // An exp act replaces the exp fallback; the copper stays the level supply, scaled by
        // the degree-4 factor (x1.25) and not by the level gap (player level 10, quest 10).
        fallback_reward_quest(d, 120, 10, 4);
        d.add_act(1202, 1202, 1202, 0, ActKind::SupplyExp { exp: 1000 });
    });
    server.config.update(|cfg| cfg.quest.reward_scaling = true);
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(120, false, 0, 0));
        drain(ctx);
        assert!(!ctx.has_quest(120));
    });
    assert_eq!(rec.take(), vec!["exp 1000 true", "money 190"]);
}

#[tokio::test]
async fn add_quest_rules() {
    let (server, rec) = server_with(|d| {
        main_quest(d);
        // Quest 101: no start component
        d.add_template(quest(101));
        d.add_component(component(1011, 101, kind::PROGRESS));
        // Quest 102: start component needs level 20
        d.add_template(quest(102));
        d.add_component(component(1021, 102, kind::START));
        d.add_act(1021, 21, 21, 0, ActKind::ConAutoComplete);
        d.unit_reqs
            .by_owner
            .entry("QuestComponent".into())
            .or_default()
            .insert(
                1021,
                vec![super::unit_reqs::UnitReq {
                    id: 1,
                    owner_id: 1021,
                    owner_type: "QuestComponent".into(),
                    kind: super::unit_reqs::req_kind::LEVEL,
                    value1: 20,
                    value2: 0,
                }],
            );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(!ctx.add_quest(999, false, 0, 0), "unknown template");
        assert!(!ctx.add_quest(101, false, 0, 0), "no Start step");
        assert!(!ctx.has_quest(101));
        assert!(!ctx.add_quest(102, false, 0, 0), "unit reqs");
        ctx.c.unit.level = 20;
        assert!(ctx.add_quest(102, false, 0, 0));
        assert!(!ctx.add_quest(102, false, 0, 0), "duplicate");
        // Completed, not repeatable -> QuestDailyLimit
        ctx.set_completed_quest_flag(100, true);
        ops(ctx);
        assert!(!ctx.add_quest(100, false, acceptor::NPC, 500));
        let sent = std::mem::take(&mut ctx.sent);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, sc::SCErrorMsgPacket);
        assert_eq!(&sent[0].1[..4], &[0x6d, 0x02, 0x6d, 0x02]); // 621
                                                                // forcibly: re-adds; C# calls DropQuest(questId, true) (update, not forcibly), so the
                                                                // completed flag stays set
        assert!(ctx.add_quest(100, true, acceptor::NPC, 500));
        assert!(!ctx.has_quest_completed(100));
    });
    rec.take();
}

#[tokio::test]
async fn drop_quest_runs_cleanup_and_drop_components() {
    let (server, rec) = server_with(|d| {
        main_quest(d);
        let mut drop = component(1006, 100, kind::DROP);
        drop.skill_id = 77;
        drop.skill_self = true;
        d.add_component(drop);
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(100, false, acceptor::NPC, 500));
        drain(ctx);
        rec.take();
        ops(ctx);
        let id = ctx.st.quest(active(ctx, 100)).unwrap().id;
        ctx.drop_quest(100, true, false);
        assert!(!ctx.has_quest(100));
        // Cleanup (SupplyItem cleanup) then Drop (destroy_when_drop) consume the supply item
        assert_eq!(rec.take(), vec!["consume 39 7000 1"]);
        // Drop(update): one update packet with component 0
        assert_eq!(ops(ctx), vec![sc::SCQuestContextUpdatedPacket]);
        // Drop component skill (UseSkill of the first Drop component)
        assert!(ctx
            .deferred
            .contains(&super::engine::Deferred::UseSkillSelf { skill_id: 77 }));
        assert!(id > 0);
    });
    assert_eq!(c.quests.removed, vec![100]);
}

#[tokio::test]
async fn item_gather_and_cleanup() {
    let (server, rec) = server_with(|d| {
        d.add_template(quest(200));
        d.add_component(component(2001, 200, kind::START));
        d.add_act(2001, 1, 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(2002, 200, kind::PROGRESS));
        d.add_act(
            2002,
            2,
            1,
            3,
            ActKind::ObjItemGather {
                item_id: 8000,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                cleanup: true,
                drop_when_destroy: true,
                destroy_when_drop: false,
            },
        );
        d.add_component(component(2003, 200, kind::READY));
        d.add_act(2003, 3, 3, 0, ActKind::ConAutoComplete);
        d.add_component(component(2004, 200, kind::REWARD));
        d.add_act(2004, 4, 1, 0, ActKind::SupplyCopper { amount: 5 });
    });
    let mut c = character();
    give_item(&mut c, 8000, 1);
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(200, false, 0, 0));
        let h = active(ctx, 200);
        drain(ctx);
        // InitializeAction counted the item already in the bag
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 1);
        assert_eq!(ctx.st.quest(h).unwrap().step, kind::PROGRESS);
        give_item(ctx.c, 8000, 5);
        ctx.dispatch(&QuestEvent::ItemGather {
            item_id: 8000,
            count: 5,
        })
        .unwrap();
        // capped at MaxObjective (3)
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 3);
        drain(ctx);
        // Ready (auto complete) -> Reward -> completed; cleanup consumes min(objective, max)
        assert!(!ctx.has_quest(200));
        assert!(ctx.has_quest_completed(200));
    });
    assert_eq!(rec.take(), vec!["money 5", "consume 39 8000 3"]);
}

#[tokio::test]
async fn talk_and_team_share() {
    let (server, rec) = server_with(|d| {
        d.add_template(quest(300));
        d.add_component(component(3001, 300, kind::START));
        d.add_act(3001, 1, 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(3002, 300, kind::PROGRESS));
        d.add_act(
            3002,
            2,
            1,
            0,
            ActKind::ObjTalk {
                npc_id: 500,
                team_share: true,
                item_id: 0,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.add_act(
            3002,
            3,
            1,
            0,
            ActKind::ObjTalkNpcGroup {
                npc_group_id: 9,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        ctx.add_quest(300, false, 0, 0);
        drain(ctx);
        let h = active(ctx, 300);
        // objective indices: talk 0, group talk 1
        ctx.dispatch(&QuestEvent::TalkMade {
            quest_id: 300,
            npc_id: 500,
            component_id: 3002,
            act_id: 2,
            source_player_id: 1,
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().objectives[..2], [1, 0]);
        assert!(matches!(
            ctx.deferred.last(),
            Some(super::engine::Deferred::TeamEvent { .. })
        ));
        ctx.deferred.clear();
        // From a team mate: no further sharing
        ctx.dispatch(&QuestEvent::TalkMade {
            quest_id: 300,
            npc_id: 500,
            component_id: 3002,
            act_id: 2,
            source_player_id: 2,
        })
        .unwrap();
        assert!(ctx.deferred.is_empty());
        ctx.dispatch(&QuestEvent::TalkNpcGroupMade {
            quest_id: 300,
            npc_group_id: 9,
            npc_id: 501,
            component_id: 3002,
            act_id: 3,
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().objectives[..2], [1, 1]);
        drain(ctx);
        // No Ready / Reward components: Progress -> Ready -> Reward (both missing) completes
        assert!(!ctx.has_quest(300));
        assert!(ctx.has_quest_completed(300));
    });
    rec.take();
}

/// Quest with one progress act of `k` (objective count `count`); returns the handle.
fn single_act_quest(d: &mut QuestData, qid: u32, count: i32, k: ActKind) {
    d.add_template(quest(qid));
    d.add_component(component(qid * 10 + 1, qid, kind::START));
    d.add_act(
        qid * 10 + 1,
        qid * 10 + 1,
        qid * 10 + 1,
        0,
        ActKind::ConAutoComplete,
    );
    d.add_component(component(qid * 10 + 2, qid, kind::PROGRESS));
    d.add_act(qid * 10 + 2, qid * 10 + 2, qid * 10 + 2, count, k);
    d.add_component(component(qid * 10 + 3, qid, kind::READY));
    d.add_act(
        qid * 10 + 3,
        qid * 10 + 3,
        qid * 10 + 3,
        0,
        ActKind::ConReportNpc {
            npc_id: 1,
            use_alias: false,
            alias_id: 0,
        },
    );
}

/// Starts `qid` and runs it to its Progress step.
fn start(ctx: &mut Ctx<'_>, qid: u32) -> QuestHandle {
    assert!(ctx.add_quest(qid, false, 0, 0), "quest {qid} added");
    drain(ctx);
    active(ctx, qid)
}

fn step_of(ctx: &Ctx<'_>, h: QuestHandle) -> u8 {
    ctx.st.quest(h).map_or(0, |q| q.step)
}

#[tokio::test]
async fn objective_acts_progress_on_their_events() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            1,
            2,
            ActKind::ObjCraft {
                craft_id: 55,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        single_act_quest(
            d,
            2,
            1,
            ActKind::ObjItemUse {
                item_id: 66,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                drop_when_destroy: false,
            },
        );
        single_act_quest(
            d,
            3,
            1,
            ActKind::ObjExpressFire {
                express_key_id: 7,
                npc_group_id: 3,
                use_alias: false,
                alias_id: 0,
            },
        );
        single_act_quest(
            d,
            4,
            2,
            ActKind::ObjInteraction {
                wi_id: 1,
                doodad_id: 88,
                use_alias: false,
                team_share: false,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                alias_id: 0,
                phase: 0,
            },
        );
        single_act_quest(
            d,
            5,
            2,
            ActKind::ObjMonsterGroupHunt {
                quest_monster_group_id: 3,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        single_act_quest(
            d,
            6,
            1,
            ActKind::ObjZoneMonsterHunt {
                zone_id: 12,
                use_alias: false,
                alias_id: 0,
            },
        );
        single_act_quest(
            d,
            7,
            0,
            ActKind::ObjLevel {
                level: 11,
                use_alias: false,
                alias_id: 0,
            },
        );
        single_act_quest(
            d,
            8,
            0,
            ActKind::ObjCinema {
                cinema_id: 44,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.group_npcs.add(3, 600);
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let craft = start(ctx, 1);
        ctx.dispatch(&QuestEvent::Craft { craft_id: 54 }).unwrap();
        ctx.dispatch(&QuestEvent::Craft { craft_id: 55 }).unwrap();
        ctx.dispatch(&QuestEvent::Craft { craft_id: 55 }).unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, craft), kind::READY);

        let use_ = start(ctx, 2);
        ctx.dispatch(&QuestEvent::ItemUse { item_id: 66 }).unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, use_), kind::READY);

        let express = start(ctx, 3);
        ctx.dispatch(&QuestEvent::ExpressFire {
            npc_id: 601,
            emotion_id: 7,
        })
        .unwrap();
        ctx.dispatch(&QuestEvent::ExpressFire {
            npc_id: 600,
            emotion_id: 8,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, express), kind::PROGRESS);
        ctx.dispatch(&QuestEvent::ExpressFire {
            npc_id: 600,
            emotion_id: 7,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, express), kind::READY);

        let inter = start(ctx, 4);
        ctx.dispatch(&QuestEvent::Interaction {
            doodad_id: 88,
            source_player_id: 1,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(ctx.st.quest(inter).unwrap().objectives[0], 1);
        ctx.dispatch(&QuestEvent::Interaction {
            doodad_id: 88,
            source_player_id: 1,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, inter), kind::READY);

        let group = start(ctx, 5);
        ctx.dispatch(&QuestEvent::MonsterGroupHunt {
            group_id: 3,
            count: 2,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, group), kind::READY);

        // ZoneMonsterHunt: `count > Count` (strictly more kills than the count)
        let zone = start(ctx, 6);
        let victim = super::engine::KillUnitInfo {
            obj_id: 9,
            npc_template_id: Some(600),
            ..Default::default()
        };
        let killer = super::engine::KillUnitInfo {
            obj_id: 1,
            character_id: Some(1),
            ..Default::default()
        };
        let zk = QuestEvent::ZoneKill {
            zone_group_id: 12,
            killer,
            victim,
        };
        ctx.dispatch(&zk).unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, zone), kind::PROGRESS);
        ctx.dispatch(&zk).unwrap();
        drain(ctx);
        // C# bug mirrored: the objective is capped at MaxObjective (= Count) but RunAct wants
        // `count > Count`, so a non-score zone monster hunt never completes
        assert_eq!(ctx.st.quest(zone).unwrap().objectives[0], 1);
        assert_eq!(step_of(ctx, zone), kind::PROGRESS);

        let level = start(ctx, 7);
        ctx.c.unit.level = 11;
        ctx.dispatch(&QuestEvent::LevelUp).unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, level), kind::READY);

        let cinema = start(ctx, 8);
        ctx.dispatch(&QuestEvent::CinemaEnded { cinema_id: 0 })
            .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, cinema), kind::PROGRESS);
        ctx.dispatch(&QuestEvent::CinemaStarted { cinema_id: 0 })
            .unwrap();
        assert_eq!(ctx.st.currently_playing_cinema_id, 44);
        ctx.dispatch(&QuestEvent::CinemaEnded { cinema_id: 44 })
            .unwrap();
        assert_eq!(ctx.st.currently_playing_cinema_id, 0);
        drain(ctx);
        assert_eq!(step_of(ctx, cinema), kind::READY);
    });
}

#[tokio::test]
async fn mate_level_objective_reads_the_summon_item_level_on_mate_events() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            20,
            0,
            ActKind::ObjMateLevel {
                item_id: 14878,
                level: 50,
                cleanup: false,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut c = character();
    give_summon_mate(&mut c, 14878, 49);
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 20);
        ctx.dispatch(&QuestEvent::MateLevelUp).unwrap();
        drain(ctx);
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 0);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);

        // The item level is what the act reads (the mate row's level is copied into it on
        // summon), so reaching 50 completes the objective on the mate event.
        set_summon_mate_level(ctx.c, 14878, 50);
        ctx.dispatch(&QuestEvent::MateLevelUp).unwrap();
        drain(ctx);
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 1);
        assert_eq!(step_of(ctx, h), kind::READY);
    });
}

#[tokio::test]
async fn mate_level_cleanup_keeps_item_until_completion_and_removes_the_satisfying_one() {
    let (server, rec) = server_with(|d| {
        single_act_quest(
            d,
            21,
            0,
            ActKind::ObjMateLevel {
                item_id: 14878,
                level: 50,
                cleanup: true,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut c = character();
    give_summon_mate(&mut c, 14878, 50);
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 21);
        for _ in 0..3 {
            ctx.dispatch(&QuestEvent::MateLevelUp).unwrap();
            drain(ctx);
            assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 1);
        }
        assert!(rec.take().is_empty());
        let count = |c: &Character| {
            let bag = c.inventory.bag().clone();
            let g = bag.lock().unwrap();
            g.items.iter().filter(|i| i.template_id == 14878).count()
        };
        assert_eq!(count(ctx.c), 1);
    });

    // Completion removes exactly the level-satisfying item, not a lower-level copy.
    let mut c = character();
    give_summon_mate(&mut c, 14878, 10);
    give_summon_mate(&mut c, 14878, 50);
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 21);
        ctx.st.quest_mut(h).unwrap().status = super::model::status::COMPLETED;
        ctx.drop_quest(21, false, false);
        assert_eq!(rec.take(), vec!["consume 39 14878 1"]);
        let bag = ctx.c.inventory.bag().clone();
        let g = bag.lock().unwrap();
        let left: Vec<u8> = g
            .items
            .iter()
            .filter(|i| i.template_id == 14878)
            .map(|i| i.details.mate_level)
            .collect();
        assert_eq!(left, vec![10]);
    });
}

#[tokio::test]
async fn zone_kill_faction_and_level_rules() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            9,
            2,
            ActKind::ObjZoneKill {
                count_player_kill: 0,
                count_npc: 2,
                zone_id: 0,
                team_share: false,
                use_alias: false,
                alias_id: 0,
                lvl_min: 0,
                lvl_max: 0,
                is_party: false,
                lvl_min_npc: 5,
                lvl_max_npc: 10,
                pc_faction_id: 0,
                pc_faction_exclusive: false,
                npc_faction_id: 115,
                npc_faction_exclusive: false,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 9);
        let me = super::engine::KillUnitInfo {
            obj_id: 1,
            character_id: Some(1),
            ..Default::default()
        };
        let npc = |faction, level| super::engine::KillUnitInfo {
            obj_id: 50,
            npc_template_id: Some(600),
            faction_id: faction,
            level,
            ..Default::default()
        };
        let kill = |ctx: &mut Ctx<'_>, killer: super::engine::KillUnitInfo, v| {
            ctx.dispatch(&QuestEvent::ZoneKill {
                zone_group_id: 1,
                killer,
                victim: v,
            })
            .unwrap();
        };
        kill(ctx, me.clone(), npc(114, 7)); // wrong faction
        kill(ctx, me.clone(), npc(115, 11)); // level too high
        let other = super::engine::KillUnitInfo {
            obj_id: 2,
            character_id: Some(2),
            ..Default::default()
        };
        kill(ctx, other, npc(115, 7)); // not party: other killer ignored
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 0);
        kill(ctx, me.clone(), npc(115, 7));
        kill(ctx, me, npc(115, 5));
        drain(ctx);
        assert_eq!(step_of(ctx, h), kind::READY);
    });
}

#[tokio::test]
async fn aggro_act_credits_the_killed_victim() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            10,
            3,
            ActKind::ObjAggro {
                range: 0,
                rank1: 10,
                rank1_ratio: 100,
                rank1_item: true,
                rank2: 50,
                rank2_ratio: 50,
                rank2_item: false,
                rank3: 100,
                rank3_ratio: 10,
                rank3_item: false,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(10, false, acceptor::NPC, 600));
        drain(ctx);
        let h = active(ctx, 10);
        let npc = super::engine::KillUnitInfo {
            obj_id: 50,
            npc_template_id: Some(600),
            aggro_percent: 40.0,
            ..Default::default()
        };
        // The live death event carries Killer / Victim, rather than a target NPC.
        ctx.dispatch(&QuestEvent::Kill {
            target: None,
            killer: None,
            victim: Some(npc.clone()),
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 2);
        ctx.dispatch(&QuestEvent::Kill {
            target: Some(npc),
            killer: None,
            victim: None,
        })
        .unwrap();
        let q = ctx.st.quest(h).unwrap();
        assert_eq!(q.objectives[0], 2);
        assert_eq!(q.reward_ratio, 0.5);
        assert!(!q.allow_item_rewards);
    });
}

#[tokio::test]
async fn check_sphere_tracks_presence_without_using_a_wire_objective_slot() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            11,
            0,
            ActKind::ObjSphere {
                sphere_id: 0,
                npc_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
            },
        );
        single_act_quest(d, 12, 0, ActKind::CheckSphere { sphere_id: 0 });
    });
    let mut c = character();
    let sphere = |quest_id, component_id| SphereQuest {
        zone_id: 1,
        world_id: "main_world".into(),
        quest_id,
        component_id,
        radius: 5.0,
        xyz: (0.0, 0.0, 0.0),
    };
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 11);
        ctx.dispatch(&QuestEvent::EnterSphere {
            sphere: sphere(11, 999),
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 0);
        ctx.dispatch(&QuestEvent::EnterSphere {
            sphere: sphere(11, 112),
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 1);
        ctx.dispatch(&QuestEvent::ExitSphere {
            sphere: sphere(11, 112),
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 0);
        let h12 = start(ctx, 12);
        ctx.dispatch(&QuestEvent::EnterSphere {
            sphere: sphere(12, 999),
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, h12), kind::PROGRESS);
        ctx.dispatch(&QuestEvent::EnterSphere {
            sphere: sphere(12, 122),
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h12).unwrap().objectives, [0; 5]);
        ctx.dispatch(&QuestEvent::ExitSphere {
            sphere: sphere(12, 122),
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, h12), kind::PROGRESS);
        ctx.dispatch(&QuestEvent::EnterSphere {
            sphere: sphere(12, 122),
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, h12), kind::READY);
    });
}

// ----- imported area spheres (`sphere_area_fill.json`) -----

fn area_sphere(sphere_id: u32, x: f32, y: f32, z: f32, radius: f32) -> SphereArea {
    SphereArea {
        world: "main_world".into(),
        zone: crate::test_support::TEST_ZONE,
        sphere_id,
        xyz: (x, y, z),
        radius,
    }
}

fn db_sphere(
    id: u32,
    detail_type: &str,
    detail_id: u32,
    enter_or_leave: bool,
    team_msg: &str,
) -> DbSphere {
    DbSphere {
        id,
        name: format!("sphere {id}"),
        enter_or_leave,
        sphere_detail_id: detail_id,
        sphere_detail_type: detail_type.into(),
        team_msg: team_msg.into(),
        is_personal_msg: true,
        ..DbSphere::default()
    }
}

fn area_server(
    spheres: Vec<DbSphere>,
    areas: Vec<SphereArea>,
) -> (GameServer, TestClient, PlayerRef) {
    let mut d = QuestData::default();
    for s in spheres {
        d.spheres.insert(s.id, s);
    }
    d.finish();
    let mut data = GameData::empty();
    data.systems.quests = d;
    let server = test_server_with(data);
    server.systems.quests.spheres.set_areas(areas);
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 1001);
    let _ = client.drain(); // the spawn packets
    (server, client, player)
}

fn set_position(player: &PlayerRef, x: f32, y: f32, z: f32) {
    let mut c = player.data();
    c.unit.transform.set_xyz(x, y, z);
}

#[test]
fn sphere_area_packets_are_byte_exact() {
    // `FUN_397b63b0`: bool enter, u32 sphereId.
    assert_eq!(
        body(&SCAreaTeamMessagePacket {
            enter: true,
            sphere_id: 0x0102_0304
        }),
        vec![1, 4, 3, 2, 1]
    );
    assert_eq!(
        body(&SCAreaTeamMessagePacket {
            enter: false,
            sphere_id: 933
        }),
        vec![0, 0xA5, 0x03, 0, 0]
    );
    // `FUN_397b6330`: bool enter, bc unitObjId, u32 sphereBubbleId.
    assert_eq!(
        body(&SCAreaChatBubblePacket {
            enter: true,
            unit_obj_id: 0x11,
            bubble_type: 543
        }),
        vec![1, 0x11, 0, 0, 0x1F, 0x02, 0, 0]
    );
    assert_eq!(
        body(&SCAreaChatBubblePacket {
            enter: false,
            unit_obj_id: 0xAB_CDEF,
            bubble_type: 323
        }),
        vec![0, 0xEF, 0xCD, 0xAB, 0x43, 0x01, 0, 0]
    );
}

#[tokio::test]
async fn sphere_area_enter_and_leave_send_once() {
    let (server, mut client, player) = area_server(
        vec![db_sphere(1001, "SphereBubble", 323, true, "")],
        vec![area_sphere(1001, 0.0, 0.0, 0.0, 10.0)],
    );
    // spawn position (100, 100, 100): outside.
    server.systems.quests.spheres.tick(&server);
    assert!(client.drain().is_empty());

    set_position(&player, 0.0, 0.0, 0.0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(
        client.drain(),
        vec![(
            sc::SCAreaChatBubblePacket,
            vec![1, 0xE9, 0x03, 0, 0x43, 0x01, 0, 0]
        )]
    );

    // Staying inside sends nothing more.
    server.systems.quests.spheres.tick(&server);
    assert!(client.drain().is_empty());

    set_position(&player, 100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(
        client.drain(),
        vec![(
            sc::SCAreaChatBubblePacket,
            vec![0, 0xE9, 0x03, 0, 0x43, 0x01, 0, 0]
        )]
    );
    server.systems.quests.spheres.tick(&server);
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn sphere_area_enter_or_leave_controls_the_leave_packet() {
    let (server, mut client, player) = area_server(
        vec![db_sphere(1004, "SphereBubble", 325, false, "")],
        vec![area_sphere(1004, 0.0, 0.0, 0.0, 10.0)],
    );
    set_position(&player, 0.0, 0.0, 0.0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(client.drain().len(), 1);

    set_position(&player, 100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick(&server);
    assert!(client.drain().is_empty(), "enter_or_leave = false");
}

#[tokio::test]
async fn sphere_area_team_message_is_for_non_blank_team_msg_only() {
    let (server, mut client, player) = area_server(
        vec![
            // A non-bubble type with a team message: message only, no bubble.
            db_sphere(1002, "SphereSkill", 0, true, "The aura can be felt."),
            // Blank (whitespace) team message: bubble only.
            db_sphere(1003, "SphereBubble", 324, true, "   "),
        ],
        vec![
            area_sphere(1002, 0.0, 0.0, 0.0, 10.0),
            area_sphere(1003, 0.0, 0.0, 0.0, 10.0),
        ],
    );
    set_position(&player, 0.0, 0.0, 0.0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(
        client.drain(),
        vec![
            (sc::SCAreaTeamMessagePacket, vec![1, 0xEA, 0x03, 0, 0]),
            (
                sc::SCAreaChatBubblePacket,
                vec![1, 0xE9, 0x03, 0, 0x44, 0x01, 0, 0]
            ),
        ]
    );

    set_position(&player, 100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(
        client.drain(),
        vec![
            (sc::SCAreaTeamMessagePacket, vec![0, 0xEA, 0x03, 0, 0]),
            (
                sc::SCAreaChatBubblePacket,
                vec![0, 0xE9, 0x03, 0, 0x44, 0x01, 0, 0]
            ),
        ]
    );
}

#[test]
fn sphere_area_loader_missing_file_is_empty_and_skips_bad_rows() {
    let dir = crate::data::pak::test_util::temp_path("sphere_area_load");
    assert!(load_sphere_areas(&dir, |_| true).is_empty());
    let world = dir.join("Data").join("Worlds").join("main_world");
    std::fs::create_dir_all(&world).unwrap();
    assert!(load_sphere_areas(&dir, |_| true).is_empty());

    std::fs::write(
        world.join("sphere_area_fill.json"),
        r#"[
            {"sphere_id": 7, "zone": 100, "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0},
            {"sphere_id": 8, "zone": 100, "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0},
            {"sphere_id": 9, "zone": 100}
        ]"#,
    )
    .unwrap();
    let areas = load_sphere_areas(&dir, |id| id == 7);
    assert_eq!(areas, vec![area_sphere(7, 1.0, 2.0, 3.0, 4.0)]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_data_sphere_areas_load_the_guarded_1_2_table() {
    let Some(conn) = real_db() else {
        return;
    };
    let known: std::collections::HashSet<u32> = {
        let mut stmt = conn.prepare("SELECT id FROM spheres").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
        rows.map(|r| r.unwrap() as u32).collect()
    };
    let mut types = std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, sphere_detail_type FROM spheres")
            .unwrap();
        for row in stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
        {
            let (id, detail_type) = row.unwrap();
            types.insert(id as u32, detail_type);
        }
    }
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas = load_sphere_areas(&app, |id| known.contains(&id));
    assert_eq!(areas.len(), 2112);
    assert_eq!(
        areas.iter().filter(|a| a.world == "main_world").count(),
        1913
    );
    assert_eq!(
        areas
            .iter()
            .filter(|a| types.get(&a.sphere_id).map(String::as_str) == Some("SphereBubble"))
            .count(),
        430
    );
}

#[tokio::test]
async fn check_timer_registers_and_fails() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            13,
            1,
            ActKind::ObjItemUse {
                item_id: 2,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                drop_when_destroy: false,
            },
        );
        d.add_act(
            132,
            135,
            135,
            0,
            ActKind::CheckTimer {
                limit_time: 60_000,
                force_change_component: false,
                next_component: 0,
                play_skill: false,
                skill_id: 0,
                check_buff: false,
                buff_id: 0,
                sustain_buff: false,
                timer_npc_id: 0,
                is_skill_player: false,
            },
        );
        single_act_quest(
            d,
            14,
            1,
            ActKind::ObjItemUse {
                item_id: 1,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                drop_when_destroy: false,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 13);
        let run = ctx.st.quest(h).unwrap().id;
        assert!(server.systems.quests.has_quest_timer(1, 13));
        assert_eq!(ctx.st.quest(h).unwrap().time_ms, 1_000_000 + 60_000);
        // The quest's row carries the time: LeftTime 60000 at "now"
        assert_eq!(ctx.st.quest(h).unwrap().row().left_time(1_000_000), 60_000);
        // The token timer task may fire a little early on the wall clock (it sleeps on the
        // monotonic clock): the matching run still fails, with no wall-clock guard.
        ctx.now_ms = 1_060_000 - 1;
        ctx.dispatch(&QuestEvent::TimerExpired {
            quest_id: 13,
            run_id: run,
        })
        .unwrap();
        let q = ctx.st.quest(h).unwrap();
        assert_eq!(q.step, kind::FAIL);
        assert_eq!(q.status, status::FAILED);
        // Step change to Fail removed the timer
        assert!(!server.systems.quests.has_quest_timer(1, 13));
        drain(ctx);
        // Failure is immediate even when no authored Fail component exists.
        assert_eq!(ctx.st.quest(h).unwrap().status, status::FAILED);
    });
    // Another quest's end-state step change must not cancel this quest's timer.
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 13);
        let run = ctx.st.quest(h).unwrap().id;
        assert!(server.systems.quests.has_quest_timer(1, 13));
        let other = start(ctx, 14);
        let other_run = ctx.st.quest(other).unwrap().id;
        ctx.dispatch(&QuestEvent::ItemUse { item_id: 1 }).unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, other), kind::READY);
        assert!(server.systems.quests.has_quest_timer(1, 13));
        ctx.dispatch(&QuestEvent::TimerExpired {
            quest_id: 14,
            run_id: other_run,
        })
        .unwrap();
        assert_eq!(step_of(ctx, active(ctx, 13)), kind::PROGRESS);
        assert!(server.systems.quests.has_quest_timer(1, 13));
        ctx.dispatch(&QuestEvent::TimerExpired {
            quest_id: 13,
            run_id: run,
        })
        .unwrap();
        assert_eq!(step_of(ctx, active(ctx, 13)), kind::FAIL);
        assert!(!server.systems.quests.has_quest_timer(1, 13));
    });
}

/// Start (accept from NPC 500 + `CheckTimer`) -> Progress (hunt NPC 600 x2) -> Ready (report
/// to NPC 500). A flagged quest gets `detail_id` 2, the client's restart gate.
fn timed_restart_quest(d: &mut QuestData, id: u32, restart_on_fail: bool, limit_ms: i32) {
    d.add_template(QuestTemplate {
        restart_on_fail,
        detail_id: if restart_on_fail {
            detail::MAIN
        } else {
            detail::NORMAL
        },
        ..quest(id)
    });
    d.add_component(component(id * 10 + 1, id, kind::START));
    d.add_act(
        id * 10 + 1,
        id * 10 + 1,
        id * 10 + 1,
        0,
        ActKind::ConAcceptNpc { npc_id: 500 },
    );
    d.add_act(
        id * 10 + 1,
        id * 10 + 2,
        id * 10 + 2,
        0,
        timer_act(limit_ms),
    );
    d.add_component(component(id * 10 + 2, id, kind::PROGRESS));
    d.add_act(
        id * 10 + 2,
        id * 10 + 3,
        id * 10 + 3,
        2,
        ActKind::ObjMonsterHunt {
            npc_id: 600,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    d.add_component(component(id * 10 + 3, id, kind::READY));
    d.add_act(
        id * 10 + 3,
        id * 10 + 4,
        id * 10 + 4,
        0,
        ActKind::ConReportNpc {
            npc_id: 500,
            use_alias: false,
            alias_id: 0,
        },
    );
}

/// Accepts `quest_id` from NPC 500 through the packet handler and returns the player.
async fn accept_timed_quest(
    server: &GameServer,
    client: &mut TestClient,
    quest_id: u32,
) -> PlayerRef {
    let player = spawn_player(server, client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut giver = crate::models::npc::Npc::new(500);
    giver.unit.transform.set_xyz(105.0, 105.0, 100.0);
    let giver = Entity::new(0x200, giver);
    world.spawn(&giver, &server.view_ctx());
    client.drain();
    let start = CSStartQuestContextPacket {
        quest_context_id: quest_id,
        npc_obj_id: 0x200,
        doodad_obj_id: 0,
        sphere_id: 0,
    };
    use crate::net::registry::ClientPacket;
    start.handle(server, &mut client.conn).await.unwrap();
    server.systems.quests.run_queued_evaluations(server);
    client.drain();
    player
}

/// The status byte of every `SCQuestContextUpdatedPacket` (`Quest.Write`: id i64, template
/// u32, status u8).
fn updated_statuses(frames: &[(u16, Vec<u8>)]) -> Vec<u8> {
    frames
        .iter()
        .filter(|f| f.0 == sc::SCQuestContextUpdatedPacket)
        .map(|f| f.1[12])
        .collect()
}

fn active_row(player: &PlayerRef, quest_id: u32) -> ActiveQuest {
    player
        .data()
        .quests
        .active
        .iter()
        .find(|q| q.template_id == quest_id)
        .cloned()
        .expect("quest row")
}

#[tokio::test]
async fn timed_quest_failure_is_told_and_a_restart_request_restarts_it() {
    let (server, _) = server_with(|d| timed_restart_quest(d, 96, true, 100));
    let mut client = TestClient::new(&server, 1, 1001);
    let player = accept_timed_quest(&server, &mut client, 96).await;
    let before = active_row(&player, 96);
    assert_eq!(
        (before.status, before.step),
        (status::PROGRESS, kind::PROGRESS)
    );
    // Real progress: one of the two hunted NPCs.
    events::on_npc_killed(&server, &player, &timed_hunt_victim(&server));
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();
    assert_eq!(active_row(&player, 96).objectives[0], 1);

    // Timer expiry only fails the quest; the restart is a separate client request.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let run = active_row(&player, 96).id;
    events::on_timer_expired(&server, &player, 96, run);
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert_eq!(
        updated_statuses(&frames).first(),
        Some(&status::FAILED),
        "the client is told about the failure: {frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|f| f.0 == sc::SCQuestContextStartedPacket),
        "server-side failure no longer restarts the quest: {frames:?}"
    );
    assert!(
        !frames.iter().any(|f| f.0 == sc::SCQuestContextFailedPacket),
        "a server-side failure is not a refused report: {frames:?}"
    );
    let failed = active_row(&player, 96);
    assert_eq!((failed.status, failed.step), (status::FAILED, kind::FAIL));
    assert_eq!(failed.objectives[0], 1);
    assert!(!server
        .systems
        .quests
        .has_quest_timer(player.character_id, 96));

    // The client's own restart request (status 2 is not Completed, detail_id 2).
    use crate::net::registry::ClientPacket;
    CSRestartMainQuestPacket { id: 96 }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert!(
        frames
            .iter()
            .any(|f| f.0 == sc::SCQuestContextStartedPacket),
        "the restart is a normal start: {frames:?}"
    );
    assert!(
        updated_statuses(&frames).contains(&status::PROGRESS),
        "the restarted quest runs again: {frames:?}"
    );
    assert!(
        !frames.iter().any(|f| f.0 == sc::SCQuestContextFailedPacket),
        "a restart is not a refused report: {frames:?}"
    );
    let after = active_row(&player, 96);
    assert_eq!(
        (after.status, after.step),
        (status::PROGRESS, kind::PROGRESS)
    );
    assert_eq!(after.objectives, [0; 5], "progress is cleared");
    assert!(
        after.time_ms > failed.time_ms,
        "the timer restarted: {} -> {}",
        failed.time_ms,
        after.time_ms
    );
    assert!(server
        .systems
        .quests
        .has_quest_timer(player.character_id, 96));
    assert!(player.data().quests.removed.contains(&96));
}

/// The NPC 600 the timed test quest hunts.
fn timed_hunt_victim(server: &GameServer) -> crate::world::NpcRef {
    let world = server.world.world(0).unwrap();
    let mut npc = crate::models::npc::Npc::new(600);
    npc.unit.transform.set_xyz(105.0, 105.0, 100.0);
    let npc = Entity::new(0x201, npc);
    world.spawn(&npc, &server.view_ctx());
    npc
}

#[tokio::test]
async fn timed_quest_without_restart_on_fail_stays_failed_and_can_be_dropped() {
    let (server, _) = server_with(|d| timed_restart_quest(d, 97, false, 100));
    let mut client = TestClient::new(&server, 1, 1001);
    let player = accept_timed_quest(&server, &mut client, 97).await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let run = active_row(&player, 97).id;
    events::on_timer_expired(&server, &player, 97, run);
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert_eq!(
        updated_statuses(&frames).first(),
        Some(&status::FAILED),
        "the client is told the quest failed: {frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|f| f.0 == sc::SCQuestContextStartedPacket),
        "a quest without restart_on_fail is not restarted: {frames:?}"
    );
    let failed = active_row(&player, 97);
    assert_eq!((failed.status, failed.step), (status::FAILED, kind::FAIL));

    // A restart request is refused without the flag, with no state change and no packet.
    use crate::net::registry::ClientPacket;
    CSRestartMainQuestPacket { id: 97 }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert!(
        frames.is_empty(),
        "a refused restart changes nothing: {frames:?}"
    );
    let still = active_row(&player, 97);
    assert_eq!((still.status, still.step), (status::FAILED, kind::FAIL));

    // It stays a normal active quest, so the player can drop it.
    CSDropQuestContextPacket { quest_id: 97 }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert!(
        updated_statuses(&frames).contains(&status::DROPPED),
        "the drop is sent to the client: {frames:?}"
    );
    let c = player.data();
    assert!(c.quests.active.is_empty());
    assert!(c.quests.removed.contains(&97));
}

#[tokio::test]
async fn restart_on_fail_restart_survives_save_and_reload() {
    let (server, _) = server_with(|d| timed_restart_quest(d, 98, true, 60_000));
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(98, false, acceptor::NPC, 500));
        drain(ctx);
        let h = active(ctx, 98);
        // Some progress before the failure, to prove it is cleared by the restart.
        ctx.st.quest_mut(h).unwrap().objectives[0] = 1;
        ctx.now_ms = 1_060_000;
        let run = ctx.st.quest(h).unwrap().id;
        ctx.dispatch(&QuestEvent::TimerExpired {
            quest_id: 98,
            run_id: run,
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().status, status::FAILED);
        let before = ctx.st.quest(h).unwrap().time_ms;
        assert!(ctx.try_restart_quest(98));
        drain(ctx);
        let h = active(ctx, 98);
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.status, q.step), (status::PROGRESS, kind::PROGRESS));
        assert_eq!(q.objectives, [0; 5]);
        assert!(q.time_ms > before, "the deadline restarted");
        assert!(server.systems.quests.has_quest_timer(1, 98));
    });
    assert!(c.quests.removed.contains(&98));
    let saved = c.quests.active.clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].template_id, 98);
    assert_eq!(
        (saved[0].status, saved[0].step),
        (status::PROGRESS, kind::PROGRESS)
    );

    // Relogin: one saved row, and the timer resumes from its deadline.
    server.systems.quests.remove_quest_timer(1, 0);
    let mut after = character();
    after.quests.active = saved;
    with_ctx(&server, &mut after, QuestEnv::default(), |ctx| {
        ctx.st.loaded = false;
        ctx.load_from_rows();
        let sent = std::mem::take(&mut ctx.sent);
        assert!(
            sent.is_empty(),
            "a load sends no quest update before SCQuestsPacket: {sent:?}"
        );
        drain(ctx);
        let h = active(ctx, 98);
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.status, q.step), (status::PROGRESS, kind::PROGRESS));
        assert_eq!(q.objectives, [0; 5]);
        assert!(server.systems.quests.has_quest_timer(1, 98));
    });
    assert_eq!(after.quests.active.len(), 1);
    assert_eq!(after.quests.active[0].status, status::PROGRESS);
}

#[tokio::test]
async fn stale_timer_event_from_a_discarded_run_is_ignored() {
    let (server, _) = server_with(|d| timed_restart_quest(d, 94, true, 60_000));
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(94, false, acceptor::NPC, 500));
        drain(ctx);
        let h = active(ctx, 94);
        let old_run = ctx.st.quest(h).unwrap().id;
        ctx.st.quest_mut(h).unwrap().objectives[0] = 1;
        assert!(ctx.try_restart_quest(94));
        drain(ctx);
        let h = active(ctx, 94);
        let new_run = ctx.st.quest(h).unwrap().id;
        assert_ne!(old_run, new_run);
        // A timer armed for the discarded run must not fail the fresh one.
        ctx.dispatch(&QuestEvent::TimerExpired {
            quest_id: 94,
            run_id: old_run,
        })
        .unwrap();
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.status, q.step), (status::PROGRESS, kind::PROGRESS));
        assert_eq!(q.objectives, [0; 5]);
        assert!(server.systems.quests.has_quest_timer(1, 94));
        // The fresh run's own timer still fails it.
        ctx.dispatch(&QuestEvent::TimerExpired {
            quest_id: 94,
            run_id: new_run,
        })
        .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().status, status::FAILED);
        assert!(!server.systems.quests.has_quest_timer(1, 94));
    });
}

#[tokio::test]
async fn arming_a_saved_deadline_beyond_the_limit_clamps_time_ms_to_the_limit() {
    let (server, _) = server_with(|d| timed_restart_quest(d, 93, false, 60_000));
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(93, false, acceptor::NPC, 500));
        drain(ctx);
        let h = active(ctx, 93);
        server.systems.quests.remove_quest_timer(1, 93);
        ctx.st.quest_mut(h).unwrap().time_ms = 1_600_000;
        assert!(ctx.add_quest_timer(h, 60_000));
        assert_eq!(
            ctx.st.quest(h).unwrap().time_ms,
            1_060_000,
            "time_ms must match the clamped remaining time the task sleeps"
        );
    });
}

#[tokio::test]
async fn let_it_done_and_score() {
    let (server, rec) = server_with(|d| {
        d.add_template(QuestTemplate {
            let_it_done: true,
            level: 0,
            ..quest(20)
        });
        d.add_component(component(201, 20, kind::START));
        d.add_act(201, 1, 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(202, 20, kind::PROGRESS));
        d.add_act(
            202,
            2,
            2,
            4,
            ActKind::ObjCraft {
                craft_id: 1,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.add_component(component(203, 20, kind::REWARD));
        d.add_act(203, 3, 3, 0, ActKind::SupplyExp { exp: 10 });
        // Score quest
        d.add_template(QuestTemplate {
            score: 10,
            ..quest(21)
        });
        d.add_component(component(211, 21, kind::START));
        d.add_act(211, 11, 11, 0, ActKind::ConAutoComplete);
        d.add_component(component(212, 21, kind::PROGRESS));
        d.add_act(
            212,
            12,
            12,
            3,
            ActKind::ObjCraft {
                craft_id: 2,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 20);
        // MaxObjective with LetItDone: ceil(4 * 1.5) = 6
        assert_eq!(ctx.objective_status(h), ObjectiveStatus::NotReady);
        ctx.try_complete_quest_as_let_it_done(20, 0);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::Craft { craft_id: 1 }).unwrap();
        }
        drain(ctx);
        // 2 >= 4/2: CanEarlyComplete, LetItDone progress never goes forward by itself
        assert_eq!(ctx.objective_status(h), ObjectiveStatus::CanEarlyComplete);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        assert_eq!(ctx.st.quest(h).unwrap().status, status::PROGRESS);
        for _ in 0..10 {
            ctx.dispatch(&QuestEvent::Craft { craft_id: 1 }).unwrap();
        }
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 6);
        assert_eq!(ctx.objective_status(h), ObjectiveStatus::Overachieved);
        ctx.try_complete_quest_as_let_it_done(20, 0);
        drain(ctx);
        assert!(!ctx.has_quest(20));

        // Score quest: score = objectives * count; MaxObjective = score / count + 1 = 4
        let h = start(ctx, 21);
        for _ in 0..3 {
            ctx.dispatch(&QuestEvent::Craft { craft_id: 2 }).unwrap();
        }
        drain(ctx);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        assert!((ctx.objective_percent(h) - 0.9).abs() < 1e-6);
        ctx.dispatch(&QuestEvent::Craft { craft_id: 2 }).unwrap();
        drain(ctx);
        // 12 >= 10: completed (no Ready / Reward step)
        assert!(!ctx.has_quest(21));
    });
    assert_eq!(rec.take(), vec!["exp 10 true"]);
}

#[tokio::test]
async fn report_doodad_and_complete_quest_acts() {
    let (server, _rec) = server_with(|d| {
        d.add_template(quest(30));
        d.add_component(component(301, 30, kind::START));
        d.add_act(301, 1, 1, 0, ActKind::ConAcceptDoodad { doodad_id: 70 });
        d.add_component(component(302, 30, kind::PROGRESS));
        // (no count column in the DB: with Count 0 the report would be accepted at once)
        d.add_act(
            302,
            2,
            2,
            1,
            ActKind::ObjCompleteQuest {
                quest_id: 31,
                accept_with: false,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.add_component(component(303, 30, kind::READY));
        d.add_act(
            303,
            3,
            3,
            0,
            ActKind::ConReportDoodad {
                doodad_id: 70,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(30, false, acceptor::DOODAD, 70));
        drain(ctx);
        let h = active(ctx, 30);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        // ReportDoodad before the objective is met is ignored
        ctx.dispatch(&QuestEvent::ReportDoodad {
            quest_id: 30,
            doodad_id: 70,
            selected: 0,
        })
        .unwrap();
        drain(ctx);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        ctx.set_completed_quest_flag(31, true);
        ctx.request_evaluation(h);
        drain(ctx);
        assert_eq!(step_of(ctx, h), kind::READY);
        ctx.dispatch(&QuestEvent::ReportDoodad {
            quest_id: 30,
            doodad_id: 70,
            selected: 0,
        })
        .unwrap();
        drain(ctx);
        assert!(!ctx.has_quest(30));
        assert!(ctx.has_quest_completed(30));
        assert!(ctx
            .st
            .subscribers(super::state::EventKind::ReportDoodad)
            .is_empty());
    });
}

#[tokio::test]
async fn selective_rewards_and_mail_when_bag_full() {
    let (server, rec) = server_with(|d| {
        d.add_template(quest(40));
        d.add_component(component(401, 40, kind::START));
        d.add_act(401, 1, 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(402, 40, kind::REWARD));
        d.add_act(
            402,
            2,
            1,
            1,
            ActKind::SupplySelectiveItem {
                item_id: 11,
                grade_id: 0,
            },
        );
        d.add_act(
            402,
            3,
            2,
            2,
            ActKind::SupplySelectiveItem {
                item_id: 12,
                grade_id: 0,
            },
        );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(40, false, 0, 0));
        let h = active(ctx, 40);
        // Start -> None -> Supply -> Progress -> Ready -> Reward: Reward is the first valid
        assert_eq!(step_of(ctx, h), kind::REWARD);
        ctx.st.quest_mut(h).unwrap().selected_reward_index = 2;
        drain(ctx);
        assert!(!ctx.has_quest(40));
    });
    assert_eq!(rec.take(), vec!["item 38 12 2 0"]);
    // Bag full: mail + SCQuestRewardedByMailPacket
    let mut c = character();
    c.inventory = crate::models::character::Inventory::standalone(1, 0, 0);
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(40, false, 0, 0));
        let h = active(ctx, 40);
        ctx.st.quest_mut(h).unwrap().selected_reward_index = 1;
        ops(ctx);
        drain(ctx);
        let sent = std::mem::take(&mut ctx.sent);
        assert!(sent.iter().any(|(op, body)| {
            *op == sc::SCQuestRewardedByMailPacket && body.as_slice() == [1, 40, 0, 0, 0]
        }));
    });
    assert_eq!(rec.take(), vec!["mail 40 1 0"]);
}

#[tokio::test]
async fn reporting_only_completes_the_requested_quest_and_preserves_reward_selection() {
    for doodad in [false, true] {
        let (server, _) = server_with(|d| {
            for qid in [91, 92] {
                single_act_quest(
                    d,
                    qid,
                    1,
                    ActKind::ObjItemUse {
                        item_id: 1,
                        highlight_doodad_id: 0,
                        highlight_doodad_phase: -1,
                        use_alias: false,
                        alias_id: 0,
                        drop_when_destroy: false,
                    },
                );
                if doodad {
                    let a = d.components[&(qid * 10 + 3)].acts[0];
                    d.acts[a].kind = ActKind::ConReportDoodad {
                        doodad_id: 1,
                        use_alias: false,
                        alias_id: 0,
                    };
                }
            }
        });
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            let first = start(ctx, 91);
            let second = start(ctx, 92);
            ctx.dispatch(&QuestEvent::ItemUse { item_id: 1 }).unwrap();
            drain(ctx);
            assert_eq!(step_of(ctx, first), kind::READY);
            assert_eq!(step_of(ctx, second), kind::READY);
            let event = if doodad {
                QuestEvent::ReportDoodad {
                    quest_id: 91,
                    doodad_id: 1,
                    selected: 2,
                }
            } else {
                QuestEvent::ReportNpc {
                    quest_id: 91,
                    npc_id: 1,
                    selected: 2,
                }
            };
            ctx.dispatch(&event).unwrap();
            assert_eq!(ctx.st.quest(first).unwrap().selected_reward_index, 2);
            assert_eq!(ctx.st.quest(second).unwrap().selected_reward_index, 0);
            drain(ctx);
            assert!(ctx.has_quest_completed(91));
            assert!(!ctx.has_quest(91));
            assert!(ctx.has_quest(92));
            assert!(!ctx.has_quest_completed(92));
            assert_eq!(step_of(ctx, second), kind::READY);
        });
    }
}

#[tokio::test]
async fn ready_report_doodad_sends_the_doodad_complete_packet() {
    use crate::systems::npcs::doodad::funcs::use_func;
    use crate::systems::npcs::doodad::runtime::DoodadCtx;
    use crate::systems::npcs::doodad::templates::{DoodadFuncQuest, FuncTemplate};

    let (server, _rec) = server_with(|d| {
        report_quest(
            d,
            100,
            Some(ActKind::ConReportDoodad {
                doodad_id: 700,
                use_alias: false,
                alias_id: 0,
            }),
        );
    });
    // Drive the real `DoodadFuncQuest.Use` -> `QuestsBridge` -> `on_server_report` path.
    server
        .systems
        .npcs
        .hooks
        .quests
        .install(Arc::new(super::bridges::QuestsForNpcs));
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let spawn = |obj_id: u32, template_id: u32| {
        let mut d = crate::models::doodad::Doodad::new(template_id);
        d.transform.set_xyz(105.0, 105.0, 100.0);
        let d = Entity::new(obj_id, d);
        world.spawn(&d, &server.view_ctx());
        d
    };
    let report_doodad = spawn(0x300, 700);
    let other_doodad = spawn(0x302, 701);
    events::add_quest(&server, &player, 100, false);
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();

    let use_quest = |doodad: &crate::world::DoodadRef| {
        let ctx = DoodadCtx::of(&server, doodad).expect("doodad world");
        let caster = WorldObject::from(&player);
        use_func(
            &FuncTemplate::Quest(DoodadFuncQuest {
                id: 1,
                quest_kind_id: 2,
                quest_id: 100,
            }),
            &ctx,
            Some(&caster),
            0,
            0,
        );
    };

    // Objectives still open: the doodad is not a hand-in yet, nothing is sent.
    use_quest(&report_doodad);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(!client
        .drain_ops()
        .contains(&sc::SCDoodadCompleteQuestPacket));

    // Finish the hunt objective: Progress -> Ready.
    events::with_quests(&server, &player, None, |ctx| {
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::MonsterHunt {
                npc_id: 600,
                count: 1,
            })
            .unwrap();
        }
    })
    .expect("not nested");
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].step, kind::READY);
    client.drain();

    // A doodad that is not this quest's report target sends nothing.
    use_quest(&other_doodad);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(!client
        .drain_ops()
        .contains(&sc::SCDoodadCompleteQuestPacket));

    // The report doodad sends the hand-in packet (0x14d): bc object id, then u32 quest id.
    use_quest(&report_doodad);
    let frames = client.drain();
    let complete = frames
        .iter()
        .find(|(op, _)| *op == sc::SCDoodadCompleteQuestPacket)
        .expect("SCDoodadCompleteQuestPacket");
    let mut body = PacketStream::new();
    body.write_bc(0x300).write_u32(100);
    assert_eq!(complete.1, body.into_bytes());
    // The report is left to the client's completion request.
    assert_eq!(player.data().quests.active[0].step, kind::READY);

    // The client's CSCompleteQuestContext with the doodad id finishes the quest.
    let request = CSCompleteQuestContextPacket {
        quest_context_id: 100,
        npc_obj_id: 0,
        doodad_obj_id: 0x300,
        selected: 0,
    };
    request.handle(&server, &mut client.conn).await.unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    assert!(!events::has_quest(&player, 100));
    assert!(events::has_quest_completed_in(&player.data(), 100));
    assert!(client
        .drain_ops()
        .contains(&sc::SCQuestContextCompletedPacket));
}

#[tokio::test]
async fn component_and_sphere_or_requirements_need_a_matching_alternative() {
    use super::unit_reqs::{req_kind, UnitReq};
    let (server, _) = server_with(|d| {
        main_quest(d);
        d.components.get_mut(&1001).unwrap().or_unit_reqs = true;
        for owner_type in ["QuestComponent", "Sphere"] {
            d.unit_reqs
                .by_owner
                .entry(owner_type.into())
                .or_default()
                .insert(
                    1001,
                    vec![
                        UnitReq {
                            kind: req_kind::LEVEL,
                            value1: 20,
                            ..Default::default()
                        },
                        UnitReq {
                            kind: req_kind::MAX_LEVEL,
                            value1: 5,
                            ..Default::default()
                        },
                    ],
                );
        }
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        for (level, expected) in [(10, false), (1, true), (30, true)] {
            ctx.c.unit.level = level;
            assert_eq!(ctx.can_component_run(1001), expected);
            assert_eq!(ctx.can_trigger_sphere(1001, true), expected);
            assert!(!ctx.can_trigger_sphere(1001, false));
        }
        assert!(ctx.can_trigger_sphere(9999, true), "no requirements");
    });
}

#[tokio::test]
async fn first_failure_keeps_the_and_or_order_of_check_all() {
    use super::unit_reqs::{req_kind, UnitReq};
    let or_rows = || {
        vec![
            UnitReq {
                id: 11,
                owner_id: 1001,
                owner_type: "QuestComponent".into(),
                kind: req_kind::LEVEL,
                value1: 20,
                value2: 0,
            },
            UnitReq {
                id: 12,
                owner_id: 1001,
                owner_type: "QuestComponent".into(),
                kind: req_kind::MAX_LEVEL,
                value1: 5,
                value2: 0,
            },
        ]
    };
    let (server, _) = server_with(|d| {
        main_quest(d);
        d.components.get_mut(&1001).unwrap().or_unit_reqs = true;
        d.unit_reqs
            .by_owner
            .entry("QuestComponent".into())
            .or_default()
            .insert(1001, or_rows());
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        ctx.c.unit.level = 10;
        assert!(!ctx.can_component_run(1001));
        assert_eq!(
            ctx.first_failed_component_req(1001).map(|r| r.id),
            Some(11),
            "an OR list reports its first row"
        );
        ctx.c.unit.level = 1;
        assert!(ctx.can_component_run(1001));
        assert_eq!(ctx.first_failed_component_req(1001), None);
        ctx.c.unit.level = 30;
        assert!(ctx.can_component_run(1001));
        assert_eq!(ctx.first_failed_component_req(1001), None);
    });

    // AND: the first row that fails in table order.
    let (server, _) = server_with(|d| {
        main_quest(d);
        d.unit_reqs
            .by_owner
            .entry("QuestComponent".into())
            .or_default()
            .insert(
                1001,
                vec![
                    UnitReq {
                        id: 21,
                        owner_id: 1001,
                        owner_type: "QuestComponent".into(),
                        kind: req_kind::LEVEL,
                        value1: 10,
                        value2: 0,
                    },
                    UnitReq {
                        id: 22,
                        owner_id: 1001,
                        owner_type: "QuestComponent".into(),
                        kind: req_kind::LEVEL,
                        value1: 20,
                        value2: 0,
                    },
                ],
            );
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        ctx.c.unit.level = 5;
        assert_eq!(ctx.first_failed_component_req(1001).map(|r| r.id), Some(21));
        ctx.c.unit.level = 15;
        assert_eq!(ctx.first_failed_component_req(1001).map(|r| r.id), Some(22));
        ctx.c.unit.level = 20;
        assert_eq!(ctx.first_failed_component_req(1001), None);
    });
}

#[tokio::test]
async fn daily_reset_and_item_destroy() {
    let (server, _rec) = server_with(|d| {
        d.add_template(QuestTemplate {
            detail_id: detail::DAILY,
            ..quest(130)
        });
        d.add_template(quest(131));
        main_quest(d);
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        ctx.set_completed_quest_flag(130, true);
        ctx.set_completed_quest_flag(131, true);
        ctx.reset_daily_quests(true);
        assert!(!ctx.has_quest_completed(130));
        assert!(ctx.has_quest_completed(131));
        let sent = std::mem::take(&mut ctx.sent);
        assert_eq!(sent.len(), 1);
        let mut s = PacketStream::new();
        s.write_u32(130)
            .write_raw(&[0x08, 0, 0, 0, 0, 0, 0, 0])
            .write_u32(2); // 131 still set
        assert_eq!(sent[0], (sc::SCQuestContextResetPacket, s.into_bytes()));
        // Destroying the supply item (DropWhenDestroy) of quest 100 drops it
        assert!(ctx.add_quest(100, false, acceptor::NPC, 500));
        drain(ctx);
        ctx.on_quest_item_manually_destroyed(7000, 100);
        assert!(!ctx.has_quest(100));
        // Login reset: leave time yesterday
        ctx.set_completed_quest_flag(130, true);
        ctx.c.leave_time = ctx.now_ms / 1000 - 86_400;
        ctx.check_daily_reset_at_login();
        assert!(!ctx.has_quest_completed(130));
    });
}

#[tokio::test]
async fn load_from_rows_restores_quests() {
    let (server, _rec) = server_with(main_quest);
    let mut c = character();
    let mut row = row();
    row.time_ms = 0;
    row.objectives = [2, 0, 0, 0, 0];
    c.quests.active = vec![
        row.clone(),
        ActiveQuest {
            template_id: 4242, // unknown template: skipped (and not saved again)
            ..row.clone()
        },
    ];
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        ctx.load_from_rows();
        let h = active(ctx, 100);
        let q = ctx.st.quest(h).unwrap();
        assert_eq!(
            (q.step, q.status, q.objectives),
            (kind::PROGRESS, status::PROGRESS, [2, 0, 0, 0, 0])
        );
        assert_eq!((q.component_id, q.acceptor_id), (1003, 500));
        // QuestInitialized + RequestEvaluation queued one evaluation
        assert_eq!(ctx.st.local_queue, vec![h]);
        drain(ctx);
        // objective 2 >= 2: Ready
        assert_eq!(step_of(ctx, h), kind::READY);
    });
    assert_eq!(c.quests.active.len(), 1);
    assert_eq!(c.quests.active[0].step, kind::READY);
}

fn timer_act(limit_time: i32) -> ActKind {
    ActKind::CheckTimer {
        limit_time,
        force_change_component: false,
        next_component: 0,
        play_skill: false,
        skill_id: 0,
        check_buff: false,
        buff_id: 0,
        sustain_buff: false,
        timer_npc_id: 0,
        is_skill_player: false,
    }
}

#[tokio::test]
async fn timed_quest_reload_preserves_remaining_time_and_fails_if_expired_offline() {
    for timer_step in [kind::START, kind::PROGRESS] {
        let (server, _) = server_with(|d| {
            single_act_quest(
                d,
                90,
                2,
                ActKind::ObjMonsterHunt {
                    npc_id: 600,
                    use_alias: false,
                    alias_id: 0,
                    highlight_doodad_id: 0,
                    highlight_doodad_phase: -1,
                },
            );
            d.add_act(
                if timer_step == kind::START { 901 } else { 902 },
                910,
                910,
                0,
                timer_act(60_000),
            );
            d.add_component(component(905, 90, kind::FAIL));
            d.add_act(905, 911, 911, 0, ActKind::ConAutoComplete);
        });
        let mut before = character();
        with_ctx(&server, &mut before, QuestEnv::default(), |ctx| {
            let h = start(ctx, 90);
            ctx.dispatch(&QuestEvent::MonsterHunt {
                npc_id: 600,
                count: 1,
            })
            .unwrap();
            drain(ctx);
            assert_eq!(ctx.st.quest(h).unwrap().time_ms, 1_060_000);
        });
        let mut saved = before.quests.active[0].clone();
        saved.read_data(&saved.write_data()).unwrap();
        assert_eq!(saved.time_ms, 1_060_000);
        for reconnect_time in [1_055_000, 1_070_000] {
            server.systems.quests.remove_quest_timer(1, 0);
            let mut after = character();
            after.quests.active.push(saved.clone());
            with_ctx(&server, &mut after, QuestEnv::default(), |ctx| {
                ctx.now_ms = reconnect_time;
                ctx.st.loaded = false;
                ctx.load_from_rows();
                let sent = std::mem::take(&mut ctx.sent);
                assert!(
                    sent.is_empty(),
                    "a load sends no quest update before SCQuestsPacket: {sent:?}"
                );
                drain(ctx);
                let h = active(ctx, 90);
                let q = ctx.st.quest(h).unwrap();
                assert_eq!(q.objectives[0], 1);
                if reconnect_time < saved.time_ms {
                    assert_eq!(q.time_ms, saved.time_ms);
                    assert_eq!(q.row().left_time(ctx.now_ms), 5000);
                    assert_eq!(q.step, kind::PROGRESS);
                    assert!(server.systems.quests.has_quest_timer(1, 90));
                } else {
                    // The expired deadline is clamped to "now" when the timer is re-armed.
                    assert_eq!(q.time_ms, reconnect_time);
                    assert_eq!(q.step, kind::FAIL);
                    assert_eq!(q.status, status::FAILED);
                    assert!(!server.systems.quests.has_quest_timer(1, 90));
                }
            });
        }
    }
}

// ----- flow test through the packet handlers and the event API -----

fn decode(frames: Vec<(u16, Vec<u8>)>) -> Vec<u16> {
    frames.into_iter().map(|f| f.0).collect()
}

/// `SCQuestContextFailedPacket` body: quest id, then the one-byte error.
fn failed_bytes(quest_id: u32, err: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(quest_id).write_u8(err);
    s.into_bytes()
}

/// The refused report answered exactly once, with `quest_id` and `err` and nothing else.
fn assert_one_failed(client: &mut TestClient, quest_id: u32, err: u8) {
    let frames = client.drain();
    assert_eq!(
        decode(frames.clone()),
        vec![sc::SCQuestContextFailedPacket],
        "expected exactly one SCQuestContextFailed, got {frames:?}"
    );
    assert_eq!(frames[0].1, failed_bytes(quest_id, err));
}

#[tokio::test]
async fn item_group_quests_track_inventory_use_and_cleanup_only_matching_items() {
    let (server, _) = server_with(|d| {
        d.group_items.add(7, 7001);
        d.group_items.add(7, 7002);
        d.group_items.add(7, 7001); // Duplicate metadata must not double the inventory.
        for id in [100, 200] {
            d.add_template(quest(id));
            d.add_component(component(id * 10 + 1, id, kind::START));
            d.add_act(id * 10 + 1, id, id, 0, ActKind::ConAutoComplete);
            d.add_component(component(id * 10 + 3, id, kind::PROGRESS));
            d.add_component(component(id * 10 + 4, id, kind::READY));
            d.add_act(
                id * 10 + 4,
                id + 2,
                id + 2,
                0,
                ActKind::ConReportNpc {
                    npc_id: 500,
                    use_alias: false,
                    alias_id: 0,
                },
            );
        }
        d.add_act(
            1003,
            101,
            1,
            3,
            ActKind::ObjItemGroupGather {
                item_group_id: 7,
                cleanup: true,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                drop_when_destroy: false,
                destroy_when_drop: true,
            },
        );
        d.add_act(
            2003,
            201,
            1,
            2,
            ActKind::ObjItemGroupUse {
                item_group_id: 7,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                drop_when_destroy: false,
            },
        );
    });
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    give_item(&mut player.data(), 7001, 1);
    give_item(&mut player.data(), 9999, 20);
    events::add_quest(&server, &player, 100, false);
    events::add_quest(&server, &player, 200, false);
    server.systems.quests.run_queued_evaluations(&server);
    let objective = |id| {
        player
            .data()
            .quests
            .active
            .iter()
            .find(|q| q.template_id == id)
            .unwrap()
            .objectives[0]
    };
    assert_eq!(objective(100), 1);
    events::on_items_acquired(&server, &player, 9999, 20);
    assert_eq!(objective(100), 1);
    give_item(&mut player.data(), 7002, 1);
    events::on_items_acquired(&server, &player, 7002, 1);
    assert_eq!(objective(100), 2);
    take_items(&mut player.data(), 7001, 1);
    events::on_items_consumed(&server, &player, 7001, 1);
    assert_eq!(objective(100), 1);
    give_item(&mut player.data(), 7001, 2);
    events::on_items_acquired(&server, &player, 7001, 2);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(objective(100), 3);
    assert_eq!(
        player
            .data()
            .quests
            .active
            .iter()
            .find(|q| q.template_id == 100)
            .unwrap()
            .step,
        kind::READY
    );

    events::on_item_use(&server, &player, 9999);
    assert_eq!(objective(200), 0);
    events::on_item_use(&server, &player, 7001);
    assert_eq!(objective(200), 1);
    events::on_item_use(&server, &player, 7002);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(objective(200), 2);
    // Abandoning the gather quest destroys only its required group quantity.
    events::drop_quest(&server, &player, 100, true);
    assert_eq!(super::engine::items_count(&player.data(), 7001), 0);
    assert_eq!(super::engine::items_count(&player.data(), 7002), 0);
    assert_eq!(super::engine::items_count(&player.data(), 9999), 20);
    assert!(!events::has_quest(&player, 100));
    assert!(events::has_quest(&player, 200));
}

#[tokio::test]
async fn quest_starter_item_uses_its_binding_and_item_acceptor() {
    let (server, _) = server_with(|d| {
        d.add_template(quest(100));
        d.add_component(component(1001, 100, kind::START));
        d.add_act(
            1001,
            1,
            1,
            0,
            ActKind::ConAcceptItem {
                item_id: 19572,
                cleanup: false,
                drop_when_destroy: false,
                destroy_when_drop: false,
            },
        );
        d.add_component(component(1003, 100, kind::PROGRESS));
        d.add_act(
            1003,
            2,
            1,
            2,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.starter_quests.insert(19572, 100);
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    give_item(&mut player.data(), 19572, 1);
    client.drain();

    // The item's binding overrides the effect's quest id.
    events::accept_quest_effect(&server, &player, 999_999, Some((19572, true)));
    assert!(events::has_quest(&player, 100));
    assert!(!events::has_quest(&player, 999_999));
    let frames = client.drain();
    let started = frames
        .iter()
        .find(|(op, _)| *op == sc::SCQuestContextStartedPacket)
        .unwrap();
    let b = &started.1;
    assert_eq!(&b[8..12], &100u32.to_le_bytes());
    assert_eq!(b[b.len() - 9], acceptor::ITEM);
    assert_eq!(&b[b.len() - 8..b.len() - 4], &19572u32.to_le_bytes());

    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].step, kind::PROGRESS);
    // Using the starter twice cannot duplicate the active quest.
    events::accept_quest_effect(&server, &player, 999_999, Some((19572, true)));
    assert_eq!(player.data().quests.active.len(), 1);
}

#[tokio::test]
async fn npc_quest_flow_through_packets_and_events() {
    let (server, rec) = server_with(main_quest);
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut giver = crate::models::npc::Npc::new(500);
    giver.unit.transform.set_xyz(105.0, 105.0, 100.0);
    let giver = Entity::new(0x200, giver);
    world.spawn(&giver, &server.view_ctx());
    let mut wolf = crate::models::npc::Npc::new(600);
    wolf.unit.transform.set_xyz(110.0, 110.0, 100.0);
    let wolf = Entity::new(0x201, wolf);
    world.spawn(&wolf, &server.view_ctx());
    client.drain();

    // Accept the quest at the NPC (CSStartQuestContextPacket with the NPC's obj id)
    let start = CSStartQuestContextPacket {
        quest_context_id: 100,
        npc_obj_id: 0x200,
        doodad_obj_id: 0,
        sphere_id: 0,
    };
    use crate::net::registry::ClientPacket;
    start.handle(&server, &mut client.conn).await.unwrap();
    assert_eq!(player.data().unit.current_target, 0x200);
    // Look away: while NPC 500 is targeted the quest is turned in as soon as it is Ready
    player.data().unit.current_target = 0;
    let frames = client.drain();
    assert_eq!(
        decode(frames.clone()),
        vec![
            sc::SCQuestContextStartedPacket,
            sc::SCQuestContextUpdatedPacket
        ]
    );
    // SCQuestContextStartedPacket: quest id 1 (QuestIdManager), template 100, status Invalid,
    // acceptor NPC 500, component 1001
    let b = &frames[0].1;
    assert_eq!(&b[..8], &1i64.to_le_bytes());
    assert_eq!(&b[8..12], &100u32.to_le_bytes());
    assert_eq!(b[12], status::INVALID);
    assert_eq!(b[b.len() - 9], acceptor::NPC);
    assert_eq!(&b[b.len() - 8..b.len() - 4], &500u32.to_le_bytes());
    assert_eq!(&b[b.len() - 4..], &1001u32.to_le_bytes());

    // QuestManagerRunQueueTask
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(rec.take(), vec!["item 38 7000 1 0"]);
    assert_eq!(player.data().quests.active[0].step, kind::PROGRESS);
    client.drain();

    // A forged object-free report cannot skip the unfinished kill objective.
    let without_object = CSCompleteQuestContextPacket {
        quest_context_id: 100,
        npc_obj_id: 0,
        doodad_obj_id: 0,
        selected: 0,
    };
    without_object
        .clone()
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].step, kind::PROGRESS);
    assert!(rec.take().is_empty());
    // The refused report is answered so the client can leave directing mode.
    let frames = client.drain();
    assert_eq!(decode(frames.clone()), vec![sc::SCQuestContextFailedPacket]);
    let mut failed = PacketStream::new();
    failed.write_u32(100).write_u8(12);
    assert_eq!(frames[0].1, failed.into_bytes());

    // Kill objective through the event API
    events::on_npc_killed(&server, &player, &wolf);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 1);
    events::on_npc_killed(&server, &player, &wolf);
    server.systems.quests.run_queued_evaluations(&server);
    {
        let c = player.data();
        assert_eq!(c.quests.active[0].objectives[0], 2);
        assert_eq!(c.quests.active[0].step, kind::READY);
        assert_eq!(c.quests.active[0].status, status::READY);
    }
    let ops = client.drain_ops();
    assert!(ops.iter().all(|o| *o == sc::SCQuestContextUpdatedPacket));

    // Ready still requires its NPC report act; omitting that object grants nothing.
    without_object
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].step, kind::READY);
    assert!(rec.take().is_empty());
    let frames = client.drain();
    assert_eq!(decode(frames.clone()), vec![sc::SCQuestContextFailedPacket]);
    let mut failed = PacketStream::new();
    failed.write_u32(100).write_u8(12);
    assert_eq!(frames[0].1, failed.into_bytes());

    // Turn in at the NPC (CSCompleteQuestContextPacket)
    let complete = CSCompleteQuestContextPacket {
        quest_context_id: 100,
        npc_obj_id: 0x200,
        doodad_obj_id: 0,
        selected: 0,
    };
    complete.handle(&server, &mut client.conn).await.unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    // Rewards hooks invoked (the Reward step's exp / copper acts replace the level supplies)
    assert_eq!(
        rec.take(),
        vec![
            "exp 300 true",
            "item 38 9000 1 1",
            "money 50",
            "consume 39 7000 1"
        ]
    );
    let frames = client.drain();
    let last = frames.last().unwrap();
    assert_eq!(last.0, sc::SCQuestContextCompletedPacket);
    let mut s = PacketStream::new();
    s.write_u32(100)
        .write_raw(&[0, 0, 0, 0, 0x10, 0, 0, 0])
        .write_u32(0);
    assert_eq!(last.1, s.into_bytes());
    let c = player.data();
    assert!(c.quests.active.is_empty());
    assert_eq!(c.quests.removed, vec![100]);
    assert!(super::engine::has_quest_completed(&c.quests.completed, 100));
    // Only the Reward acts: 300 exp and 50 copper, no level supplies on top.
    assert_eq!(c.experience, 300);
    assert_eq!(c.money, 50);
    assert_eq!(server.systems.quests.queued_count(), 0);
}

/// Quest `id` whose Start component accepts anything and needs the owner to be of `level`
/// (`QuestComponent` `unit_reqs`).
fn level_gated_start(d: &mut QuestData, id: u32, component_id: u32, level: u32) {
    use super::unit_reqs::{req_kind, UnitReq};
    d.add_template(quest(id));
    d.add_component(component(component_id, id, kind::START));
    d.add_act(component_id, id, 1, 0, ActKind::ConAutoComplete);
    d.unit_reqs
        .by_owner
        .entry("QuestComponent".into())
        .or_default()
        .insert(
            component_id,
            vec![UnitReq {
                id: component_id,
                owner_id: component_id,
                owner_type: "QuestComponent".into(),
                kind: req_kind::LEVEL,
                value1: level,
                value2: 0,
            }],
        );
}

#[tokio::test]
async fn start_quest_context_reports_the_failed_level_requirement() {
    let (server, _) = server_with(|d| level_gated_start(d, 300, 3001, 10));
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    player.data().unit.level = 5;
    let start = CSStartQuestContextPacket {
        quest_context_id: 300,
        npc_obj_id: 0,
        doodad_obj_id: 0,
        sphere_id: 0,
    };

    start
        .clone()
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert_eq!(
        decode(frames.clone()),
        vec![sc::SCQuestUnitReqFailedPacket],
        "a player-initiated refusal is answered: {frames:?}"
    );
    // Reader FUN_397b5d00 / bag FUN_397b1ef0: quest, bc unit object id, flags 0x05 (kind and
    // value set), then kind LEVEL (1) and the row's value1 (10).
    let mut expected = PacketStream::new();
    expected
        .write_u32(300)
        .write_bc(0x100)
        .write_u8(0x05)
        .write_u8(1)
        .write_u32(10);
    assert_eq!(frames[0].1, expected.into_bytes());
    assert!(!events::has_quest(&player, 300));

    // The same request at the required level starts the quest and sends no failure.
    player.data().unit.level = 10;
    client.drain();
    start.handle(&server, &mut client.conn).await.unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert!(
        !frames.iter().any(|f| f.0 == sc::SCQuestUnitReqFailedPacket),
        "an accepted start must not report a requirement failure: {frames:?}"
    );
    assert!(frames
        .iter()
        .any(|f| f.0 == sc::SCQuestContextStartedPacket));
}

#[tokio::test]
async fn automatic_level_up_start_does_not_report_a_requirement_failure() {
    use super::unit_reqs::{req_kind, UnitReq};
    let (server, _) = server_with(|d| {
        d.add_template(quest(301));
        d.add_component(component(3011, 301, kind::START));
        // The level-up act that makes `on_level_up` try the quest at level 10.
        d.add_act(3011, 301, 1, 0, ActKind::ConAcceptLevelUp { level: 10 });
        d.unit_reqs
            .by_owner
            .entry("QuestComponent".into())
            .or_default()
            .insert(
                3011,
                vec![UnitReq {
                    id: 3011,
                    owner_id: 3011,
                    owner_type: "QuestComponent".into(),
                    kind: req_kind::LEVEL,
                    value1: 20,
                    value2: 0,
                }],
            );
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    player.data().unit.level = 10;
    client.conn.active_char = Some(player.clone());

    events::on_level_up(&server, &player);
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    assert!(
        !frames.iter().any(|f| f.0 == sc::SCQuestUnitReqFailedPacket),
        "an automatic starter must stay silent: {frames:?}"
    );
    assert!(!events::has_quest(&player, 301));
}
fn report_quest(d: &mut QuestData, id: u32, report: Option<ActKind>) {
    d.add_template(quest(id));
    d.add_component(component(id * 10 + 1, id, kind::START));
    d.add_act(
        id * 10 + 1,
        id * 10 + 1,
        id * 10 + 1,
        0,
        ActKind::ConAutoComplete,
    );
    d.add_component(component(id * 10 + 2, id, kind::PROGRESS));
    d.add_act(
        id * 10 + 2,
        id * 10 + 2,
        id * 10 + 2,
        2,
        ActKind::ObjMonsterHunt {
            npc_id: 600,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    d.add_component(component(id * 10 + 3, id, kind::READY));
    if let Some(k) = report {
        d.add_act(id * 10 + 3, id * 10 + 3, id * 10 + 3, 0, k);
    } else {
        d.add_act(
            id * 10 + 3,
            id * 10 + 3,
            id * 10 + 3,
            0,
            ActKind::ConAutoComplete,
        );
    }
    d.add_component(component(id * 10 + 4, id, kind::REWARD));
    d.add_act(
        id * 10 + 4,
        id * 10 + 4,
        id * 10 + 4,
        0,
        ActKind::SupplyCopper { amount: 10 },
    );
}

#[tokio::test]
async fn refused_reports_answer_with_sc_quest_context_failed() {
    let (server, _rec) = server_with(|d| {
        report_quest(
            d,
            50,
            Some(ActKind::ConReportNpc {
                npc_id: 500,
                use_alias: false,
                alias_id: 0,
            }),
        );
        report_quest(
            d,
            52,
            Some(ActKind::ConReportDoodad {
                doodad_id: 700,
                use_alias: false,
                alias_id: 0,
            }),
        );
        // Active quest with no NPC report act: the turn-in cannot reach any act.
        report_quest(d, 53, None);
        // Quest 51 has two report NPCs; the act that does not match must not refuse a
        // turn-in the other act accepts.
        d.add_template(quest(51));
        d.add_component(component(511, 51, kind::START));
        d.add_act(511, 511, 511, 0, ActKind::ConAutoComplete);
        d.add_component(component(512, 51, kind::PROGRESS));
        d.add_act(
            512,
            512,
            512,
            2,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.add_component(component(513, 51, kind::READY));
        d.add_act(
            513,
            513,
            513,
            0,
            ActKind::ConReportNpc {
                npc_id: 500,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.add_act(
            513,
            514,
            514,
            0,
            ActKind::ConReportNpc {
                npc_id: 501,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.add_component(component(515, 51, kind::REWARD));
        d.add_act(515, 515, 515, 0, ActKind::SupplyCopper { amount: 10 });
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let spawn = |obj_id: u32, data: crate::models::npc::Npc| {
        let mut data = data;
        data.unit.transform.set_xyz(105.0, 105.0, 100.0);
        world.spawn(Entity::new(obj_id, data), &server.view_ctx());
    };
    spawn(0x200, crate::models::npc::Npc::new(500));
    spawn(0x202, crate::models::npc::Npc::new(501));
    let mut right_doodad = crate::models::doodad::Doodad::new(700);
    right_doodad.transform.set_xyz(105.0, 105.0, 100.0);
    let right_doodad = Entity::new(0x300, right_doodad);
    world.spawn(&right_doodad, &server.view_ctx());
    let mut wrong_doodad = crate::models::doodad::Doodad::new(701);
    wrong_doodad.transform.set_xyz(105.0, 105.0, 100.0);
    let wrong_doodad = Entity::new(0x302, wrong_doodad);
    world.spawn(&wrong_doodad, &server.view_ctx());
    for id in [50, 51, 52, 53] {
        events::add_quest(&server, &player, id, false);
    }
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();

    // NPC object not in the character's world.
    events::on_report(&server, &player, 50, 0x999, 0, 0);
    assert_one_failed(
        &mut client,
        50,
        super::packets::quest_error::INVALID_NPC_OR_QUEST,
    );

    // Another NPC than the quest's report NPC.
    events::on_report(&server, &player, 50, 0x202, 0, 0);
    assert_one_failed(
        &mut client,
        50,
        super::packets::quest_error::INVALID_NPC_OR_QUEST,
    );

    // The right NPC, objectives not complete (the live quest 1065 refusal).
    events::on_report(&server, &player, 50, 0x200, 0, 0);
    assert_one_failed(
        &mut client,
        50,
        super::packets::quest_error::INVALID_QUEST_STATUS,
    );

    // A quest the character does not have.
    events::on_report(&server, &player, 999, 0x200, 0, 0);
    assert_one_failed(&mut client, 999, super::packets::quest_error::INVALID_QUEST);

    // An active quest that has no NPC report act at all.
    events::on_report(&server, &player, 53, 0x200, 0, 0);
    assert_one_failed(
        &mut client,
        53,
        super::packets::quest_error::INVALID_NPC_OR_QUEST,
    );

    // Doodad object not in the character's world.
    events::on_report(&server, &player, 52, 0, 0x999, 0);
    assert_one_failed(&mut client, 52, super::packets::quest_error::INVALID_DOODAD);

    // Another doodad than the quest's report doodad.
    events::on_report(&server, &player, 52, 0, 0x302, 0);
    assert_one_failed(&mut client, 52, super::packets::quest_error::INVALID_DOODAD);

    // The right doodad, objectives not complete.
    events::on_report(&server, &player, 52, 0, 0x300, 0);
    assert_one_failed(
        &mut client,
        52,
        super::packets::quest_error::INVALID_QUEST_STATUS,
    );

    // A server-side report (DoodadFuncQuest, a doodad used on an owned quest) did not put
    // the client in directing mode: its refusals are only logged, never answered.
    for doodad_obj_id in [0x300, 0x302, 0x999] {
        events::on_server_report(&server, &player, 52, 0, doodad_obj_id, 0);
        server.systems.quests.run_queued_evaluations(&server);
        let ops = client.drain_ops();
        assert!(
            !ops.contains(&sc::SCQuestContextFailedPacket),
            "a server-side report {doodad_obj_id:#x} was answered with a refusal: {ops:?}"
        );
    }
    // The flag is scoped to the server report: a client report right after is answered.
    events::on_report(&server, &player, 52, 0, 0x300, 0);
    assert_one_failed(
        &mut client,
        52,
        super::packets::quest_error::INVALID_QUEST_STATUS,
    );

    // A successful report still completes (0x14b) and is not answered with 0x146.
    events::with_quests(&server, &player, None, |ctx| {
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::MonsterHunt {
                npc_id: 600,
                count: 1,
            })
            .unwrap();
        }
    })
    .expect("not nested");
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();
    events::on_report(&server, &player, 50, 0x200, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    let ops = client.drain_ops();
    assert!(
        ops.contains(&sc::SCQuestContextCompletedPacket),
        "a successful report did not complete the quest: {ops:?}"
    );
    assert!(
        !ops.contains(&sc::SCQuestContextFailedPacket),
        "a successful report was answered with a refusal: {ops:?}"
    );

    // A quest with two report NPCs: reporting to the second one must not be answered with
    // a refusal by the first act (which does not match).
    events::with_quests(&server, &player, None, |ctx| {
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::MonsterHunt {
                npc_id: 600,
                count: 1,
            })
            .unwrap();
        }
    })
    .expect("not nested");
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();
    events::on_report(&server, &player, 51, 0x202, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    let ops = client.drain_ops();
    assert!(
        ops.contains(&sc::SCQuestContextCompletedPacket),
        "the matching report NPC did not complete the quest: {ops:?}"
    );
    assert!(
        !ops.contains(&sc::SCQuestContextFailedPacket),
        "a mismatching report act refused an accepted turn-in: {ops:?}"
    );
}

#[tokio::test]
async fn refused_early_completion_answers_with_sc_quest_context_failed() {
    let (server, _rec) = server_with(|d| {
        report_quest(d, 60, Some(ActKind::ConReportJournal));
        // 60 allows early completion; 61 does not.
        if let Some(t) = d.templates.get_mut(&60) {
            t.let_it_done = true;
        }
        report_quest(d, 61, Some(ActKind::ConReportJournal));
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    for id in [60, 61] {
        events::add_quest(&server, &player, id, false);
    }
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();

    // The client's report target is not the NPC it named.
    player.data().unit.current_target = 0x999;
    CSTryQuestCompleteAsLetItDonePacket {
        id: 60,
        obj_id: 0x200,
        selected: 0,
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert_one_failed(
        &mut client,
        60,
        super::packets::quest_error::INVALID_NPC_OR_QUEST,
    );
    player.data().unit.current_target = 0;

    // LetItDone quest whose objectives are not far enough.
    CSTryQuestCompleteAsLetItDonePacket {
        id: 60,
        obj_id: 0,
        selected: 0,
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert_one_failed(
        &mut client,
        60,
        super::packets::quest_error::INVALID_QUEST_STATUS,
    );

    // Quest that cannot be completed early at all.
    CSTryQuestCompleteAsLetItDonePacket {
        id: 61,
        obj_id: 0,
        selected: 0,
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert_one_failed(
        &mut client,
        61,
        super::packets::quest_error::INVALID_QUEST_STATUS,
    );

    // Quest that is not active.
    CSTryQuestCompleteAsLetItDonePacket {
        id: 999,
        obj_id: 0,
        selected: 0,
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert_one_failed(&mut client, 999, super::packets::quest_error::INVALID_QUEST);
}

#[tokio::test]
async fn let_it_done_without_reward_answers_when_ready_does_not_complete() {
    let (server, _rec) = server_with(|d| {
        d.add_template(QuestTemplate {
            let_it_done: true,
            ..quest(70)
        });
        d.add_component(component(701, 70, kind::START));
        d.add_act(701, 701, 701, 0, ActKind::ConAutoComplete);
        d.add_component(component(702, 70, kind::PROGRESS));
        d.add_act(
            702,
            702,
            702,
            2,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        // The report act is overridden by TryCompleteQuestAsLetItDone, but the craft act in
        // the other Ready component keeps the step from completing.
        d.add_component(component(703, 70, kind::READY));
        d.add_act(
            703,
            703,
            703,
            0,
            ActKind::ConReportNpc {
                npc_id: 500,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.add_component(component(704, 70, kind::READY));
        d.add_act(
            704,
            704,
            704,
            99,
            ActKind::ObjCraft {
                craft_id: 70,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        // No Reward component: the early completion has to finish at Ready.
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    events::add_quest(&server, &player, 70, false);
    server.systems.quests.run_queued_evaluations(&server);
    events::with_quests(&server, &player, None, |ctx| {
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::MonsterHunt {
                npc_id: 600,
                count: 1,
            })
            .unwrap();
        }
    })
    .expect("not nested");
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();

    CSTryQuestCompleteAsLetItDonePacket {
        id: 70,
        obj_id: 0,
        selected: 0,
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    let frames = client.drain();
    let failed: Vec<_> = frames
        .iter()
        .filter(|f| f.0 == sc::SCQuestContextFailedPacket)
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "expected exactly one SCQuestContextFailed, got {frames:?}"
    );
    assert_eq!(
        failed[0].1,
        failed_bytes(70, super::packets::quest_error::INVALID_QUEST_STATUS)
    );
    assert_eq!(
        player.data().quests.active[0].step,
        kind::READY,
        "the quest completed instead of staying at Ready"
    );
}

#[tokio::test]
async fn completed_cinema_resends_neighbours_before_advancing_its_objective() {
    use crate::net::registry::ClientPacket;
    let (server, _) = server_with(|d| {
        single_act_quest(
            d,
            8,
            0,
            ActKind::ObjCinema {
                cinema_id: 44,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let mut npc = crate::models::npc::Npc::new(777);
    npc.unit.transform.set_xyz(103.0, 100.0, 100.0);
    let npc = Entity::new(0x200, npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    events::add_quest(&server, &player, 8, false);
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();
    CSStartedCinemaPacket
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert_eq!(player.data().quest_state.currently_playing_cinema_id, 44);
    CSCompletedCinemaPacket
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    let frames = client.drain();
    for opcode in [sc::SCUnitStatePacket, sc::SCUnitPointsPacket] {
        assert!(frames.iter().any(|(op, body)| {
            *op == opcode && PacketStream::from_bytes(body.clone()).read_bc().unwrap() == npc.obj_id
        }));
    }
    assert_eq!(player.data().quest_state.currently_playing_cinema_id, 0);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].step, kind::READY);
    assert_eq!(player.data().quests.active[0].objectives[0], 1);
    client.drain();
    client.conn.active_char = None;
    CSCompletedCinemaPacket
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn object_free_quest_report_preserves_journal_and_validated_early_completion() {
    let (server, rec) = server_with(|d| {
        for id in [800, 801] {
            d.add_template(QuestTemplate {
                let_it_done: id == 800,
                ..quest(id)
            });
            d.add_component(component(id * 10 + 1, id, kind::START));
            d.add_act(id * 10 + 1, id, id, 0, ActKind::ConAutoComplete);
            d.add_component(component(id * 10 + 2, id, kind::PROGRESS));
            d.add_act(
                id * 10 + 2,
                id + 10,
                id,
                if id == 800 { 4 } else { 2 },
                ActKind::ObjCraft {
                    craft_id: id,
                    use_alias: false,
                    alias_id: 0,
                    highlight_doodad_id: 0,
                    highlight_doodad_phase: -1,
                },
            );
            if id == 801 {
                d.add_component(component(id * 10 + 3, id, kind::READY));
                d.add_act(id * 10 + 3, id + 20, id, 0, ActKind::ConReportJournal);
            }
            d.add_component(component(id * 10 + 4, id, kind::REWARD));
            d.add_act(
                id * 10 + 4,
                id + 30,
                id,
                0,
                ActKind::SupplyCopper { amount: 10 },
            );
        }
    });
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    for id in [800, 801] {
        events::add_quest(&server, &player, id, false);
    }
    server.systems.quests.run_queued_evaluations(&server);
    events::on_report(&server, &player, 800, 0, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(rec.take().is_empty());
    assert!(events::has_quest(&player, 800));
    events::with_quests(&server, &player, None, |ctx| {
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::Craft { craft_id: 800 }).unwrap();
            ctx.dispatch(&QuestEvent::Craft { craft_id: 801 }).unwrap();
        }
        let early = ctx.st.active_handle(800).unwrap();
        assert_eq!(
            ctx.objective_status(early),
            ObjectiveStatus::CanEarlyComplete
        );
        let journal = ctx.st.active_handle(801).unwrap();
        ctx.run_current_step(journal);
        assert_eq!(ctx.st.quest(journal).unwrap().step, kind::READY);
    });
    events::on_report(&server, &player, 800, 0, 0, 0);
    events::on_report(&server, &player, 801, 0, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(!events::has_quest(&player, 800));
    assert!(!events::has_quest(&player, 801));
    assert_eq!(rec.take(), vec!["money 10", "money 10"]);
    events::on_report(&server, &player, 800, 0, 0, 0);
    events::on_report(&server, &player, 801, 0, 0, 0);
    assert!(rec.take().is_empty());
}

#[tokio::test]
async fn talk_quest_flow_and_nested_item_events() {
    let (server, rec) = server_with(|d| {
        d.add_template(quest(300));
        d.add_component(component(3001, 300, kind::START));
        d.add_act(3001, 1, 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(3002, 300, kind::PROGRESS));
        d.add_act(
            3002,
            2,
            1,
            0,
            ActKind::ObjTalk {
                npc_id: 500,
                team_share: false,
                item_id: 0,
                use_alias: false,
                alias_id: 0,
            },
        );
        d.add_component(component(3003, 300, kind::REWARD));
        d.add_act(
            3003,
            3,
            1,
            1,
            ActKind::SupplyItem {
                item_id: 8000,
                grade_id: 0,
                show_action_bar: false,
                cleanup: false,
                drop_when_destroy: false,
                destroy_when_drop: false,
            },
        );
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut n = crate::models::npc::Npc::new(500);
    n.unit.transform.set_xyz(105.0, 105.0, 100.0);
    world.spawn(Entity::new(0x200, n), &server.view_ctx());
    client.drain();

    assert_eq!(events::add_quest(&server, &player, 300, false), Some(true));
    server.systems.quests.run_queued_evaluations(&server);
    let talk = CSQuestTalkMadePacket {
        npc_obj_id: 0x200,
        quest_context_id: 300,
        quest_comp_id: 3002,
        quest_act_id: 2,
    };
    use crate::net::registry::ClientPacket;
    talk.handle(&server, &mut client.conn).await.unwrap();
    server.systems.quests.run_queued_evaluations(&server);
    assert!(player.data().quests.active.is_empty());
    assert_eq!(rec.take(), vec!["item 38 8000 1 0"]);
    // A hook raising a quest event for the same character is queued (no deadlock)
    struct Nested;
    impl QuestHooks for Nested {
        fn acquire_default_item(
            &self,
            server: &GameServer,
            player: Option<&PlayerRef>,
            _: &mut Character,
            _: u8,
            t: u32,
            n: i32,
            _: u8,
        ) -> bool {
            events::on_items_acquired(server, player.unwrap(), t, n);
            true
        }
    }
    server.systems.quests.hooks.install(Arc::new(Nested));
    assert_eq!(events::add_quest(&server, &player, 300, false), Some(false)); // completed
    let r = events::with_quests(&server, &player, None, |ctx| {
        ctx.hooks.clone().acquire_default_item(
            ctx.server,
            ctx.player.clone().as_ref(),
            ctx.c,
            0,
            1,
            1,
            0,
        )
    });
    assert_eq!(r, Some(true));
}

#[tokio::test]
async fn select_hook_loads_rows_and_sends() {
    let (server, _rec) = server_with(main_quest);
    let client = TestClient::new(&server, 1, 1001);
    let mut c = character();
    let mut r = row();
    r.time_ms = 0;
    c.quests.active = vec![r];
    let player = spawn_player(&server, &client, c, 0x100);
    super::on_character_selected(&server, &player);
    assert!(player.data().quest_state.loaded);
    assert!(events::has_quest(&player, 100));
    assert_eq!(server.systems.quests.queued_count(), 1);
    super::on_leave_world(&server, 1);
}

/// Waits for the 1 ms `QuestManagerRunQueueTask` (and anything it spawned) to run.
async fn let_queue_run() {
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
}

#[tokio::test]
async fn select_evaluations_wait_for_notify_in_game_completed() {
    let (server, _rec) = server_with(main_quest);
    // The quest system must be able to schedule `QuestManagerRunQueueTask` (set by `start`).
    let server = Arc::new(server);
    let _ = server.systems.quests.server.set(Arc::downgrade(&server));
    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = character();
    let mut r = row();
    r.time_ms = 0;
    c.quests.active = vec![r];
    let player = spawn_player(&server, &client, c, 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();

    // CSSelectCharacter -> Quests.Load requests the evaluation of the progress quest.
    super::on_character_selected(&server, &player);
    assert_eq!(server.systems.quests.deferred_count(), 1);
    let_queue_run().await;
    // Nothing reaches the client before it is in the world.
    assert_eq!(server.systems.quests.deferred_count(), 1);
    assert!(
        !client
            .drain_ops()
            .contains(&sc::SCQuestContextUpdatedPacket),
        "SCQuestContextUpdated sent before CSNotifyInGameCompleted"
    );
    // A second request for the same quest does not duplicate the parked entry.
    server
        .systems
        .quests
        .enqueue_evaluation(&player, server.systems.quests.queued_handles(1)[0]);
    assert_eq!(server.systems.quests.deferred_count(), 1);

    // CSNotifyInGameCompleted releases it.
    crate::world::session::notify_in_game_completed(&server, &mut client.conn);
    assert_eq!(server.systems.quests.deferred_count(), 0);
    let_queue_run().await;
    assert_eq!(server.systems.quests.queued_count(), 0);
    assert!(
        client
            .drain_ops()
            .contains(&sc::SCQuestContextUpdatedPacket),
        "the deferred evaluation must run after CSNotifyInGameCompleted"
    );

    // A later NotifyInGameCompleted (world switch) neither re-parks nor loses evaluations.
    let h = server.systems.quests.queued_handles(1);
    assert!(h.is_empty());
    let h = events::with_quests(&server, &player, None, |ctx| ctx.new_quest(100, 1)).unwrap();
    server.systems.quests.enqueue_evaluation(&player, h);
    assert_eq!(server.systems.quests.deferred_count(), 0);
    crate::world::session::notify_in_game_completed(&server, &mut client.conn);
    let_queue_run().await;
    assert_eq!(server.systems.quests.queued_count(), 0);
    super::on_leave_world(&server, 1);
}

#[tokio::test]
async fn leaving_before_in_game_drops_deferred_evaluations() {
    let (server, _rec) = server_with(main_quest);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = character();
    let mut r = row();
    r.time_ms = 0;
    c.quests.active = vec![r];
    let player = spawn_player(&server, &client, c, 0x100);
    client.drain();
    super::on_character_selected(&server, &player);
    assert_eq!(server.systems.quests.deferred_count(), 1);
    super::on_leave_world(&server, 1);
    assert_eq!(server.systems.quests.queued_count(), 0);
    // A completion that arrives afterwards has nothing to run.
    super::on_in_game_completed(&server, 1);
    let_queue_run().await;
    assert!(!client
        .drain_ops()
        .contains(&sc::SCQuestContextUpdatedPacket));
}

// ----- real compact.sqlite3 -----

pub(super) fn real_db() -> Option<rusqlite::Connection> {
    let path = std::env::var("AAEMU_COMPACT_DB").ok()?;
    Some(
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open compact db"),
    )
}

fn real_data(conn: &rusqlite::Connection) -> QuestData {
    let files = crate::data::client_files::ClientFileManager::new();
    QuestData::load_from(conn, &files).expect("quest tables load without errors")
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_quest_tables() {
    let Some(conn) = real_db() else {
        return;
    };
    let d = real_data(&conn);
    assert_eq!(d.starter_quests.len(), 367);
    // These are live progress acts in 1.2, not orphan detail rows.
    for (quest_id, class) in [
        (5490, "QuestActObjItemGroupGather"),
        (5489, "QuestActObjItemGroupUse"),
    ] {
        let act = d
            .acts
            .iter()
            .find(|a| a.quest_id == quest_id && a.kind.class_name() == class)
            .unwrap();
        assert!(
            act.objective_index < 5,
            "{class} must have a wire objective slot"
        );
    }
    let count =
        |sql: &str| -> usize { conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize };
    assert_eq!(
        d.templates.len(),
        count("SELECT COUNT(*) FROM quest_contexts WHERE category_id <> 45")
    );
    assert_eq!(
        d.components.len(),
        count("SELECT COUNT(*) FROM quest_components c JOIN quest_contexts q ON q.id = c.quest_context_id WHERE q.category_id <> 45")
    );
    assert_eq!(
        d.base_acts.len(),
        count("SELECT COUNT(*) FROM quest_acts a JOIN quest_components c ON c.id = a.quest_component_id JOIN quest_contexts q ON q.id = c.quest_context_id WHERE q.category_id <> 45")
    );
    assert_eq!(
        d.supplies.len(),
        count("SELECT COUNT(*) FROM quest_supplies")
    );
    assert_eq!(
        d.group_items.rows(),
        count("SELECT COUNT(*) FROM quest_item_group_items")
    );
    assert_eq!(
        d.group_npcs.rows(),
        count("SELECT COUNT(*) FROM quest_monster_npcs")
    );
    assert_eq!(d.unit_reqs.rows, count("SELECT COUNT(*) FROM unit_reqs"));
    assert_eq!(d.spheres.len(), count("SELECT COUNT(*) FROM spheres"));
    assert_eq!(
        d.sphere_quests.len(),
        count("SELECT COUNT(*) FROM sphere_quests")
    );
    // Values from the 1.2 compact.sqlite3 this port was validated against
    assert_eq!(d.templates.len(), 4775);
    assert_eq!(d.components.len(), 17720);
    assert_eq!(d.base_acts.len(), 19047);
    assert_eq!(d.acts.len(), 19047);
    let n = |c: &str| d.acts_by_type.get(c).map_or(0, Vec::len);
    for (class, expected) in [
        ("QuestActCheckCompleteComponent", 10),
        ("QuestActCheckGuard", 4),
        ("QuestActCheckSphere", 1),
        ("QuestActCheckTimer", 44),
        ("QuestActConAcceptComponent", 316),
        ("QuestActConAcceptDoodad", 203),
        ("QuestActConAcceptItem", 340),
        ("QuestActConAcceptItemGain", 23),
        ("QuestActConAcceptLevelUp", 3),
        ("QuestActConAcceptNpc", 3072),
        ("QuestActConAcceptNpcKill", 1055),
        ("QuestActConAcceptSphere", 421),
        ("QuestActConAutoComplete", 1086),
        ("QuestActConReportDoodad", 15),
        ("QuestActConReportJournal", 467),
        ("QuestActConReportNpc", 2828),
        ("QuestActEtcItemObtain", 66),
        ("QuestActObjAbilityLevel", 11),
        ("QuestActObjAggro", 30),
        ("QuestActObjCinema", 12),
        ("QuestActObjCompleteQuest", 53),
        ("QuestActObjCraft", 173),
        ("QuestActObjExpressFire", 48),
        ("QuestActObjInteraction", 289),
        ("QuestActObjItemGather", 1492),
        ("QuestActObjItemGroupGather", 4),
        ("QuestActObjItemGroupUse", 1),
        ("QuestActObjItemUse", 243),
        ("QuestActObjLevel", 1),
        ("QuestActObjMateLevel", 6),
        ("QuestActObjMonsterGroupHunt", 528),
        ("QuestActObjMonsterHunt", 589),
        ("QuestActObjSphere", 125),
        ("QuestActObjTalk", 131),
        ("QuestActObjTalkNpcGroup", 2),
        ("QuestActObjZoneKill", 110),
        ("QuestActSupplyAppellation", 218),
        ("QuestActSupplyCopper", 1474),
        ("QuestActSupplyCrimePoint", 7),
        ("QuestActSupplyExp", 1100),
        ("QuestActSupplyHonorPoint", 46),
        ("QuestActSupplyItem", 1928),
        ("QuestActSupplyJuryPoint", 4),
        ("QuestActSupplyLivingPoint", 12),
        ("QuestActSupplyLp", 5),
        ("QuestActSupplyRemoveItem", 13),
        ("QuestActSupplySelectiveItem", 438),
    ] {
        assert_eq!(n(class), expected, "{class}");
    }
    assert_eq!(
        (d.supplies.len(), d.group_items.rows(), d.group_npcs.rows()),
        (55, 158, 2771)
    );
    assert_eq!(
        (d.unit_reqs.rows, d.spheres.len(), d.sphere_quests.len()),
        (13354, 1908, 1079)
    );
    // Every component's acts point back to it; objective indices fit `Objectives[5]`
    let mut over = 0;
    for (cid, c) in &d.components {
        for &a in &c.acts {
            assert_eq!(d.acts[a].component_id, *cid);
            assert_eq!(d.acts[a].quest_id, c.quest_id);
            if d.acts[a].objective_index != 0xFF && d.acts[a].objective_index >= 5 {
                over += 1;
            }
        }
    }
    eprintln!(
        "quests {} components {} acts {} (objective index >= 5: {over}) supplies {} item groups {} npc groups {} unit_reqs {} spheres {} sphere_quests {}",
        d.templates.len(),
        d.components.len(),
        d.acts.len(),
        d.supplies.len(),
        d.group_items.len(),
        d.group_npcs.len(),
        d.unit_reqs.rows,
        d.spheres.len(),
        d.sphere_quests.len()
    );
    // Every quest can be instantiated and started on a fresh character
    let mut gd = GameData::empty();
    gd.systems.quests = d;
    let server = test_server_with(gd);
    let ids: Vec<u32> = server.game_data.systems.quests.template_order.clone();
    let mut started = 0;
    for qid in ids {
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            let h = ctx.new_quest(qid, 1);
            if ctx.start_quest(h) {
                started += 1;
            }
        });
    }
    eprintln!("quests with a Start step: {started}");
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_restart_on_fail_and_successive_flags() {
    let Some(conn) = real_db() else {
        return;
    };
    let d = real_data(&conn);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM quest_contexts WHERE restart_on_fail='t'"),
        222
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM quest_contexts WHERE successive='t'"),
        37
    );
    assert_eq!(
        d.templates.values().filter(|t| t.restart_on_fail).count(),
        222
    );
    assert_eq!(d.templates.values().filter(|t| t.successive).count(), 37);
    assert!(d.template(213).unwrap().restart_on_fail);
    assert!(d.template(80).unwrap().successive);
    // Quest 64 is the level-3 timed quest used by the failure live check.
    assert!(!d.template(64).unwrap().restart_on_fail);
    // QUEST-1: no real restart_on_fail quest can fail server-side.
    assert_eq!(
        count(
            "SELECT COUNT(DISTINCT q.id) FROM quest_contexts q              JOIN quest_components c ON c.quest_context_id=q.id              JOIN quest_acts a ON a.quest_component_id=c.id              WHERE q.restart_on_fail='t' AND (a.act_detail_type IN              ('QuestActCheckTimer','QuestActCheckGuard','QuestActConFail')              OR c.component_kind_id=5)"
        ),
        0
    );
    // 211 of the 222 flagged quests are detail_id 2, the client's restart gate.
    assert_eq!(
        count("SELECT COUNT(*) FROM quest_contexts WHERE restart_on_fail='t' AND detail_id=2"),
        211
    );
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_report_doodads_are_ready_doodad_func_quests() {
    let Some(conn) = real_db() else {
        return;
    };
    let d = real_data(&conn);
    let mut checked = 0;
    for a in &d.acts {
        let ActKind::ConReportDoodad { doodad_id, .. } = a.kind else {
            continue;
        };
        // Every live report doodad act of 1.2 sits on the Ready step.
        assert_eq!(
            a.component_kind,
            kind::READY,
            "quest {} act {}: the report doodad is not on the Ready step",
            a.quest_id,
            a.act_id
        );
        // The doodad offers the quest through `DoodadFuncQuest`; that use is the one
        // `SCDoodadCompleteQuestPacket` is sent from.
        let funcs: i64 = conn
            .query_row(
                r"SELECT COUNT(*) FROM doodad_funcs f
                  JOIN doodad_func_groups g ON g.id = f.doodad_func_group_id
                  JOIN doodad_func_quests q ON q.id = f.actual_func_id
                  WHERE g.doodad_almighty_id = ?1
                    AND f.actual_func_type = 'DoodadFuncQuest' AND q.quest_id = ?2",
                [doodad_id, a.quest_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            funcs > 0,
            "quest {}: doodad {doodad_id} has no DoodadFuncQuest",
            a.quest_id
        );
        checked += 1;
    }
    assert_eq!(checked, 15, "live QuestActConReportDoodad acts");
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_reward_acts_replace_level_supplies() {
    let Some(conn) = real_db() else {
        return;
    };
    let d = real_data(&conn);
    // The two oracle examples: an act XP that differs from the level supply.
    for (quest_id, act_exp, supply_exp) in [(2785u32, 8400, 3224), (1213, 110, 420)] {
        let t = d.template(quest_id).expect("quest template");
        let supply = d.supplies(t.level).expect("level supply");
        assert_eq!(supply.exp, supply_exp, "quest {quest_id} supply exp");
        let acts: i32 = t
            .components_of(&d.components, kind::REWARD)
            .iter()
            .flat_map(|c| c.acts.iter())
            .map(|&a| match d.acts[a].kind {
                ActKind::SupplyExp { exp } => exp,
                _ => 0,
            })
            .sum();
        assert_eq!(acts, act_exp, "quest {quest_id} Reward SupplyExp");
    }

    let mut gd = GameData::empty();
    gd.systems.quests = d;
    let server = test_server_with(gd);
    let rec = Arc::new(Rec::default());
    server.systems.quests.hooks.install(rec.clone());
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        for (quest_id, act_exp, supply_exp, supply_copper) in
            [(2785u32, 8400, 3224, 1786), (1213, 110, 420, 33)]
        {
            let h = ctx.new_quest(quest_id, 1);
            ctx.set_step(h, kind::REWARD);
            rec.take();
            ctx.run_current_step(h);
            let calls = rec.take();
            assert!(
                calls.contains(&format!("exp {act_exp} true")),
                "quest {quest_id}: act XP missing in {calls:?}"
            );
            assert!(
                !calls.contains(&format!("exp {supply_exp} true")),
                "quest {quest_id}: level supply XP must not be added on top: {calls:?}"
            );
            assert!(
                calls.contains(&format!("money {supply_copper}")),
                "quest {quest_id}: level supply copper missing in {calls:?}"
            );
        }
    });
}

// ----- AAEmu.IntegrationTests/Models/Game/Quests/QuestTests.cs -----

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB; optional AAEMU_GAME_PAK and AAEMU_QUEST_SPHERE_AUDIT"]
async fn real_compact_db_quest_accounting() {
    let Some(conn) = real_db() else {
        return;
    };
    let mut files = crate::data::client_files::ClientFileManager::new();
    if let Ok(path) = std::env::var("AAEMU_GAME_PAK") {
        assert!(files.add_source(&path, std::path::Path::new(".")));
    }
    let d = QuestData::load_from(&conn, &files).expect("load real quest data");
    let mut stmt = conn
        .prepare("SELECT id, category_id FROM quest_contexts ORDER BY id")
        .unwrap();
    for row in stmt
        .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))
        .unwrap()
    {
        let (id, category) = row.unwrap();
        assert_eq!(
            d.template(id).is_some(),
            category != super::data::QUEST_CATEGORY_TUTORIAL,
            "quest {id}"
        );
    }
    // An aggregate count alone can hide one missing act and one extra act.
    for base in &d.base_acts {
        let act = d
            .act_template(base.detail_id, &base.detail_type)
            .expect("every base act has a loaded detail");
        assert_eq!(act.act_id, base.id);
        assert_eq!(act.component_id, base.component_id);
        assert!(act.objective_index == 0xFF || act.objective_index < 5);
    }
    let mut no_start = Vec::new();
    for &id in &d.template_order {
        let quest = &d.templates[&id];
        if quest.first_component(&d.components, kind::START).is_none() {
            no_start.push(id);
        }
        for &cid in &quest.components {
            assert_eq!(d.components[&cid].quest_id, id);
            for &a in &d.components[&cid].acts {
                assert_eq!(d.acts[a].quest_id, id);
            }
        }
    }
    let sphere_acts: Vec<_> = d
        .acts
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                ActKind::CheckSphere { .. }
                    | ActKind::ObjSphere { .. }
                    | ActKind::ConAcceptSphere { .. }
            )
        })
        .collect();
    let missing_sphere_components: Vec<_> = sphere_acts.iter().filter(|a| {
        !d.sign_spheres.iter().any(|s| s.component_id == a.component_id && s.quest_id == a.quest_id)
    }).map(|a| serde_json::json!({"quest_id": a.quest_id, "component_id": a.component_id, "class": a.kind.class_name()})).collect();
    if !files.sources().is_empty() {
        assert!(
            !d.sign_spheres.is_empty(),
            "client pack must contain quest sign spheres"
        );
    }
    let raw: Vec<_> = d.sign_spheres.iter().map(|s| serde_json::json!({
        "quest_id": s.quest_id, "component_id": s.component_id, "zone_id": s.zone_id,
        "local": s.local, "radius": s.radius,
        "loaded_component": d.components.contains_key(&s.component_id),
        "matching_component": d.component(s.component_id).is_some_and(|c| c.quest_id == s.quest_id),
    })).collect();
    let summary = serde_json::json!({
        "loaded_quests": d.templates.len(), "no_start_ids": no_start,
        "loaded_acts": d.acts.len(), "sphere_act_count": sphere_acts.len(),
        "client_sources": files.sources().len(), "sign_sphere_count": raw.len(),
        "missing_sphere_components": missing_sphere_components,
    });
    eprintln!("quest accounting: {summary}");
    if let Ok(path) = std::env::var("AAEMU_QUEST_SPHERE_AUDIT") {
        let artifact = serde_json::json!({"summary": summary, "sign_spheres": raw});
        std::fs::write(path, serde_json::to_vec_pretty(&artifact).unwrap()).unwrap();
    }
}

/// `GetQuestIdsWithComponentKindContainingActDetailType(params QuestCondition[])`.
fn quest_ids_with(
    conn: &rusqlite::Connection,
    conds: &[(u8, &str, Option<bool>, Option<bool>)],
) -> Vec<u32> {
    if conds.is_empty() {
        return Vec::new();
    }
    let mut from = String::new();
    let mut wh = String::new();
    for (i, (k, t, has_skill, has_buff)) in conds.iter().enumerate() {
        let n = i + 1;
        from.push_str(&format!(
            " inner join quest_components qc{n} on qc{n}.quest_context_id = qc.quest_context_id inner join quest_acts qa{n} on qc{n}.id = qa{n}.quest_component_id"
        ));
        wh.push_str(&format!(
            " and qc{n}.component_kind_id = {k} and qa{n}.act_detail_type = '{t}'"
        ));
        if let Some(s) = has_skill {
            wh.push_str(&format!(
                " and qc{n}.skill_id {} 0",
                if *s { ">" } else { "=" }
            ));
        }
        if let Some(b) = has_buff {
            wh.push_str(&format!(
                " and qc{n}.buff_id {} 0",
                if *b { ">" } else { "=" }
            ));
        }
    }
    let sql = format!(
        "select DISTINCT qc.quest_context_id from quest_components qc {from} where qc.id == qc1.id {wh} order by qc.quest_context_id"
    );
    let mut st = conn.prepare(&sql).unwrap();
    st.query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|v| v.unwrap() as u32)
        .collect()
}

/// `RemoveSphereAndNotImplementedQuests`. Also drops the ids whose `quest_contexts` row does
/// not exist (orphan components, e.g. 5765): the C# builds `new Quest(null, ..)` for them and
/// `StartQuest` returns false, so the C# tests would fail on them as well.
fn remove_sphere_and_not_implemented(conn: &rusqlite::Connection, ids: Vec<u32>) -> Vec<u32> {
    let mut ex = quest_ids_with(conn, &[(kind::PROGRESS, "QuestActObjSphere", None, None)]);
    ex.extend(quest_ids_with(
        conn,
        &[(kind::START, "QuestActConAcceptComponent", None, None)],
    ));
    let exists = |id: u32| {
        conn.query_row(
            "SELECT COUNT(*) FROM quest_contexts WHERE id = ? AND category_id <> 45",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    };
    ids.into_iter()
        .filter(|i| !ex.contains(i) && exists(*i))
        .collect()
}

/// `SetupQuest`: a fresh character + the quest object (constructor only).
fn setup_quest<R>(
    server: &GameServer,
    qid: u32,
    f: impl FnOnce(&mut Ctx<'_>, QuestHandle) -> R,
) -> R {
    let mut c = character();
    c.id = 1 + qid; // random character id in C#
    with_ctx(server, &mut c, QuestEnv::default(), |ctx| {
        let h = ctx.new_quest(qid, 0);
        f(ctx, h)
    })
}

fn legacy_enabled() -> bool {
    std::env::var("AAEMU_QUEST_LEGACY_TESTS").is_ok()
}

fn real_server() -> Option<(GameServer, rusqlite::Connection)> {
    let conn = real_db()?;
    let mut gd = GameData::empty();
    gd.systems.quests = real_data(&conn);
    Some((test_server_with(gd), conn))
}

/// `Start_WhenQuestStart_AllActsAreQuestActConAcceptNpc_And_TargetNpcIsNotValid_ShouldNotStartQuest`.
/// Skipped in C# too: `StartQuest` only checks for a Start step, so it returns true here.
#[tokio::test]
#[ignore = "skipped in C# (quest system rewrite): StartQuest no longer checks the acceptor"]
async fn start_all_acts_con_accept_npc_target_not_valid_should_not_start() {
    if !legacy_enabled() {
        return;
    }
    let Some((server, conn)) = real_server() else {
        return;
    };
    let mut st = conn
        .prepare("select qc.quest_context_id, qa.act_detail_type, qaNot.act_detail_type from quest_contexts qcx inner join quest_components qc on qcx.id = qc.quest_context_id inner join quest_acts qa on qc.id = qa.quest_component_id and qa.act_detail_type = 'QuestActConAcceptNpc' left join quest_acts qaNot on qc.id = qaNot.quest_component_id and qaNot.act_detail_type <> 'QuestActConAcceptNpc' where qc.component_kind_id = 2 and qaNot.act_detail_type is null group by qc.quest_context_id, qa.act_detail_type order by quest_context_id")
        .unwrap();
    let ids: Vec<u32> = st
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|v| v.unwrap() as u32)
        .collect();
    for qid in ids.into_iter().take(500) {
        setup_quest(&server, qid, |ctx, h| {
            assert!(!ctx.start_quest(h), "quest {qid}");
            assert!(ctx
                .sent
                .iter()
                .all(|p| p.0 != sc::SCQuestContextStartedPacket));
            assert!(!ctx
                .deferred
                .iter()
                .any(|d| matches!(d, super::engine::Deferred::UseSkillSelf { .. })));
        });
    }
}

/// `Start_DoodadQuestsSupplyingItem_WhenBagIsNotFull_ShouldAddToCharacterBag`. Skipped in C#:
/// `StartQuest` does not run the Supply step, so nothing is added.
#[tokio::test]
#[ignore = "skipped in C# (quest system rewrite): StartQuest does not supply items"]
async fn start_doodad_quests_supplying_item_bag_not_full_should_add_to_bag() {
    if !legacy_enabled() {
        return;
    }
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(
        &conn,
        &[
            (kind::START, "QuestActConAcceptDoodad", None, None),
            (kind::SUPPLY, "QuestActSupplyItem", None, None),
        ],
    );
    for qid in remove_sphere_and_not_implemented(&conn, ids) {
        setup_quest(&server, qid, |ctx, h| {
            let r = ctx.start_quest(h);
            let data = ctx.data;
            let t = data.template(qid).unwrap();
            if let Some(c) = t.first_component(&data.components, kind::SUPPLY) {
                if let Some(&a) = c
                    .acts
                    .iter()
                    .find(|&&a| matches!(data.acts[a].kind, ActKind::SupplyItem { .. }))
                {
                    let ActKind::SupplyItem { item_id, .. } = data.acts[a].kind else {
                        unreachable!()
                    };
                    assert_eq!(
                        data.acts[a].count,
                        super::engine::items_count(ctx.c, item_id),
                        "quest {qid}"
                    );
                }
            }
            assert!(r);
        });
    }
}

/// `Start_DoodadQuestsSupplyingItem_WhenBagAndBackIsFull_ShouldAddToCharacterBag`. Skipped in
/// C#: `StartQuest` returns true whenever there is a Start step.
#[tokio::test]
#[ignore = "skipped in C# (quest system rewrite): StartQuest does not check the bag"]
async fn start_doodad_quests_supplying_item_bag_and_back_full_should_not_start() {
    if !legacy_enabled() {
        return;
    }
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(
        &conn,
        &[
            (kind::START, "QuestActConAcceptDoodad", None, None),
            (kind::SUPPLY, "QuestActSupplyItem", None, None),
        ],
    );
    for qid in remove_sphere_and_not_implemented(&conn, ids) {
        let mut c = character();
        c.inventory = crate::models::character::Inventory::standalone(1, 0, 0);
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            let h = ctx.new_quest(qid, 0);
            assert!(!ctx.start_quest(h), "quest {qid}");
            assert!(ctx.sent.iter().any(|p| p.0 == sc::SCErrorMsgPacket));
        });
    }
}

/// `Start_ActCheckTimer_ShouldStartSchedulerAndSendCharacterMessage` (enabled: passes).
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn start_act_check_timer_should_start_scheduler() {
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(&conn, &[(kind::START, "QuestActCheckTimer", None, None)]);
    let ids = remove_sphere_and_not_implemented(&conn, ids);
    assert!(!ids.is_empty());
    for qid in ids {
        let char_id = 1 + qid;
        setup_quest(&server, qid, |ctx, h| {
            let data = ctx.data;
            if let Some(a) = data
                .template(qid)
                .and_then(|t| t.first_component(&data.components, kind::START))
                .and_then(|c| {
                    c.acts
                        .iter()
                        .copied()
                        .find(|&a| matches!(data.acts[a].kind, ActKind::ConAcceptNpc { .. }))
                })
            {
                let ActKind::ConAcceptNpc { npc_id } = data.acts[a].kind else {
                    unreachable!()
                };
                ctx.env.target = Some(UnitSnapshot {
                    npc_template_id: Some(npc_id),
                    is_unit: true,
                    ..UnitSnapshot::default()
                });
            }
            let r = ctx.start_quest(h);
            let has_timer = data
                .template(qid)
                .and_then(|t| t.first_component(&data.components, kind::START))
                .is_some_and(|c| {
                    c.acts
                        .iter()
                        .any(|&a| matches!(data.acts[a].kind, ActKind::CheckTimer { .. }))
                });
            if has_timer {
                assert!(
                    server.systems.quests.has_quest_timer(char_id, qid),
                    "quest {qid}"
                );
            }
            assert!(r, "quest {qid}");
        });
    }
}

/// `Start_ActConAcceptNpcWhenTargetingCorrectNpc_ShouldStartQuestSuccessfully` (enabled).
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn start_act_con_accept_npc_targeting_correct_npc_should_start() {
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(&conn, &[(kind::START, "QuestActConAcceptNpc", None, None)]);
    let ids = remove_sphere_and_not_implemented(&conn, ids);
    assert!(ids.len() > 1000);
    for qid in ids {
        let char_id = 1 + qid;
        setup_quest(&server, qid, |ctx, h| {
            let data = ctx.data;
            let start = data
                .template(qid)
                .and_then(|t| t.first_component(&data.components, kind::START));
            if let Some(a) = start.and_then(|c| {
                c.acts
                    .iter()
                    .copied()
                    .find(|&a| matches!(data.acts[a].kind, ActKind::ConAcceptNpc { .. }))
            }) {
                let ActKind::ConAcceptNpc { npc_id } = data.acts[a].kind else {
                    unreachable!()
                };
                ctx.env.target = Some(UnitSnapshot {
                    npc_template_id: Some(npc_id),
                    is_unit: true,
                    ..UnitSnapshot::default()
                });
            }
            let r = ctx.start_quest(h);
            if start.is_some_and(|c| {
                c.acts
                    .iter()
                    .any(|&a| matches!(data.acts[a].kind, ActKind::CheckTimer { .. }))
            }) {
                assert!(
                    server.systems.quests.has_quest_timer(char_id, qid),
                    "quest {qid}"
                );
            }
            assert!(r, "quest {qid}");
        });
    }
}

/// `UseSkillAndBuff_MockedWorldManager_WhenQuestUseSkill_ShouldUseOnSelfOrTargetNpc`. Skipped
/// in C#: `StartQuest` neither sets the acceptor nor runs the Start components.
#[tokio::test]
#[ignore = "skipped in C# (quest system rewrite): StartQuest does not run the components"]
async fn use_skill_and_buff_mocked_world_should_use_on_self_or_npc() {
    if !legacy_enabled() {
        return;
    }
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(
        &conn,
        &[(kind::START, "QuestActConAcceptNpc", Some(true), None)],
    );
    for qid in remove_sphere_and_not_implemented(&conn, ids) {
        setup_quest(&server, qid, |ctx, h| {
            assert!(ctx.start_quest(h));
            let q = ctx.st.quest(h).unwrap();
            assert_eq!(q.acceptor_type, acceptor::NPC, "quest {qid}");
            assert!(!ctx.deferred.is_empty(), "quest {qid}: skill used");
        });
    }
}

/// `UseSkillAndBuff_WhenQuestUseSkill_ShouldUseOnSelfOrTargetNpc` (skipped in C#, as above).
#[tokio::test]
#[ignore = "skipped in C# (quest system rewrite): StartQuest does not run the components"]
async fn use_skill_and_buff_should_use_on_self_or_npc() {
    if !legacy_enabled() {
        return;
    }
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(
        &conn,
        &[(kind::START, "QuestActConAcceptNpc", Some(true), None)],
    );
    for qid in remove_sphere_and_not_implemented(&conn, ids) {
        setup_quest(&server, qid, |ctx, h| {
            assert!(ctx.start_quest(h));
            assert_eq!(
                ctx.st.quest(h).unwrap().acceptor_type,
                acceptor::NPC,
                "quest {qid}"
            );
        });
    }
}

/// `UseSkillAndBuff_WhenQuestUseBuff_ShouldUseOnSelfOrTargetNpc` (skipped in C#: `StartQuest`
/// does not run `UseBuff`).
#[tokio::test]
#[ignore = "skipped in C# (quest system rewrite): StartQuest does not run the components"]
async fn use_skill_and_buff_when_quest_use_buff() {
    if !legacy_enabled() {
        return;
    }
    let Some((server, conn)) = real_server() else {
        return;
    };
    let ids = quest_ids_with(
        &conn,
        &[(kind::START, "QuestActConAcceptNpc", None, Some(true))],
    );
    for qid in remove_sphere_and_not_implemented(&conn, ids) {
        setup_quest(&server, qid, |ctx, h| {
            assert!(ctx.start_quest(h));
            assert!(ctx.buffs_changed, "quest {qid}: buff added");
        });
    }
}

#[tokio::test]
async fn invalid_starters_leave_no_active_quest_or_subscriptions() {
    let (server, _) = server_with(main_quest);
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        for (source, id) in [
            (acceptor::NPC, 501),
            (acceptor::ITEM, 500),
            (acceptor::UNKNOWN, 0),
        ] {
            assert!(!ctx.add_quest(100, false, source, id));
            assert!(!ctx.has_quest(100));
            assert!(ctx.st.quests.is_empty());
        }
        assert!(ctx.add_quest(100, false, acceptor::NPC, 500));
    });
}

#[tokio::test]
async fn escort_failure_only_follows_the_guarded_npc_death() {
    let (server, _) = server_with(|d| {
        single_act_quest(
            d,
            91,
            2,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.add_act(912, 913, 913, 0, ActKind::CheckGuard { npc_id: 700 });
        d.add_component(component(915, 91, kind::FAIL));
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 91);
        ctx.dispatch(&QuestEvent::GuardNpcDied { npc_id: 701 })
            .unwrap();
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        ctx.dispatch(&QuestEvent::GuardNpcDied { npc_id: 700 })
            .unwrap();
        assert_eq!(ctx.st.quest(h).unwrap().status, status::FAILED);
        assert_eq!(step_of(ctx, h), kind::FAIL);
        assert!(ctx
            .st
            .subscribers(super::state::EventKind::GuardNpcDied)
            .is_empty());
    });
}

#[tokio::test]
async fn guard_death_fails_without_restart_or_sc_quest_context_failed() {
    let (server, _) = server_with(|d| {
        d.add_template(QuestTemplate {
            restart_on_fail: true,
            detail_id: detail::MAIN,
            ..quest(92)
        });
        d.add_component(component(921, 92, kind::START));
        d.add_act(921, 921, 921, 0, ActKind::ConAutoComplete);
        d.add_component(component(922, 92, kind::PROGRESS));
        d.add_act(
            922,
            922,
            922,
            2,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.add_act(922, 923, 923, 0, ActKind::CheckGuard { npc_id: 700 });
        d.add_component(component(924, 92, kind::READY));
        d.add_act(924, 924, 924, 0, ActKind::ConAutoComplete);
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 92);
        let _ = std::mem::take(&mut ctx.sent);
        ctx.st.quest_mut(h).unwrap().objectives[0] = 1;
        ctx.dispatch(&QuestEvent::GuardNpcDied { npc_id: 700 })
            .unwrap();
        drain(ctx);
        let sent = std::mem::take(&mut ctx.sent);
        assert!(
            sent.iter()
                .any(|p| p.0 == sc::SCQuestContextUpdatedPacket && p.1[12] == status::FAILED),
            "the failure is sent: {sent:?}"
        );
        assert!(
            !sent.iter().any(|p| p.0 == sc::SCQuestContextStartedPacket),
            "a server-side failure does not restart: {sent:?}"
        );
        assert!(
            !sent.iter().any(|p| p.0 == sc::SCQuestContextFailedPacket),
            "a server-side failure is not a refused report: {sent:?}"
        );
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.status, q.step), (status::FAILED, kind::FAIL));
        assert_eq!(q.objectives[0], 1, "the failed run keeps its progress");
    });
}

#[tokio::test]
async fn restart_request_requires_restart_on_fail_and_main_detail() {
    let (server, _) = server_with(|d| {
        for (id, restart_on_fail, detail_id) in
            [(93u32, true, detail::NORMAL), (94, false, detail::MAIN)]
        {
            d.add_template(QuestTemplate {
                restart_on_fail,
                detail_id,
                ..quest(id)
            });
            d.add_component(component(id * 10 + 1, id, kind::START));
            d.add_act(
                id * 10 + 1,
                id * 10 + 1,
                id * 10 + 1,
                0,
                ActKind::ConAutoComplete,
            );
            d.add_component(component(id * 10 + 2, id, kind::PROGRESS));
            d.add_act(
                id * 10 + 2,
                id * 10 + 2,
                id * 10 + 2,
                2,
                ActKind::ObjMonsterHunt {
                    npc_id: 600,
                    use_alias: false,
                    alias_id: 0,
                    highlight_doodad_id: 0,
                    highlight_doodad_phase: -1,
                },
            );
        }
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        for (i, id) in [93, 94].into_iter().enumerate() {
            // The previous refused attempt consumed the per-character throttle window.
            if i > 0 {
                ctx.now_ms += 3_001;
            }
            let h = start(ctx, id);
            ctx.st.quest_mut(h).unwrap().objectives[0] = 1;
            assert!(!ctx.try_restart_quest(id), "quest {id} must be refused");
            let q = ctx.st.quest(h).unwrap();
            assert_eq!(q.objectives[0], 1, "a refused restart changes nothing");
        }
    });
}

#[tokio::test]
async fn restart_request_is_throttled_per_character() {
    let (server, _) = server_with(|d| timed_restart_quest(d, 95, true, 60_000));
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(95, false, acceptor::NPC, 500));
        drain(ctx);
        let h = active(ctx, 95);
        ctx.st.quest_mut(h).unwrap().objectives[0] = 1;
        assert!(ctx.try_restart_quest(95));
        assert!(
            !ctx.try_restart_quest(95),
            "within 3 s the request is throttled"
        );
        ctx.now_ms += 3_001;
        assert!(ctx.try_restart_quest(95), "after 3 s it is accepted again");
        let h = active(ctx, 95);
        let q = ctx.st.quest(h).unwrap();
        assert_eq!((q.status, q.step), (status::PROGRESS, kind::PROGRESS));
        assert_eq!(q.objectives, [0; 5]);
    });
}

#[tokio::test]
async fn restart_keeps_the_con_accept_item_of_the_start_component() {
    const ITEM: u32 = 7000;
    let (server, _) = server_with(|d| {
        d.add_template(QuestTemplate {
            restart_on_fail: true,
            detail_id: detail::MAIN,
            ..quest(99)
        });
        d.add_component(component(991, 99, kind::START));
        d.add_act(
            991,
            991,
            991,
            1,
            ActKind::ConAcceptItem {
                item_id: ITEM,
                cleanup: false,
                drop_when_destroy: false,
                destroy_when_drop: true,
            },
        );
        d.add_component(component(992, 99, kind::PROGRESS));
        d.add_act(
            992,
            992,
            992,
            2,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.add_component(component(993, 99, kind::READY));
        d.add_act(993, 993, 993, 0, ActKind::ConAutoComplete);
    });
    let mut c = character();
    give_item(&mut c, ITEM, 1);
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        assert!(ctx.add_quest(99, false, acceptor::ITEM, ITEM));
        drain(ctx);
        let h = active(ctx, 99);
        assert_eq!(step_of(ctx, h), kind::PROGRESS);
        assert!(ctx.try_restart_quest(99));
        drain(ctx);
        let h = active(ctx, 99);
        assert_eq!(
            step_of(ctx, h),
            kind::PROGRESS,
            "the restarted quest must not stop at Start with the acceptor item gone"
        );
        let count: i32 = ctx
            .c
            .inventory
            .bag()
            .lock()
            .unwrap()
            .items
            .iter()
            .filter(|i| i.template_id == ITEM)
            .map(|i| i.count)
            .sum();
        assert_eq!(count, 1, "the ConAcceptItem acceptor item is kept");
    });
}

/// Quest engine fixes of the October 2026 audit (early completion, cleanup, selective rewards,
/// saved-state healing, data robustness, completed block ids).
mod engine_fixes {
    use super::*;
    use crate::systems::quests::engine::{has_quest_completed, set_completed_quest_flag};

    fn craft(craft_id: u32) -> ActKind {
        ActKind::ObjCraft {
            craft_id,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        }
    }

    fn supply_item(item_id: u32, cleanup: bool) -> ActKind {
        ActKind::SupplyItem {
            item_id,
            grade_id: 0,
            show_action_bar: false,
            cleanup,
            drop_when_destroy: false,
            destroy_when_drop: false,
        }
    }

    fn report(npc_id: u32) -> ActKind {
        ActKind::ConReportNpc {
            npc_id,
            use_alias: false,
            alias_id: 0,
        }
    }

    /// `id`: Start (auto) -> Supply (item 7000, cleaned up) -> Progress (craft x4) and the
    /// optional Ready (report NPC 1 + copper 50) / Reward (exp 10) steps.
    fn early_quest(d: &mut QuestData, id: u32, ready: bool, reward: bool, let_it_done: bool) {
        d.add_template(QuestTemplate {
            let_it_done,
            ..quest(id)
        });
        let c = id * 10;
        d.add_component(component(c + 1, id, kind::START));
        d.add_act(c + 1, c + 1, c + 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(c + 2, id, kind::SUPPLY));
        d.add_act(c + 2, c + 2, c + 2, 1, supply_item(7000, true));
        d.add_component(component(c + 3, id, kind::PROGRESS));
        d.add_act(c + 3, c + 3, c + 3, 4, craft(id));
        if ready {
            d.add_component(component(c + 4, id, kind::READY));
            d.add_act(c + 4, c + 4, c + 4, 0, report(1));
            d.add_act(c + 4, c + 5, c + 5, 0, ActKind::SupplyCopper { amount: 50 });
        }
        if reward {
            d.add_component(component(c + 6, id, kind::REWARD));
            d.add_act(c + 6, c + 6, c + 6, 0, ActKind::SupplyExp { exp: 10 });
        }
    }

    fn progress_to_early(ctx: &mut Ctx<'_>, id: u32) -> QuestHandle {
        let h = start(ctx, id);
        for _ in 0..2 {
            ctx.dispatch(&QuestEvent::Craft { craft_id: id }).unwrap();
        }
        drain(ctx);
        assert_eq!(ctx.objective_status(h), ObjectiveStatus::CanEarlyComplete);
        h
    }

    #[tokio::test]
    async fn let_it_done_without_reward_step_completes() {
        let (server, rec) = server_with(|d| {
            // 301: Ready step (copper) but no Reward step; 302: neither; 303: Reward step
            early_quest(d, 301, true, false, true);
            early_quest(d, 302, false, false, true);
            early_quest(d, 303, false, true, true);
        });
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            for id in [301u32, 302, 303] {
                let h = progress_to_early(ctx, id);
                assert!(ctx.st.quest(h).is_some());
                rec.take();
                ctx.try_complete_quest_as_let_it_done(id, 0);
                drain(ctx);
                assert!(!ctx.has_quest(id), "quest {id} completed");
                assert!(ctx.has_quest_completed(id), "quest {id} flagged completed");
                let calls = rec.take();
                // The completion cleanup ran (it did not for a quest that never got the
                // Completed status) and took the supply item back.
                assert!(
                    calls
                        .iter()
                        .any(|l| l.starts_with("consume") && l.ends_with("7000 1")),
                    "quest {id}: cleanup missing in {calls:?}"
                );
                match id {
                    301 => assert!(calls.contains(&"money 50".to_string()), "{calls:?}"),
                    303 => assert!(calls.contains(&"exp 10 true".to_string()), "{calls:?}"),
                    _ => {}
                }
            }
        });
    }

    #[tokio::test]
    async fn refused_early_completion_is_logged_once_per_reason() {
        let (server, _rec) = server_with(|d| early_quest(d, 304, false, true, true));
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            let h = start(ctx, 304);
            ctx.try_complete_quest_as_let_it_done(304, 0);
            ctx.try_complete_quest_as_let_it_done(304, 0);
            ctx.try_complete_quest_as_let_it_done(999, 0);
            assert_eq!(step_of(ctx, h), kind::PROGRESS);
            // Two distinct refusals were recorded once each.
            assert_eq!(ctx.st.refusal_log.len(), 2);
        });
    }

    /// Quest with two selective reward items in the Reward step; `report_npc` adds a Ready step.
    fn selective_quest(d: &mut QuestData, id: u32, report_npc: Option<u32>) {
        d.add_template(quest(id));
        let c = id * 10;
        d.add_component(component(c + 1, id, kind::START));
        d.add_act(c + 1, c + 1, c + 1, 0, ActKind::ConAutoComplete);
        d.add_component(component(c + 2, id, kind::PROGRESS));
        d.add_act(c + 2, c + 2, c + 2, 1, craft(id));
        if let Some(npc) = report_npc {
            d.add_component(component(c + 3, id, kind::READY));
            d.add_act(c + 3, c + 3, c + 3, 0, report(npc));
        }
        d.add_component(component(c + 4, id, kind::REWARD));
        for (i, item) in [9001u32, 9002].into_iter().enumerate() {
            let i = i as u32;
            d.add_act(
                c + 4,
                c + 5 + i,
                c + 5 + i,
                1,
                ActKind::SupplySelectiveItem {
                    item_id: item,
                    grade_id: 0,
                },
            );
        }
    }

    #[tokio::test]
    async fn selective_reward_defaults_to_first_item_and_honours_the_choice() {
        let (server, rec) = server_with(|d| {
            selective_quest(d, 310, None);
            selective_quest(d, 311, Some(7));
            selective_quest(d, 312, Some(7));
        });
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            // No report act: the selected index stays 0, the first item is delivered.
            start(ctx, 310);
            rec.take();
            ctx.dispatch(&QuestEvent::Craft { craft_id: 310 }).unwrap();
            drain(ctx);
            assert!(!ctx.has_quest(310));
            let calls = rec.take();
            assert_eq!(calls.iter().filter(|l| l.starts_with("item")).count(), 1);
            assert!(calls.iter().any(|l| l.contains(" 9001 ")), "{calls:?}");

            // The client sends the 1-based index of the chosen item.
            for (id, selected, item) in [(311u32, 2, "9002"), (312, 1, "9001")] {
                start(ctx, id);
                ctx.dispatch(&QuestEvent::Craft { craft_id: id }).unwrap();
                drain(ctx);
                rec.take();
                ctx.dispatch(&QuestEvent::ReportNpc {
                    quest_id: id,
                    npc_id: 7,
                    selected,
                })
                .unwrap();
                drain(ctx);
                assert!(!ctx.has_quest(id));
                let calls = rec.take();
                assert_eq!(calls.iter().filter(|l| l.starts_with("item")).count(), 1);
                assert!(
                    calls.iter().any(|l| l.contains(&format!(" {item} "))),
                    "quest {id}: {calls:?}"
                );
            }
        });
    }

    #[tokio::test]
    async fn out_of_range_selective_index_gives_the_first_item() {
        let (server, rec) = server_with(|d| selective_quest(d, 313, Some(7)));
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            start(ctx, 313);
            ctx.dispatch(&QuestEvent::Craft { craft_id: 313 }).unwrap();
            drain(ctx);
            rec.take();
            ctx.dispatch(&QuestEvent::ReportNpc {
                quest_id: 313,
                npc_id: 7,
                selected: 9,
            })
            .unwrap();
            drain(ctx);
            let calls = rec.take();
            assert!(calls.iter().any(|l| l.contains(" 9001 ")), "{calls:?}");
        });
    }

    fn saved(template_id: u32, step: u8, status: u8, objectives: [i32; 5]) -> ActiveQuest {
        ActiveQuest {
            id: 0x21,
            template_id,
            status,
            objectives,
            step,
            acceptor_type: acceptor::NPC,
            component_id: 0,
            acceptor_id: 500,
            time_ms: 0,
        }
    }

    /// Live character Shael: quest 87 (LetItDone) saved with status Ready on its Progress step
    /// and 5/5 objectives. That combination is how a LetItDone quest shows "ready to turn in";
    /// it loads unchanged and can be reported.
    #[tokio::test]
    async fn let_it_done_ready_status_on_progress_step_loads_and_reports() {
        let (server, rec) = server_with(|d| {
            d.add_template(QuestTemplate {
                let_it_done: true,
                ..quest(87)
            });
            d.add_component(component(871, 87, kind::START));
            d.add_act(871, 871, 871, 0, ActKind::ConAutoComplete);
            d.add_component(component(872, 87, kind::PROGRESS));
            d.add_act(872, 872, 872, 5, craft(87));
            d.add_component(component(873, 87, kind::READY));
            d.add_act(873, 873, 873, 0, report(210));
            d.add_component(component(874, 87, kind::REWARD));
            d.add_act(874, 874, 874, 1, supply_item(4093, false));
        });
        let mut c = character();
        c.quests.active = vec![saved(87, kind::PROGRESS, status::READY, [5, 0, 0, 0, 0])];
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            ctx.load_from_rows();
            drain(ctx);
            let h = active(ctx, 87);
            assert_eq!(step_of(ctx, h), kind::PROGRESS);
            assert_eq!(ctx.st.quest(h).unwrap().status, status::READY);
            rec.take();
            ctx.dispatch(&QuestEvent::ReportNpc {
                quest_id: 87,
                npc_id: 210,
                selected: 0,
            })
            .unwrap();
            drain(ctx);
            assert!(!ctx.has_quest(87));
            assert!(ctx.has_quest_completed(87));
            assert!(rec.take().iter().any(|l| l.contains(" 4093 ")));
        });
    }

    /// A non LetItDone quest saved with the same stale combination heals itself on load.
    #[tokio::test]
    async fn stale_ready_status_on_progress_step_heals_on_load() {
        let (server, _rec) = server_with(main_quest);
        for objectives in [[0, 0, 0, 0, 0], [2, 0, 0, 0, 0]] {
            let mut c = character();
            c.quests.active = vec![saved(100, kind::PROGRESS, status::READY, objectives)];
            with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
                ctx.load_from_rows();
                let h = active(ctx, 100);
                assert_eq!(ctx.st.quest(h).unwrap().status, status::PROGRESS);
                drain(ctx);
                if objectives[0] == 2 {
                    assert_eq!(step_of(ctx, h), kind::READY);
                    assert_eq!(ctx.st.quest(h).unwrap().status, status::READY);
                } else {
                    assert_eq!(step_of(ctx, h), kind::PROGRESS);
                    assert_eq!(ctx.st.quest(h).unwrap().status, status::PROGRESS);
                }
            });
        }
    }

    #[tokio::test]
    async fn score_quest_without_score_acts_completes() {
        let (server, _rec) = server_with(|d| {
            d.add_template(QuestTemplate {
                score: 100,
                ..quest(5641)
            });
            d.add_component(component(56411, 5641, kind::START));
            d.add_act(56411, 56411, 56411, 0, ActKind::ConAutoComplete);
            d.add_component(component(56412, 5641, kind::PROGRESS));
        });
        let mut c = character();
        with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
            assert!(ctx.add_quest(5641, false, 0, 0));
            drain(ctx);
            assert!(!ctx.has_quest(5641));
            assert!(ctx.has_quest_completed(5641));
        });
    }

    #[test]
    fn self_referencing_completion_requirement_is_dropped() {
        use crate::systems::quests::unit_reqs::{req_kind, UnitReq};
        let mut d = QuestData::default();
        d.add_template(quest(6596));
        d.add_component(component(28139, 6596, kind::START));
        let req = |id: u32, kind: u32, value1: u32| UnitReq {
            id,
            owner_id: 28139,
            owner_type: "QuestComponent".into(),
            kind,
            value1,
            value2: 0,
        };
        let reqs = [
            req(1, req_kind::LEVEL, 50),
            req(2, req_kind::COMPLETE_QUEST_CONTEXT, 6596),
            req(3, req_kind::COMPLETE_QUEST_CONTEXT, 6000),
        ];
        for r in reqs {
            d.unit_reqs.by_id.insert(r.id, r.clone());
            d.unit_reqs
                .by_owner
                .entry("QuestComponent".into())
                .or_default()
                .entry(28139)
                .or_default()
                .push(r);
        }
        d.drop_self_referencing_requirements();
        let left: Vec<u32> = d
            .unit_reqs
            .requirements("QuestComponent", 28139)
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(left, vec![1, 3]);
    }

    #[test]
    fn completed_flags_of_event_quests_keep_their_full_block() {
        let mut blocks = Vec::new();
        // 8000001 / 64 = 125000 does not fit the 16 bit block id (wrapped to 59464 before).
        let block = set_completed_quest_flag(&mut blocks, 8_000_001, true);
        assert_eq!(block.id, 125_000);
        assert!(has_quest_completed(&blocks, 8_000_001));
        assert!(!has_quest_completed(&blocks, 3_805_697));
        assert!(!has_quest_completed(&blocks, 8_000_002));
        // The client wire cannot show the block: it is left out of SCCompletedQuests.
        set_completed_quest_flag(&mut blocks, 100, true);
        let p = crate::packets::world::SCCompletedQuestsPacket { quests: blocks };
        let mut s = PacketStream::new();
        aaemu_commons::Marshal::write(&p, &mut s);
        let bytes = s.into_bytes();
        assert_eq!(i32::from_le_bytes(bytes[..4].try_into().unwrap()), 1);
        assert_eq!(bytes.len(), 4 + 2 + 8);
        assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), 1);
    }
}

fn zone_kill_act(
    count_player_kill: i32,
    count_npc: i32,
    team_share: bool,
    levels: (i32, i32, i32, i32),
) -> ActKind {
    ActKind::ObjZoneKill {
        count_player_kill,
        count_npc,
        zone_id: 0,
        team_share,
        use_alias: false,
        alias_id: 0,
        lvl_min: levels.0,
        lvl_max: levels.1,
        is_party: team_share,
        lvl_min_npc: levels.2,
        lvl_max_npc: levels.3,
        pc_faction_id: 0,
        pc_faction_exclusive: false,
        npc_faction_id: 0,
        npc_faction_exclusive: false,
    }
}

#[test]
fn zone_kill_level_bounds_of_zero_are_unset() {
    assert!(super::acts::level_in_range(60, 0, 0));
    assert!(super::acts::level_in_range(1, 0, 0));
    assert!(super::acts::level_in_range(60, 10, 0));
    assert!(!super::acts::level_in_range(9, 10, 0));
    assert!(super::acts::level_in_range(5, 0, 5));
    assert!(!super::acts::level_in_range(6, 0, 5));
    assert!(super::acts::level_in_range(7, 5, 10));
    assert!(!super::acts::level_in_range(11, 5, 10));
}

#[tokio::test]
async fn zone_kill_with_unset_level_range_accepts_any_victim() {
    let (server, _rec) = server_with(|d| {
        // The 1.2 data shape: lv_min/lv_max set (the questor), lv_min_npc/lv_max_npc = 0/0.
        single_act_quest(d, 14, 1, zone_kill_act(0, 1, false, (1, 50, 0, 0)));
        // Only a lower NPC bound.
        single_act_quest(d, 15, 1, zone_kill_act(0, 1, false, (1, 50, 10, 0)));
        // Player kills with an unset window.
        single_act_quest(d, 16, 1, zone_kill_act(1, 0, false, (0, 0, 0, 0)));
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let me = super::engine::KillUnitInfo {
            obj_id: 1,
            character_id: Some(1),
            ..Default::default()
        };
        let npc = |level| super::engine::KillUnitInfo {
            obj_id: 50,
            npc_template_id: Some(600),
            level,
            ..Default::default()
        };
        let pc = |level| super::engine::KillUnitInfo {
            obj_id: 51,
            character_id: Some(9),
            level,
            ..Default::default()
        };
        let kill = |ctx: &mut Ctx<'_>, victim| {
            ctx.dispatch(&QuestEvent::ZoneKill {
                zone_group_id: 1,
                killer: me.clone(),
                victim,
            })
            .unwrap();
        };
        let (a, b, p) = (start(ctx, 14), start(ctx, 15), start(ctx, 16));
        // A level-1 victim is below the lower bound of 15 but passes the 0/0 quest.
        kill(ctx, npc(9));
        assert_eq!(ctx.st.quest(a).unwrap().objectives[0], 1, "0/0 npc range");
        assert_eq!(ctx.st.quest(b).unwrap().objectives[0], 0, "below min 10");
        assert_eq!(ctx.st.quest(p).unwrap().objectives[0], 0, "not a player");
        kill(ctx, npc(60));
        assert_eq!(ctx.st.quest(b).unwrap().objectives[0], 1, "no upper bound");
        kill(ctx, pc(70));
        assert_eq!(ctx.st.quest(p).unwrap().objectives[0], 1, "0/0 pc range");
    });
}

#[tokio::test]
async fn zone_kill_team_share_is_not_reshared_by_the_receiver() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(d, 17, 5, zone_kill_act(0, 5, true, (1, 50, 0, 0)));
    });
    let mut c = character();
    with_ctx(&server, &mut c, QuestEnv::default(), |ctx| {
        let h = start(ctx, 17);
        let victim = super::engine::KillUnitInfo {
            obj_id: 50,
            npc_template_id: Some(600),
            level: 20,
            ..Default::default()
        };
        let own = |id| super::engine::KillUnitInfo {
            obj_id: id,
            character_id: Some(id),
            ..Default::default()
        };
        // Own kill: shared with the team.
        ctx.dispatch(&QuestEvent::ZoneKill {
            zone_group_id: 1,
            killer: own(1),
            victim: victim.clone(),
        })
        .unwrap();
        assert!(matches!(
            ctx.deferred.last(),
            Some(super::engine::Deferred::TeamEvent { .. })
        ));
        ctx.deferred.clear();
        // A team mate's kill arrives: it counts, but is not shared again.
        ctx.dispatch(&QuestEvent::ZoneKill {
            zone_group_id: 1,
            killer: own(2),
            victim,
        })
        .unwrap();
        assert!(ctx.deferred.is_empty());
        assert_eq!(ctx.st.quest(h).unwrap().objectives[0], 2);
    });
}

#[tokio::test]
async fn zone_kill_team_share_credits_each_member_once() {
    let (server, rec) = server_with(|d| {
        single_act_quest(d, 17, 5, zone_kill_act(0, 5, true, (1, 50, 0, 0)));
    });
    let a_client = TestClient::new(&server, 1, 1001);
    let b_client = TestClient::new(&server, 2, 1002);
    let a = spawn_player(&server, &a_client, character(), 0x100);
    let b = spawn_player(
        &server,
        &b_client,
        test_character(2, "Mate", 100.0, 100.0),
        0x101,
    );
    *rec.team.lock().unwrap() = vec![a.clone(), b.clone()];
    events::add_quest(&server, &a, 17, false);
    events::add_quest(&server, &b, 17, false);
    server.systems.quests.run_queued_evaluations(&server);

    let world = server.world.world(0).unwrap();
    let mut wolf = crate::models::npc::Npc::new(600);
    wolf.unit.transform.set_xyz(110.0, 110.0, 100.0);
    wolf.unit.level = 20;
    let wolf = Entity::new(0x201, wolf);
    world.spawn(&wolf, &server.view_ctx());

    // Before the guard the shared event bounced between the members without end.
    events::on_npc_killed(&server, &a, &wolf);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(a.data().quests.active[0].objectives[0], 1);
    assert_eq!(b.data().quests.active[0].objectives[0], 1);
}

#[tokio::test]
async fn aggro_quest_started_by_engaging_the_npc_counts_its_kill() {
    // The 30 `QuestActObjAggro` quests of the 1.2 data start through
    // `NpcTemplate.EngageCombatGiveQuestId` (`AddQuestFromNpc`), so the acceptor is the NPC.
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            18,
            3,
            ActKind::ObjAggro {
                range: 0,
                rank1: 10,
                rank1_ratio: 100,
                rank1_item: true,
                rank2: 50,
                rank2_ratio: 50,
                rank2_item: false,
                rank3: 100,
                rank3_ratio: 10,
                rank3_item: false,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    let world = server.world.world(0).unwrap();
    let mut boss = crate::models::npc::Npc::new(600);
    boss.unit.transform.set_xyz(110.0, 110.0, 100.0);
    let boss = Entity::new(0x201, boss);
    world.spawn(&boss, &server.view_ctx());
    let mut other = crate::models::npc::Npc::new(601);
    other.unit.transform.set_xyz(111.0, 110.0, 100.0);
    let other = Entity::new(0x202, other);
    world.spawn(&other, &server.view_ctx());

    events::add_quest_from_npc(&server, &player, 18, boss.obj_id);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].acceptor_id, 600);
    events::on_npc_killed(&server, &player, &other);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 0);
    events::on_npc_killed(&server, &player, &boss);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 3);
}

#[tokio::test]
async fn sphere_trigger_uses_the_world_position_of_an_attached_player() {
    let (server, _rec) = server_with(|d| {
        single_act_quest(
            d,
            20,
            1,
            ActKind::ObjSphere {
                sphere_id: 0,
                npc_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    server.systems.quests.spheres.load(
        &server.game_data.systems.quests,
        &[super::data::RawSignSphere {
            zone_id: 1,
            quest_id: 20,
            component_id: 202,
            local: [125.0, 120.0, 100.0],
            radius: 3.0,
        }],
        &[],
        "main_world",
        |_| (0, 0),
    );
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    // A mounted rider / ship passenger: the transform is relative to the parent (the
    // vehicle), the sphere is in world coordinates.
    let world = server.world.world(0).unwrap();
    let mut ship = crate::models::npc::Npc::new(700);
    ship.unit.transform.set_xyz(120.0, 120.0, 100.0);
    let ship = Entity::new(0x300, ship);
    world.spawn(&ship, &server.view_ctx());
    {
        let mut c = player.data();
        c.unit.transform.parent_obj_id = ship.obj_id;
        c.unit.transform.set_xyz(5.0, 0.0, 0.0);
    }
    let pos = events::world_position(&server, &WorldObject::from(player.clone()));
    assert!(
        (pos.0 - 125.0).abs() < 0.01 && (pos.1 - 120.0).abs() < 0.01,
        "{pos:?}"
    );
    assert_eq!(events::capture_env(&server, &player).position, pos);

    events::add_quest(&server, &player, 20, false);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 1);
}

#[tokio::test]
async fn save_tutorial_ignores_ids_that_are_not_tutorial_entries() {
    let (server, _rec) = server_with(|_| {});
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, test_character(1, "Al", 10.0, 10.0), 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    CSSaveTutorialPacket { id: 0xFFFF_FFFF }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert!(
        player.data().quests.completed.is_empty(),
        "an arbitrary id was stored as a completed tutorial"
    );
    assert!(client.drain_ops().is_empty());
}

#[tokio::test]
async fn save_tutorial_accepts_a_real_tutorial_entry() {
    let (server, _rec) = server_with(|d| {
        d.tutorial_ids.insert(2584);
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, test_character(1, "Al", 10.0, 10.0), 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    CSSaveTutorialPacket { id: 2584 }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert!(super::engine::has_quest_completed(
        &player.data().quests.completed,
        2584
    ));
    assert_eq!(client.drain_ops(), vec![sc::SCTutorialSavedPacket]);
}

#[tokio::test]
async fn save_tutorial_caps_the_tutorial_entries() {
    let cap = super::packets::MAX_TUTORIAL_ENTRIES as u32;
    let first = 10_000;
    let (server, _rec) = server_with(|d| {
        for id in first..first + cap + 4 {
            d.tutorial_ids.insert(id);
        }
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, test_character(1, "Al", 10.0, 10.0), 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    {
        let mut c = player.data();
        for id in first..first + cap {
            super::engine::set_completed_quest_flag(&mut c.quests.completed, id, true);
        }
    }
    let next = first + cap;
    CSSaveTutorialPacket { id: next }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert!(
        !super::engine::has_quest_completed(&player.data().quests.completed, next),
        "the tutorial cap did not stop the completed-quest set from growing"
    );
    assert!(client.drain_ops().is_empty());
}

// ----- item-use quest events through a consumable Item cast -----

const CASTABLE_ITEM: u32 = 7_100;
const CASTABLE_SKILL: u32 = 20_100;

/// A heal effect that consumes the casting item, plus the use skill / template of that item.
fn castable_item_data(build: impl FnOnce(&mut QuestData)) -> GameData {
    use crate::systems::skills::data::effects::{EffectTemplate, HealEffect};
    use crate::systems::skills::data::EffectKind;

    let mut d = QuestData::default();
    build(&mut d);
    d.finish();
    let mut data = GameData::empty();
    // The hand-built quest data references item ids that the fixtures never loaded. Seed them
    // like the real item catalog so the reward filter (unknown templates are skipped) sees the
    // same shape as production.
    for act in &d.acts {
        let item_id = match &act.kind {
            ActKind::ConAcceptItem { item_id, .. }
            | ActKind::ConAcceptItemEquip { item_id }
            | ActKind::ConAcceptItemGain { item_id }
            | ActKind::EtcItemObtain { item_id, .. }
            | ActKind::ObjItemGather { item_id, .. }
            | ActKind::ObjItemUse { item_id, .. }
            | ActKind::ObjMateLevel { item_id, .. }
            | ActKind::SupplyItem { item_id, .. }
            | ActKind::SupplyRemoveItem { item_id }
            | ActKind::SupplySelectiveItem { item_id, .. } => *item_id,
            _ => 0,
        };
        if item_id > 0 {
            data.items
                .templates
                .entry(item_id)
                .or_insert_with(|| ItemTemplate::plain(item_id, 1000));
        }
    }
    data.systems.quests = d;
    data.systems.skills.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(HealEffect {
            id: 1,
            use_fixed_heal: true,
            fixed_min: 1,
            fixed_max: 1,
            use_level_heal: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: false,
            use_charged_buff: false,
            charged_buff_id: 0,
            charged_mul: 0.0,
            slave_applicable: false,
            ignore_heal_aggro: false,
            dps_multiplier: 0.0,
            actability_group_id: 0,
            actability_step: 0,
            actability_mul: 0.0,
            actability_add: 0.0,
        }),
        1,
    );
    let mut skill = crate::systems::skills::tests::skill_template(CASTABLE_SKILL);
    skill.target_type = 0;
    skill.damage_type_id = 0;
    skill.casting_time = 0;
    let mut effect = crate::systems::skills::tests::skill_effect(1, EffectKind::HealEffect, 1);
    effect.consume_source_item = true;
    effect.consume_item_count = 1;
    skill.effects.push(effect);
    data.systems.skills.add_skill(skill);
    let mut item = ItemTemplate::plain(CASTABLE_ITEM, 1000);
    item.use_skill_id = CASTABLE_SKILL;
    data.items.templates.insert(CASTABLE_ITEM, item);
    data
}

fn castable_item_server(
    build: impl FnOnce(&mut QuestData),
) -> (Arc<GameServer>, TestClient, PlayerRef) {
    let server = Arc::new(test_server_with(castable_item_data(build)));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    super::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    (server, client, player)
}

/// Adds the castable item to the player's bag; returns its item id.
fn give_castable_item(server: &GameServer, player: &PlayerRef, count: i32) -> u64 {
    use crate::systems::economy::inventory::Inv;
    let item = server
        .items
        .create(
            CASTABLE_ITEM,
            count,
            0,
            server.config.id,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("castable item");
    let id = item.id;
    let bag = Inv::of(player).bag();
    bag.lock()
        .unwrap()
        .add_new_item(item, 0, &server.game_data.items)
        .expect("placed");
    server.items.add_container(bag);
    id
}

async fn cast_item_skill(
    server: &GameServer,
    client: &mut TestClient,
    player: &PlayerRef,
    item_id: u64,
) {
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    crate::systems::skills::handlers::CSStartSkillPacket {
        skill_id: CASTABLE_SKILL,
        caster: SkillCaster::Item {
            obj_id: player.obj_id,
            item_id,
            item_template_id: CASTABLE_ITEM,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(player.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
    .handle(server, &mut client.conn)
    .await
    .expect("cast");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    server.systems.quests.run_queued_evaluations(server);
}

fn objective_count(player: &PlayerRef, quest_id: u32, index: usize) -> i32 {
    player
        .data()
        .quests
        .active
        .iter()
        .find(|q| q.template_id == quest_id)
        .expect("active quest")
        .objectives[index]
}

#[tokio::test]
async fn consumable_item_cast_progresses_a_use_item_objective() {
    let (server, mut client, player) = castable_item_server(|d| {
        single_act_quest(
            d,
            200,
            1,
            ActKind::ObjItemUse {
                item_id: CASTABLE_ITEM,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                drop_when_destroy: false,
            },
        );
    });
    // One unit: the debit removes the item before the chance / ItemUse event is raised.
    let item_id = give_castable_item(&server, &player, 1);
    events::add_quest(&server, &player, 200, false);
    server.systems.quests.run_queued_evaluations(&server);
    cast_item_skill(&server, &mut client, &player, item_id).await;
    assert_eq!(
        objective_count(&player, 200, 0),
        1,
        "the use-item objective progresses for a consumed item"
    );
}

#[tokio::test]
async fn partial_consumption_updates_a_collect_objective() {
    let (server, mut client, player) = castable_item_server(|d| {
        single_act_quest(
            d,
            201,
            3,
            ActKind::ObjItemGather {
                item_id: CASTABLE_ITEM,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
                cleanup: false,
                drop_when_destroy: false,
                destroy_when_drop: false,
            },
        );
    });
    // Two units in one stack: the cast consumes one and keeps the other.
    let item_id = give_castable_item(&server, &player, 2);
    events::add_quest(&server, &player, 201, false);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(objective_count(&player, 201, 0), 2, "initial count");
    cast_item_skill(&server, &mut client, &player, item_id).await;
    assert_eq!(
        objective_count(&player, 201, 0),
        1,
        "the partial consume reports one consumed unit"
    );
}

/// A pirate whose infamy reaches 0 through the quest act `QuestActSupplyCrimePoint` returns to
/// the faction of the race/gender template (the home faction). The 1.2 client prints ui_text
/// 2297 `pardon_pirate` from the `SCUnitFactionChangedPacket` (old 161 -> non-pirate), so the
/// faction change is the notice; the same event credits achievement kind 49 ("Repentant",
/// `char_records` 2401). This drives the real quest operation, act and shared helper.
#[tokio::test]
async fn pirate_reaching_zero_infamy_through_the_quest_act_returns_home() {
    use crate::data::character_templates::CharacterTemplate;
    use crate::systems::social::achievement::char_record_kind;
    use crate::systems::social::achievement_flow_tests::one_achievement;
    use crate::systems::social::packets::faction::SCUnitFactionChangedPacket;

    const QUEST: u32 = 9_001;
    const HOME: u32 = 101;
    const PIRATE: u32 = 161;

    let mut quests = QuestData::default();
    quests.add_template(quest(QUEST));
    let c = QUEST * 10;
    quests.add_component(component(c + 1, QUEST, kind::START));
    quests.add_act(c + 1, c + 1, c + 1, 0, ActKind::ConAutoComplete);
    quests.add_component(component(c + 2, QUEST, kind::PROGRESS));
    quests.add_act(
        c + 2,
        c + 2,
        c + 2,
        1,
        ActKind::ObjCraft {
            craft_id: QUEST,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    quests.add_component(component(c + 3, QUEST, kind::REWARD));
    quests.add_act(
        c + 3,
        c + 3,
        c + 3,
        0,
        ActKind::SupplyCrimePoint { point: -500 },
    );
    quests.finish();

    let mut data = GameData::empty();
    data.systems.quests = quests;
    data.characters.templates.insert(
        0x11,
        CharacterTemplate {
            race: 1,
            gender: 1,
            creatable: true,
            faction_id: HOME,
            ..Default::default()
        },
    );
    data.systems.social.achievements = one_achievement(
        1380,
        1,
        0,
        2401,
        char_record_kind::MY_DISHONORABLE_POINT,
        0,
        0,
    );
    let server = std::sync::Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::social::install_hooks(&server);
    super::install_hooks(&server);

    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Pirate", 100.0, 100.0);
    character.unit.faction_id = PIRATE;
    character.crime_point = 500;
    character.infamy_point = 500;
    character.is_online = true;
    let player = spawn_player(&server, &client, character, 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();

    assert!(events::add_quest(&server, &player, QUEST, false).is_some());
    server.systems.quests.run_queued_evaluations(&server);
    events::on_craft(&server, &player, QUEST);
    server.systems.quests.run_queued_evaluations(&server);

    {
        let c = player.data();
        assert_eq!((c.crime_point, c.infamy_point), (0, 0));
        assert_eq!(c.unit.faction_id, HOME, "the pirate returned home");
        assert_eq!(crate::db_character::persisted_faction(&c), HOME);
        assert_eq!(
            c.achievement_state.record_value(2401),
            1,
            "repentant (kind 49) credited"
        );
    }
    let frames = client.drain();
    let expected = crate::systems::social::packets::body(&SCUnitFactionChangedPacket {
        obj_id: 0x100,
        unit_name: "Pirate".into(),
        id: PIRATE,
        id2: HOME,
        temp: false,
    });
    assert!(
        frames
            .iter()
            .any(|(op, body)| *op == sc::SCUnitFactionChangedPacket && *body == expected),
        "the faction change the client turns into ui_text 2297: {frames:x?}"
    );
}

/// `QuestActSupplyJuryPoint` goes through the `Character.JuryPoint` setter: the 1.2 client's
/// jury point counter only changes on `SCJuryPointChangedPacket` (the C# raised it from the
/// property; the direct field write here sent nothing).
#[tokio::test]
async fn supply_jury_point_sends_the_changed_packet() {
    use crate::systems::social::packets::trial::SCJuryPointChangedPacket;

    const QUEST: u32 = 9_002;
    let mut quests = QuestData::default();
    quests.add_template(quest(QUEST));
    let c = QUEST * 10;
    quests.add_component(component(c + 1, QUEST, kind::START));
    quests.add_act(c + 1, c + 1, c + 1, 0, ActKind::ConAutoComplete);
    quests.add_component(component(c + 2, QUEST, kind::PROGRESS));
    quests.add_act(
        c + 2,
        c + 2,
        c + 2,
        1,
        ActKind::ObjCraft {
            craft_id: QUEST,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    quests.add_component(component(c + 3, QUEST, kind::REWARD));
    quests.add_act(
        c + 3,
        c + 3,
        c + 3,
        0,
        ActKind::SupplyJuryPoint { point: 1 },
    );
    quests.finish();

    let mut data = GameData::empty();
    data.systems.quests = quests;
    let server = std::sync::Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::social::install_hooks(&server);
    super::install_hooks(&server);

    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Juror", 100.0, 100.0);
    character.is_online = true;
    let player = spawn_player(&server, &client, character, 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();

    assert!(events::add_quest(&server, &player, QUEST, false).is_some());
    server.systems.quests.run_queued_evaluations(&server);
    events::on_craft(&server, &player, QUEST);
    server.systems.quests.run_queued_evaluations(&server);

    assert_eq!(player.data().jury_point, 1);
    let frames = client.drain();
    let expected =
        crate::systems::social::packets::body(&SCJuryPointChangedPacket { jury_point: 1 });
    assert!(
        frames
            .iter()
            .any(|(op, body)| *op == sc::SCJuryPointChangedPacket && *body == expected),
        "jury point change packet: {frames:x?}"
    );
}

#[tokio::test]
async fn client_quest_requests_name_an_npc_within_reach() {
    let (server, _) = server_with(|d| {
        d.add_template(quest(60));
        d.add_component(component(601, 60, kind::START));
        d.add_act(601, 601, 601, 0, ActKind::ConAcceptNpc { npc_id: 500 });
        d.add_component(component(602, 60, kind::READY));
        d.add_act(
            602,
            602,
            602,
            0,
            ActKind::ConReportNpc {
                npc_id: 500,
                use_alias: false,
                alias_id: 0,
            },
        );
        // Quest 61: auto-started, turned in at NPC 501.
        d.add_template(quest(61));
        d.add_component(component(611, 61, kind::START));
        d.add_act(611, 611, 611, 0, ActKind::ConAutoComplete);
        d.add_component(component(612, 61, kind::READY));
        d.add_act(
            612,
            612,
            612,
            0,
            ActKind::ConReportNpc {
                npc_id: 501,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    // The player stands at (100, 100): obj 0x210 is 150 m away, 0x211 about 7 m.
    for (obj, template, x) in [
        (0x210u32, 500u32, 250.0f32),
        (0x211, 500, 105.0),
        (0x212, 501, 250.0),
        (0x213, 501, 105.0),
    ] {
        let mut n = crate::models::npc::Npc::new(template);
        n.unit.transform.set_xyz(x, 105.0, 100.0);
        world.spawn(Entity::new(obj, n), &server.view_ctx());
    }
    events::add_quest(&server, &player, 61, false);
    server.systems.quests.run_queued_evaluations(&server);
    client.drain();

    // Start: the far NPC is refused, the near one (within reach + dialog slack) accepted.
    events::start_quest_context(&server, &player, 60, 0x210, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(!events::has_quest(&player, 60));
    events::start_quest_context(&server, &player, 60, 0x211, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(events::has_quest(&player, 60));

    // Report: the far NPC answers INVALID_NPC_OR_QUEST without reaching the quest.
    client.drain();
    events::on_report(&server, &player, 61, 0x212, 0, 0);
    assert_one_failed(
        &mut client,
        61,
        super::packets::quest_error::INVALID_NPC_OR_QUEST,
    );
    assert!(events::has_quest(&player, 61));
    events::on_report(&server, &player, 61, 0x213, 0, 0);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(
        !events::has_quest(&player, 61),
        "the near NPC completes the report"
    );

    // The config switch turns the check off.
    server
        .config
        .update(|c| c.interaction.npc_reach_check = false);
    events::start_quest_context(&server, &player, 60, 0x210, 0, 0);
    // (already active: nothing to assert beyond no panic)
}
