//! Doodad func item requirements are refused at cast start (1.2 stops a cast on a non-zero
//! `SCSkillStarted` result) and the Farm Wagon's eco fuel is checked when its mount skill is
//! pressed, before the mount animation plays.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::items::{ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::Item;
use crate::models::slave::Slave;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::templates::*;
use crate::systems::vehicles::data::{attach_point, MountAttachedSkills, MountSkills};
use crate::test_support::*;
use crate::world::{DoodadRef, Entity, PlayerRef, WorldObject};

use super::data::{effects::EffectTemplate, EffectKind, SkillItemAmount};
use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{cast_target_type, caster_type, SkillCastTarget, SkillCaster, SkillObject};

const PLAYER_OBJ: u32 = 0x100;
const RACK: u32 = 3446;
const RACK_OBJ: u32 = 0x200;
const EMPTY: u32 = 8264;
const LOADED: u32 = 8265;
const STORE_SKILL: u32 = 15307;
const PACK: u32 = 18469;
const WRONG_PACK: u32 = 18470;
const USE_EFFECT: u32 = 9_001;

const WAGON: u32 = 60;
const WAGON_OBJ: u32 = 0x300;
const MOUNT_SKILL: u32 = 20291;
const RIDER_SKILL: u32 = 20292;
const MOUNT_SKILL_ROW: u32 = 190;
const ECO_FUEL: u32 = 34583;
const ECO_FUEL_UNBOUND: u32 = 26548;
const FUEL_BUFF: u32 = 4841;
const FUEL_EFFECT: u32 = 26272;

const SHIP: u32 = 21;
const SHIP_MOUNT_SKILL: u32 = 22287;
const SHIP_RIDER_SKILL: u32 = 25632;
const SHIP_MOUNT_ROW: u32 = 345;
const SHIP_FUEL_COUNT: i32 = 10;

fn doodad_data() -> GameData {
    let mut data = crate::systems::economy::tests::test_data();
    for id in [PACK, WRONG_PACK] {
        data.items.templates.insert(
            id,
            ItemTemplate {
                kind: TemplateKind::Backpack { backpack_type: 1 },
                ..ItemTemplate::plain(id, 1)
            },
        );
    }
    let groups = vec![
        DoodadFuncGroups {
            id: EMPTY,
            almighty: RACK,
            group_kind_id: doodad_func_group_kind::START,
            ..Default::default()
        },
        DoodadFuncGroups {
            id: LOADED,
            almighty: RACK,
            group_kind_id: doodad_func_group_kind::NORMAL,
            ..Default::default()
        },
    ];
    let doodads = &mut data.systems.npcs.doodad;
    doodads.templates.insert(
        RACK,
        DoodadTemplate {
            id: RACK,
            func_groups: groups,
            ..Default::default()
        },
    );
    doodads.funcs_by_groups.insert(
        EMPTY,
        vec![DoodadFunc {
            group_id: EMPTY,
            func_id: 2,
            func_key: 7481,
            func_type: "DoodadFuncConsumeChanger".into(),
            next_phase: LOADED as i32,
            skill_id: STORE_SKILL,
            perm_id: 5,
            ..Default::default()
        }],
    );
    doodads
        .func_templates
        .entry("DoodadFuncConsumeChanger")
        .or_default()
        .insert(
            2,
            FuncTemplate::ConsumeChanger(DoodadFuncConsumeChanger {
                id: 2,
                slot_id: equip_slot::BACKPACK as u32,
                count: 1,
            }),
        );
    doodads
        .consume_changer_items
        .push(DoodadFuncConsumeChangerItem {
            id: 10,
            doodad_func_consume_changer_id: 2,
            item_id: PACK,
        });
    let skills = &mut data.systems.skills;
    skills.add_effect(
        EffectKind::InteractionEffect,
        EffectTemplate::Interaction {
            id: USE_EFFECT,
            world_interaction:
                crate::systems::npcs::doodad::interactions::world_interaction_type::USE as i32,
            doodad_id: 0,
        },
        USE_EFFECT,
    );
    let mut store = super::tests::skill_template(STORE_SKILL);
    store.target_type = super::types::SkillTargetType::Doodad as i32;
    store.casting_time = 0;
    store.damage_type_id = 0;
    store.effects.push(super::tests::skill_effect(
        USE_EFFECT,
        EffectKind::InteractionEffect,
        USE_EFFECT,
    ));
    skills.add_skill(store);
    data.skills.add_client_sourced_skill(STORE_SKILL);
    data
}

