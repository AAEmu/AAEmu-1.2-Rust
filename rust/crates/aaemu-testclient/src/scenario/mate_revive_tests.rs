use super::mate_revive::{added_items, error_codes, item_tasks, mate_flags};
use super::*;
use crate::game::{GameEvent, GameState, LEVEL_GAME};
use aaemu_commons::Marshal;
use aaemu_game::data::items::ItemTemplate;
use aaemu_game::data::ItemData;
use aaemu_game::models::item::Item;
use aaemu_game::models::item_task::ItemTask;
use aaemu_game::systems::economy::item_packets::SCItemTaskSuccessPacket;
use aaemu_game::systems::npcs::npc::packets::SCNpcInteractionSkillListPacket;

fn game_packet(opcode: u16, body: Vec<u8>) -> Packet {
    Packet {
        level: LEVEL_GAME,
        opcode,
        body,
    }
}

fn item(template: u32, id: u64) -> Item {
    let mut i = Item::create(
        id,
        &ItemTemplate::plain(template, 1),
        1,
        0,
        1,
        &ItemData::default(),
        77,
    );
    i.slot_type = 2;
    i.slot = 4;
    i
}

fn encode<M: Marshal>(m: &M) -> Vec<u8> {
    let mut s = PacketStream::new();
    m.write(&mut s);
    s.into_bytes()
}

#[test]
fn item_create_tasks_yield_template_and_item_id() {
    let body = encode(&SCItemTaskSuccessPacket {
        action: 11,
        tasks: vec![ItemTask::add(&item(8159, 0x0100_0042))],
        force_remove: Vec::new(),
    });
    assert_eq!(added_items(&body), vec![(8159, 0x0100_0042)]);
}

#[test]
fn create_tasks_are_read_across_money_tasks_and_truncation() {
    let body = encode(&SCItemTaskSuccessPacket {
        action: 11,
        tasks: vec![
            ItemTask::add(&item(8159, 7)),
            ItemTask::add(&item(8160, 8)),
            ItemTask::MoneyChange(5),
            ItemTask::add(&item(8161, 9)),
        ],
        force_remove: Vec::new(),
    });
    assert_eq!(added_items(&body), vec![(8159, 7), (8160, 8), (8161, 9)]);
    assert!(added_items(&[]).is_empty());
    // Cut inside the first task's details: only its leading ids were readable.
    assert_eq!(added_items(&body[..40]), vec![(8159, 7)]);
}

#[test]
fn interaction_skill_list_decodes_npc_and_skills() {
    let body = encode(&SCNpcInteractionSkillListPacket {
        npc_obj_id: 0x0A0B0C,
        obj_id: 0x0A0B0C,
        extra_info: 0,
        pick_id: 0,
        mouse_button: 1,
        modifier_keys: 0,
        skill_list: vec![12195, 7],
    });
    assert_eq!(
        interaction_skill_list(&body).unwrap(),
        (0x0A0B0C, vec![12195, 7])
    );
    let mut huge = PacketStream::new();
    huge.write_bc(1)
        .write_bc(1)
        .write_i32(0)
        .write_i32(0)
        .write_u8(1)
        .write_i32(1_000_000);
    assert!(interaction_skill_list(huge.as_bytes()).is_err());
    assert!(interaction_skill_list(&body[..4]).is_err());
}

#[test]
fn game_state_tracks_mate_spawn_points_and_death() {
    let mut st = GameState::default();
    let mut spawned = PacketStream::new();
    spawned
        .write_u16(33)
        .write_u32(5)
        .write_u64(0x0100_0042)
        .write_u8(1);
    st.observe(&game_packet(sc::SCMateSpawnedPacket, spawned.into_bytes()))
        .unwrap();
    let spawn = st.mate_spawned.unwrap();
    assert_eq!(
        (spawn.tl_id, spawn.db_id, spawn.item_id),
        (33, 5, 0x0100_0042)
    );
    assert_eq!(st.mate_spawn_count, 1);

    let mut points = PacketStream::new();
    points.write_bc(0x300).write_i32(250_000).write_i32(10_000);
    st.observe(&game_packet(sc::SCUnitPointsPacket, points.into_bytes()))
        .unwrap();
    assert_eq!(st.unit_points[&0x300], (2500, 100));

    let mut death = PacketStream::new();
    death
        .write_bc(0x300)
        .write_u8(1)
        .write_i32(0)
        .write_i32(0)
        .write_u8(0)
        .write_bc(0);
    st.observe(&game_packet(sc::SCUnitDeathPacket, death.into_bytes()))
        .unwrap();
    assert!(st.dead_units.contains(&0x300));

    let mut removed = PacketStream::new();
    removed.write_u16(1).write_bc(0x300);
    st.observe(&game_packet(sc::SCUnitsRemovedPacket, removed.into_bytes()))
        .unwrap();
    assert!(!st.dead_units.contains(&0x300));
    assert!(!st.unit_points.contains_key(&0x300));
}