fn wagon_data() -> GameData {
    let mut data = crate::systems::economy::tests::test_data();
    for id in [ECO_FUEL, ECO_FUEL_UNBOUND] {
        data.items.templates.insert(id, ItemTemplate::plain(id, 20));
    }
    let skills = &mut data.systems.skills;
    skills.add_buff(super::data::BuffTemplate {
        id: FUEL_BUFF,
        kind: 1,
        duration: 480_000,
        ..Default::default()
    });
    skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(super::data::effects::BuffEffect {
            id: FUEL_EFFECT,
            buff_id: Some(FUEL_BUFF),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        FUEL_EFFECT,
    );
    let mut mount = super::tests::skill_template(MOUNT_SKILL);
    mount.target_type = 0;
    mount.casting_time = 0;
    mount.damage_type_id = 0;
    mount.effects.push(super::tests::skill_effect(
        FUEL_EFFECT,
        EffectKind::BuffEffect,
        FUEL_EFFECT,
    ));
    skills.add_skill(mount);
    let mut rider = super::tests::skill_template(RIDER_SKILL);
    rider.target_type = 0;
    rider.casting_time = 0;
    rider.damage_type_id = 0;
    rider.first_reagent_only = true;
    skills.add_skill(rider);
    skills.skill_reagents.push(SkillItemAmount {
        id: 1,
        skill_id: RIDER_SKILL,
        item_id: ECO_FUEL,
        amount: 1,
    });
    skills.skill_reagents.push(SkillItemAmount {
        id: 2,
        skill_id: RIDER_SKILL,
        item_id: ECO_FUEL_UNBOUND,
        amount: 1,
    });
    data.systems.vehicles.mount_skills.push(MountSkills {
        id: MOUNT_SKILL_ROW,
        name: "Farm Wagon".into(),
        skill_id: MOUNT_SKILL,
    });
    data.systems
        .vehicles
        .slave_mount_skills
        .push((181, WAGON, MOUNT_SKILL_ROW));
    data.systems
        .vehicles
        .mount_attached_skills
        .push(MountAttachedSkills {
            id: 302,
            mount_skill_id: MOUNT_SKILL_ROW,
            attach_point_id: attach_point::DRIVER,
            skill_id: RIDER_SKILL,
        });
    data
}

/// The real ship speed boost row 345 (`mount_skills` 345 -> 22287, rider 25632 x10 fuel).
fn ship_data() -> GameData {
    let mut data = wagon_data();
    let skills = &mut data.systems.skills;
    let mut mount = super::tests::skill_template(SHIP_MOUNT_SKILL);
    mount.target_type = 0;
    mount.casting_time = 0;
    mount.damage_type_id = 0;
    skills.add_skill(mount);
    let mut rider = super::tests::skill_template(SHIP_RIDER_SKILL);
    rider.target_type = 0;
    rider.casting_time = 0;
    rider.damage_type_id = 0;
    rider.first_reagent_only = true;
    skills.add_skill(rider);
    for (id, item) in [(20, ECO_FUEL), (21, ECO_FUEL_UNBOUND)] {
        skills.skill_reagents.push(SkillItemAmount {
            id,
            skill_id: SHIP_RIDER_SKILL,
            item_id: item,
            amount: SHIP_FUEL_COUNT as i16,
        });
    }
    data.systems.vehicles.mount_skills.push(MountSkills {
        id: SHIP_MOUNT_ROW,
        name: "Ship boost".into(),
        skill_id: SHIP_MOUNT_SKILL,
    });
    data.systems
        .vehicles
        .slave_mount_skills
        .push((345, SHIP, SHIP_MOUNT_ROW));
    data.systems
        .vehicles
        .mount_attached_skills
        .push(MountAttachedSkills {
            id: 345,
            mount_skill_id: SHIP_MOUNT_ROW,
            attach_point_id: attach_point::DRIVER,
            skill_id: SHIP_RIDER_SKILL,
        });
    data
}

struct Fixture {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    rack: DoodadRef,
}

fn fixture() -> Fixture {
    let server = Arc::new(test_server_with(doodad_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tester", 100.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let instance = server.world.world(0).unwrap();
    let rack = manager::create(&server, &instance, RACK_OBJ, RACK, None, true).expect("rack");
    rack.data().transform.set_xyz(101.0, 100.0, 100.0);
    instance.spawn(&rack, &server.view_ctx());
    client.drain();
    Fixture {
        server,
        client,
        player,
        rack,
    }
}

/// Puts `template_id` into `st` at `slot` and registers the container for `server.items`.
fn give_at(
    server: &GameServer,
    player: &PlayerRef,
    item_id: u64,
    template_id: u32,
    st: u8,
    slot: i32,
) -> u64 {
    let template = server.game_data.items.template(template_id).expect("item");
    let mut item = Item::create(item_id, template, 1, 0, 1, &server.game_data.items, 0);
    item.owner_id = u64::from(player.character_id);
    let container = Inv::of(player).get(st).expect("container");
    container
        .lock()
        .unwrap()
        .add_new_item(item, slot, &server.game_data.items)
        .expect("add item");
    server.items.add_container(container);
    item_id
}

fn store_cast(doodad_obj: u32) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id: STORE_SKILL,
        caster: SkillCaster::Unit(PLAYER_OBJ),
        target: SkillCastTarget::Doodad(doodad_obj),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

/// A byte-exact failed `SCSkillStartedPacket` for a unit caster / doodad target (no skill
/// object).
fn expected_refused(skill: u32, doodad_obj: u32, result: u8, value: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(skill)
        .write_u16(0)
        .write_u8(caster_type::UNIT)
        .write_bc(PLAYER_OBJ)
        .write_u8(cast_target_type::DOODAD)
        .write_bc(doodad_obj)
        .write_u8(0)
        .write_u16(0)
        .write_u16(0)
        .write_u8(0);
    let mut flags = 0u8;
    if result != 0 {
        flags |= 1;
    }
    if value != 0 {
        flags |= 4;
    }
    s.write_u8(flags);
    if result != 0 {
        s.write_u8(result);
    }
    if value != 0 {
        s.write_u32(value);
    }
    s.into_bytes()
}

fn error_body(error: i16) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_i16(error)
        .write_i16(error)
        .write_u32(0)
        .write_bool(true);
    s.into_bytes()
}

fn has_error(frames: &[(u16, Vec<u8>)], error: i16) -> bool {
    frames
        .iter()
        .any(|(op, body)| *op == sc::SCErrorMsgPacket && *body == error_body(error))
}

fn started_results(frames: &[(u16, Vec<u8>)], skill: u32) -> Vec<u8> {
    started_results_and_value(frames, skill)
        .into_iter()
        .map(|(result, _)| result)
        .collect()
}

fn started_value(frames: &[(u16, Vec<u8>)], skill: u32) -> Option<u32> {
    started_results_and_value(frames, skill)
        .into_iter()
        .find_map(|(_, value)| value)
}

fn started_results_and_value(frames: &[(u16, Vec<u8>)], skill: u32) -> Vec<(u8, Option<u32>)> {
    frames
        .iter()
        .filter(|(op, _)| *op == sc::SCSkillStartedPacket)
        .filter_map(|(_, body)| {
            let mut s = PacketStream::from_bytes(body.clone());
            (s.read_u32().ok() == Some(skill)).then_some(())?;
            let _ = s.read_u16();
            match s.read_u8().ok()? {
                caster_type::UNIT | caster_type::UNK1 | caster_type::DOODAD => {
                    let _ = s.read_bc();
                }
                caster_type::ITEM => {
                    let _ = s.read_bc();
                    let _ = s.read_u64();
                    let _ = s.read_u32();
                    let _ = s.read_u8();
                    let _ = s.read_u32();
                }
                caster_type::MOUNT => {
                    let _ = s.read_bc();
                    let _ = s.read_u32();
                }
                _ => return None,
            }
            match s.read_u8().ok()? {
                cast_target_type::UNIT | cast_target_type::DOODAD => {
                    let _ = s.read_bc();
                }
                cast_target_type::POSITION => {
                    let _ = s.read_i64();
                    let _ = s.read_i64();
                    let _ = s.read_f32();
                    let _ = s.read_f32();
                    let _ = s.read_bc();
                    let _ = s.read_bc();
                }
                cast_target_type::POSITION2 => {
                    let _ = s.read_i64();
                    let _ = s.read_i64();
                    let _ = s.read_f32();
                    for _ in 0..6 {
                        let _ = s.read_f32();
                    }
                }
                cast_target_type::ITEM => {
                    let _ = s.read_bc();
                    let _ = s.read_u64();
                    let _ = s.read_u32();
                    let _ = s.read_u8();
                }
                cast_target_type::POSITION3 => {
                    let _ = s.read_i64();
                    let _ = s.read_i64();
                    let _ = s.read_f32();
                    let _ = s.read_f32();
                }
                _ => return None,
            }
            let _ = s.read_u8(); // skill object
            let _ = s.read_u16();
            let _ = s.read_u16();
            let _ = s.read_u8();
            let flags = s.read_u8().ok()?;
            let result = if flags & 1 != 0 { s.read_u8().ok()? } else { 0 };
            if flags & 2 != 0 {
                let _ = s.read_u16();
            }
            let value = if flags & 4 != 0 {
                Some(s.read_u32().ok()?)
            } else {
                None
            };
            Some((result, value))
        })
        .collect()
}

/// No trade pack on the back: the storage skill is refused before the cast instead of playing
/// the bar and animation and failing in the effect.
#[tokio::test]
async fn storage_skill_without_a_pack_is_refused_at_cast_start() {
    let mut f = fixture();
    let rack_obj = f.rack.obj_id;
    start_skill(&f.server, &mut f.client.conn, store_cast(rack_obj));
    let frames = f.client.drain();
    assert_eq!(
        started_results(&frames, STORE_SKILL),
        vec![0x24],
        "{frames:x?}"
    );
    assert_eq!(
        frames
            .iter()
            .find(|(op, _)| *op == sc::SCSkillStartedPacket)
            .map(|(_, body)| body.clone()),
        Some(expected_refused(STORE_SKILL, rack_obj, 0x24, 0))
    );
    assert!(
        has_error(&frames, error_message::UNKNOWN_ITEM),
        "{frames:x?}"
    );
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    let d = f.rack.data();
    assert_eq!(d.func_group_id, EMPTY);
    assert_eq!((d.item_id, d.item_template_id), (0, 0));
    assert!(f.player.data().unit.skill_state.skill_task.is_none());
}

/// The accepted pack still stores as before: the cast starts, the phase advances and the pack
/// moves to the rack.
#[tokio::test]
async fn storage_skill_with_an_accepted_pack_stores_it() {
    let mut f = fixture();
    let item_id = give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100,
        PACK,
        slot_type::EQUIPMENT,
        equip_slot::BACKPACK,
    );
    start_skill(&f.server, &mut f.client.conn, store_cast(f.rack.obj_id));
    let frames = f.client.drain();
    assert!(
        !has_error(&frames, error_message::UNKNOWN_ITEM),
        "{frames:x?}"
    );
    assert!(
        !has_error(&frames, error_message::STORE_INVALID_ITEM),
        "{frames:x?}"
    );
    assert!(
        started_results(&frames, STORE_SKILL).is_empty(),
        "{frames:x?}"
    );
    let d = f.rack.data();
    assert_eq!(d.func_group_id, LOADED);
    assert_eq!((d.item_id, d.item_template_id), (item_id, PACK));
    assert!(Inv::of(&f.player).equipped(equip_slot::BACKPACK).is_none());
}

/// A pack that is not on the func's list is refused at cast start with the same rule.
#[tokio::test]
async fn storage_skill_with_a_wrong_pack_is_refused_at_cast_start() {
    let mut f = fixture();
    let item_id = give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100 + 1,
        WRONG_PACK,
        slot_type::EQUIPMENT,
        equip_slot::BACKPACK,
    );
    start_skill(&f.server, &mut f.client.conn, store_cast(f.rack.obj_id));
    let frames = f.client.drain();
    assert_eq!(
        started_results(&frames, STORE_SKILL),
        vec![0x24],
        "{frames:x?}"
    );
    assert!(
        has_error(&frames, error_message::STORE_INVALID_ITEM),
        "{frames:x?}"
    );
    let d = f.rack.data();
    assert_eq!(d.func_group_id, EMPTY);
    assert_eq!((d.item_id, d.item_template_id), (0, 0));
    assert_eq!(
        Inv::of(&f.player)
            .equipped(equip_slot::BACKPACK)
            .map(|i| i.id),
        Some(item_id)
    );
}

struct WagonFixture {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    wagon: crate::world::SlaveRef,
}

fn wagon_fixture() -> WagonFixture {
    vehicle_fixture(wagon_data(), WAGON)
}

fn ship_fixture() -> WagonFixture {
    vehicle_fixture(ship_data(), SHIP)
}

fn vehicle_fixture(data: GameData, slave_template: u32) -> WagonFixture {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Driver", 100.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let instance = server.world.world(0).unwrap();
    let mut wagon = Slave::new(WAGON_OBJ, slave_template);
    wagon.summoner_id = 1;
    wagon.summoner_obj_id = player.obj_id;
    wagon.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let wagon = Entity::new(WAGON_OBJ, wagon);
    instance.spawn(&wagon, &server.view_ctx());
    {
        let mut p = player.data();
        p.attached_point = attach_point::DRIVER;
        p.is_riding = true;
        p.unit.transform.parent_obj_id = wagon.obj_id;
    }
    client.drain();
    WagonFixture {
        server,
        client,
        player,
        wagon,
    }
}

fn wagon_cast() -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id: MOUNT_SKILL,
        caster: SkillCaster::Mount {
            obj_id: WAGON_OBJ,
            mount_skill_template_id: MOUNT_SKILL_ROW,
        },
        target: SkillCastTarget::Unit(WAGON_OBJ),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