fn mate_update_body(action: u8, item_id: u64, exp: i32, dead: u8, level: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u8(action)
        .write_u8(2)
        .write_u8(1) // ItemAction.ChangeMoneyAmount
        .write_i32(-12_345)
        .write_u8(9) // ItemAction.UpdateDetail
        .write_u8(2)
        .write_u8(4)
        .write_u64(item_id)
        .write_i16(128);
    let mut block = [0u8; 128];
    block[0] = 3; // DetailType.Mate
    block[1..5].copy_from_slice(&exp.to_le_bytes());
    block[5] = dead;
    block[6] = level;
    s.write_raw(&block);
    s.write_u8(0).write_u32(0); // force remove count, type id
    s.into_bytes()
}

#[test]
fn mate_update_detail_exposes_the_dead_flag_and_money() {
    let info = crate::game::parse_item_task(&mate_update_body(59, 77, 500, 1, 3));
    assert_eq!(info.action, 59);
    assert_eq!(info.money, -12_345);
    assert_eq!(info.items.len(), 1);
    assert_eq!(info.items[0].id, 77);
    assert_eq!(info.items[0].task, 9);
    assert_eq!(info.items[0].mate, Some((500, 1, 3)));

    let events = [
        GameEvent::ErrorMsg(111),
        GameEvent::ItemTask(info),
        GameEvent::ItemTask(crate::game::parse_item_task(&mate_update_body(
            58, 77, 500, 0, 3,
        ))),
    ];
    assert_eq!(error_codes(&events), vec![111]);
    assert_eq!(mate_flags(&events, 77), vec![(500, 1, 3), (500, 0, 3)]);
    assert!(mate_flags(&events, 78).is_empty());
    assert_eq!(
        item_tasks(&events).map(|t| t.action).collect::<Vec<_>>(),
        vec![59, 58]
    );
}

#[test]
fn state_collects_error_and_item_task_events() {
    let mut st = GameState::default();
    let mut err = PacketStream::new();
    err.write_i16(106)
        .write_i16(106)
        .write_u32(0)
        .write_bool(false);
    st.observe(&game_packet(sc::SCErrorMsgPacket, err.into_bytes()))
        .unwrap();
    st.observe(&game_packet(
        sc::SCItemTaskSuccessPacket,
        mate_update_body(58, 5, 0, 0, 1),
    ))
    .unwrap();
    assert_eq!(st.events.len(), 2);
    assert_eq!(st.events[0], GameEvent::ErrorMsg(106));
    assert!(matches!(&st.events[1], GameEvent::ItemTask(t) if t.action == 58));
}

#[test]
fn revive_method_and_scenario_names_parse() {
    assert_eq!(
        "repair".parse::<ReviveMethod>().unwrap(),
        ReviveMethod::Repair
    );
    assert_eq!(ReviveMethod::default(), ReviveMethod::Repair);
    assert_eq!(
        "skill".parse::<ReviveMethod>().unwrap(),
        ReviveMethod::Skill
    );
    assert_eq!("none".parse::<ReviveMethod>().unwrap(), ReviveMethod::None);
    assert!("x".parse::<ReviveMethod>().is_err());
    assert_eq!("edit".parse::<Kind>().unwrap(), Kind::Edit);
    assert_eq!("mate-revive".parse::<Kind>().unwrap(), Kind::MateRevive);
    assert_eq!("cometsboon".parse::<Kind>().unwrap(), Kind::CometsBoon);
}