/// No eco fuel: the wagon's speed skill is refused before the cast and the rider skill never
/// runs.
#[tokio::test]
async fn wagon_eco_fuel_without_the_item_is_refused_at_cast_start() {
    let mut f = wagon_fixture();
    start_skill(&f.server, &mut f.client.conn, wagon_cast());
    let frames = f.client.drain();
    assert_eq!(
        started_results(&frames, MOUNT_SKILL),
        vec![0x24],
        "{frames:x?}"
    );
    assert_eq!(started_value(&frames, MOUNT_SKILL), Some(ECO_FUEL));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    assert!(f.player.data().unit.skill_state.skill_task.is_none());
    assert!(!f.wagon.data().unit.buffs.check_buff(FUEL_BUFF));
    assert_eq!(Inv::of(&f.player).items_count(ECO_FUEL, -1), 0);
    assert_eq!(Inv::of(&f.player).items_count(ECO_FUEL_UNBOUND, -1), 0);
    let _ = WorldObject::from(&f.wagon);
}

/// With one eco fuel the mount skill runs and its rider skill consumes the item as before.
#[tokio::test]
async fn wagon_eco_fuel_with_the_item_cast_and_consumes_it() {
    let mut f = wagon_fixture();
    give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100 + 2,
        ECO_FUEL,
        slot_type::INVENTORY,
        0,
    );
    start_skill(&f.server, &mut f.client.conn, wagon_cast());
    let frames = f.client.drain();
    assert!(
        !started_results(&frames, MOUNT_SKILL).contains(&0x24),
        "{frames:x?}"
    );
    assert!(
        started_results(&frames, RIDER_SKILL).is_empty(),
        "{frames:x?}"
    );
    assert!(f.wagon.data().unit.buffs.check_buff(FUEL_BUFF));
    assert_eq!(Inv::of(&f.player).items_count(ECO_FUEL, -1), 0);
}

fn ship_cast() -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id: SHIP_MOUNT_SKILL,
        caster: SkillCaster::Mount {
            obj_id: WAGON_OBJ,
            mount_skill_template_id: SHIP_MOUNT_ROW,
        },
        target: SkillCastTarget::Unit(WAGON_OBJ),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

/// A ship bar entry (row 345 -> 22287, rider 25632 x10 fuel) uses the same pre-check.
#[tokio::test]
async fn ship_speed_boost_without_fuel_is_refused_before_the_slave_cast() {
    let mut f = ship_fixture();
    start_skill(&f.server, &mut f.client.conn, ship_cast());
    let frames = f.client.drain();
    assert_eq!(
        started_results(&frames, SHIP_MOUNT_SKILL),
        vec![0x24],
        "{frames:x?}"
    );
    assert_eq!(started_value(&frames, SHIP_MOUNT_SKILL), Some(ECO_FUEL));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    assert_eq!(Inv::of(&f.player).items_count(ECO_FUEL, -1), 0);
}

#[tokio::test]
async fn ship_speed_boost_with_fuel_consumes_ten() {
    let mut f = ship_fixture();
    for slot in 0..SHIP_FUEL_COUNT {
        give_at(
            &f.server,
            &f.player,
            PLAYER_OBJ as u64 * 100 + 20 + slot as u64,
            ECO_FUEL,
            slot_type::INVENTORY,
            slot,
        );
    }
    start_skill(&f.server, &mut f.client.conn, ship_cast());
    let frames = f.client.drain();
    assert!(
        !started_results(&frames, SHIP_MOUNT_SKILL).contains(&0x24),
        "{frames:x?}"
    );
    assert_eq!(Inv::of(&f.player).items_count(ECO_FUEL, -1), 0);
}
