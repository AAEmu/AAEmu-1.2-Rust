//! Client buff cancellation may only change the player's own companions and vehicles.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::GameData;
use crate::models::slave::Slave;
use crate::net::registry::ClientPacket;
use crate::test_support::*;
use crate::world::{Entity, WorldObject};

use super::data::BuffTemplate;
use super::handlers::CSRemoveBuffPacket;

fn mate_cast_data() -> GameData {
    use super::data::effects::{EffectTemplate, HealEffect};
    use super::data::EffectKind;
    let mut data = GameData::empty();
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "1000", 14, 5), (2, "1000", 15, 5)],
        &[],
    );
    data.systems.skills.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(HealEffect {
            id: 1,
            use_fixed_heal: true,
            fixed_min: 150,
            fixed_max: 150,
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
        900001,
    );
    let mut skill = super::tests::skill_template(20001);
    skill.target_type = 0;
    skill.damage_type_id = 0;
    skill.mana_cost = 25;
    skill.cancel_ongoing_buffs = true;
    skill.effects.push(super::tests::skill_effect(
        900001,
        EffectKind::HealEffect,
        1,
    ));
    data.systems.skills.add_skill(skill);
    data.systems.skills.add_buff(BuffTemplate {
        id: 50002,
        kind: 1,
        duration: 0,
        stack_rule: 1,
        remove_on_start_skill: true,
        ..Default::default()
    });
    // The vehicle bar: mount skill 77 casts 20001, both as a mate bar entry
    // (`Mate.skills`) and as a slave template entry (`slave_mount_skills`).
    data.systems
        .vehicles
        .mount_skills
        .push(crate::systems::vehicles::data::MountSkills {
            id: 77,
            name: "Test".into(),
            skill_id: 20001,
        });
    data.systems
        .vehicles
        .npc_mount_skills
        .push(crate::systems::vehicles::data::NpcMountSkills {
            id: 1,
            npc_id: 1,
            mount_skill_id: 77,
        });
    data.systems.vehicles.slave_mount_skills.push((1, 1, 77));
    data
}

fn mate_cast_packet(obj_id: u32) -> super::handlers::CSStartSkillPacket {
    let mut body = PacketStream::new();
    body.write_u32(20001)
        .write_u8(super::types::caster_type::MOUNT)
        .write_bc(obj_id)
        .write_u32(77)
        .write_u8(0)
        .write_bc(obj_id)
        .write_u8(0);
    let mut body = PacketStream::from_bytes(body.into_bytes());
    super::handlers::CSStartSkillPacket::read(&mut body).unwrap()
}

#[tokio::test]
async fn mate_skill_packet_rejects_foreign_actor_before_costs_and_allows_unmounted_owner() {
    use crate::models::mate::Mate;
    let server = Arc::new(test_server_with(mate_cast_data()));
    super::bind(&server);
    let mut foreign = TestClient::new(&server, 1, 1);
    let mut owner = TestClient::new(&server, 2, 2);
    let a = spawn_player(
        &server,
        &foreign,
        test_character(1, "Foreign", 100.0, 100.0),
        0x100,
    );
    let b = spawn_player(
        &server,
        &owner,
        test_character(2, "Owner", 102.0, 100.0),
        0x101,
    );
    foreign.conn.active_char = Some(a);
    owner.conn.active_char = Some(b.clone());
    let mut data = Mate::new(1, 1);
    data.skills = vec![77];
    data.owner_obj_id = b.obj_id;
    data.unit.hp = 500;
    data.unit.mp = 100;
    data.unit.transform.set_xyz(103.0, 100.0, 100.0);
    let mate = Entity::new(0x201, data);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&mate, &server.view_ctx());
    let caster = WorldObject::from(&mate);
    super::buffs::add_buff_by_id(&server, &caster, 50002, &caster, 0);
    mate_cast_packet(mate.obj_id)
        .handle(&server, &mut foreign.conn)
        .await
        .unwrap();
    {
        let data = mate.data();
        assert_eq!(
            (data.unit.hp, data.unit.mp, data.unit.skill_last_used_ms),
            (500, 100, 0)
        );
        assert!(data.unit.buffs.check_buff(50002));
    }
    assert!(!b.data().is_riding);
    mate_cast_packet(mate.obj_id)
        .handle(&server, &mut owner.conn)
        .await
        .unwrap();
    let data = mate.data();
    assert_eq!((data.unit.hp, data.unit.mp), (650, 75));
    assert!(data.unit.skill_last_used_ms > 0);
    assert!(!data.unit.buffs.check_buff(50002));
}

#[tokio::test]
async fn mate_skill_packet_requires_real_passenger_attachment_and_keeps_unlisted_skills() {
    use crate::data::items::{ItemTemplate, TemplateKind};
    use crate::models::container::slot_type;
    use crate::models::item::Item;
    use crate::systems::npcs::npc::data::NpcTemplate;
    use crate::systems::vehicles::{data::attach_point, mates};
    let mut data = mate_cast_data();
    data.systems.npcs.npc.templates.insert(
        900,
        NpcTemplate {
            id: 900,
            name: "Horse".into(),
            level: 1,
            scale: 1.0,
            ..Default::default()
        },
    );
    data.systems
        .vehicles
        .npc_mount_skills
        .push(crate::systems::vehicles::data::NpcMountSkills {
            id: 2,
            npc_id: 900,
            mount_skill_id: 77,
        });
    data.items.templates.insert(
        5000,
        ItemTemplate {
            kind: TemplateKind::SummonMate { npc_id: 900 },
            ..ItemTemplate::plain(5000, 1)
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let owner_client = TestClient::new(&server, 2, 2);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Passenger", 100.0, 100.0),
        0x00F0_0001,
    );
    let owner = spawn_player(
        &server,
        &owner_client,
        test_character(2, "Owner", 102.0, 100.0),
        0x00F0_0002,
    );
    client.conn.active_char = Some(player.clone());
    let template = server.game_data.items.template(5000).unwrap();
    let mut item = Item::create(0x7002, template, 1, 0, 1, &server.game_data.items, 0);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    let bag = owner
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .cloned()
        .unwrap();
    bag.lock().unwrap().items.push(item);
    mates::spawn_mount(&server, &owner, 0x7002);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(2)
        .pop()
        .unwrap();
    let tl_id = {
        let mut data = mate.data();
        data.unit.hp = 500;
        data.unit.mp = 100;
        data.passenger_mut(attach_point::PASSENGER0).unwrap().obj_id = player.obj_id;
        u32::from(data.unit.tl_id)
    };
    // A stale registry entry without the actual riding relation is insufficient.
    {
        let mut p = player.data();
        p.attached_point = attach_point::PASSENGER0;
        p.is_riding = true;
    }
    mate_cast_packet(mate.obj_id)
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    {
        let data = mate.data();
        assert_eq!((data.unit.hp, data.unit.mp), (500, 100));
    }
    // Establish the normal server-managed passenger seat and parent relation.
    mate.data()
        .passenger_mut(attach_point::PASSENGER0)
        .unwrap()
        .obj_id = 0;
    mates::mount_mate(&server, &player, tl_id, attach_point::PASSENGER0, 0);
    assert_eq!(player.data().unit.transform.parent_obj_id, mate.obj_id);
    assert_eq!(mate.data().skills, vec![77]);
    mate_cast_packet(mate.obj_id)
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    let data = mate.data();
    assert_eq!((data.unit.hp, data.unit.mp), (650, 75));
}

/// `CSStartSkillPacket` with the given caster type, self-targeted on `target_obj`.
fn cast_packet_as(
    caster_type: u8,
    caster_obj: u32,
    target_obj: u32,
) -> super::handlers::CSStartSkillPacket {
    let mut body = PacketStream::new();
    body.write_u32(20001)
        .write_u8(caster_type)
        .write_bc(caster_obj);
    if caster_type == super::types::caster_type::MOUNT {
        body.write_u32(77);
    }
    body.write_u8(0).write_bc(target_obj).write_u8(0);
    let mut body = PacketStream::from_bytes(body.into_bytes());
    super::handlers::CSStartSkillPacket::read(&mut body).unwrap()
}

macro_rules! hp_mp {
    ($e:expr) => {{
        let d = $e.data();
        (d.unit.hp, d.unit.mp)
    }};
}

fn started_results(client: &mut TestClient) -> usize {
    client
        .drain_ops()
        .into_iter()
        .filter(|op| *op == crate::opcodes::sc::SCSkillStartedPacket)
        .count()
}

#[tokio::test]
async fn slave_skill_packet_requires_owner_or_attached_character() {
    let server = Arc::new(test_server_with(mate_cast_data()));
    super::bind(&server);
    let mut stranger = TestClient::new(&server, 1, 1);
    let owner_client = TestClient::new(&server, 2, 2);
    let a = spawn_player(
        &server,
        &stranger,
        test_character(1, "Stranger", 100.0, 100.0),
        0x100,
    );
    let b = spawn_player(
        &server,
        &owner_client,
        test_character(2, "Owner", 102.0, 100.0),
        0x101,
    );
    stranger.conn.active_char = Some(a.clone());
    let mut data = Slave::new(0x300, 1);
    data.summoner_id = 2;
    data.summoner_obj_id = b.obj_id;
    data.unit.hp = 500;
    data.unit.mp = 100;
    data.unit.transform.set_xyz(103.0, 100.0, 100.0);
    let slave = Entity::new(0x300, data);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&slave, &server.view_ctx());
    stranger.drain();
    // A foreign slave is refused: no cost, no effect, and the client is told.
    cast_packet_as(super::types::caster_type::MOUNT, slave.obj_id, slave.obj_id)
        .handle(&server, &mut stranger.conn)
        .await
        .unwrap();
    assert_eq!(hp_mp!(slave), (500, 100));
    assert_eq!(started_results(&mut stranger), 1);
    // Attached to it (driver / gunner seat): allowed.
    slave.data().attached_characters.push((1, a.obj_id));
    cast_packet_as(super::types::caster_type::MOUNT, slave.obj_id, slave.obj_id)
        .handle(&server, &mut stranger.conn)
        .await
        .unwrap();
    assert_eq!(hp_mp!(slave).1, 75);
    // Its owner is allowed without sitting on it.
    slave.data().attached_characters.clear();
    let mut owner = TestClient::new(&server, 2, 2);
    owner.conn.active_char = Some(b);
    cast_packet_as(super::types::caster_type::MOUNT, slave.obj_id, slave.obj_id)
        .handle(&server, &mut owner.conn)
        .await
        .unwrap();
    assert_eq!(hp_mp!(slave).1, 50);
}

#[tokio::test]
async fn mount_caster_packet_cannot_name_other_players_or_npcs() {
    use crate::models::npc::Npc;
    let server = Arc::new(test_server_with(mate_cast_data()));
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let other_client = TestClient::new(&server, 2, 2);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Attacker", 100.0, 100.0),
        0x100,
    );
    let b = spawn_player(
        &server,
        &other_client,
        test_character(2, "Victim", 102.0, 100.0),
        0x101,
    );
    client.conn.active_char = Some(a);
    b.data().unit.hp = 500;
    b.data().unit.mp = 100;
    let mut n = Npc::new(1);
    n.unit.hp = 500;
    n.unit.mp = 100;
    n.unit.transform.set_xyz(103.0, 100.0, 100.0);
    let npc = Entity::new(0x201, n);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    cast_packet_as(super::types::caster_type::MOUNT, b.obj_id, b.obj_id)
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    cast_packet_as(super::types::caster_type::MOUNT, npc.obj_id, npc.obj_id)
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert_eq!(hp_mp!(b), (500, 100));
    assert_eq!(hp_mp!(npc), (500, 100));
}

#[tokio::test]
async fn fallback_cast_needs_a_player_source_for_the_skill() {
    let mut data = mate_cast_data();
    let mut npc_only = super::tests::skill_template(20002);
    npc_only.target_type = 0;
    npc_only.damage_type_id = 0;
    npc_only.effects.push(super::tests::skill_effect(
        900001,
        super::data::EffectKind::HealEffect,
        1,
    ));
    data.systems.skills.add_skill(npc_only);
    // 20001 is handed to players by the static data, 20002 is not.
    data.skills.add_client_sourced_skill(20001);
    let server = Arc::new(test_server_with(data));
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    {
        let mut c = a.data();
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        c.unit.mp = 100;
    }
    let packet = |skill: u32| {
        let mut p = cast_packet_as(super::types::caster_type::UNIT, a.obj_id, a.obj_id);
        p.skill_id = skill;
        p
    };
    super::handlers::start_skill(&server, &mut client.conn, packet(20002));
    assert_eq!(a.data().unit.hp, 500);
    assert_eq!(started_results(&mut client), 1);
    super::handlers::start_skill(&server, &mut client.conn, packet(20001));
    assert_eq!(a.data().unit.hp, 650);
}

#[tokio::test]
async fn remove_buff_packet_preserves_foreign_slave_buff_and_removes_owned_slave_buff() {
    let mut data = GameData::empty();
    data.systems.skills.add_buff(BuffTemplate {
        id: 50001,
        kind: 1,
        duration: 0,
        stack_rule: 1,
        ..Default::default()
    });
    let server = Arc::new(test_server_with(data));
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Owner", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).expect("world");
    let caster = WorldObject::from(&player);
    for (obj_id, summoner_id, removable) in [(0x200, 2, false), (0x201, 1, true)] {
        let mut data = Slave::new(obj_id, 1);
        data.summoner_id = summoner_id;
        data.unit.transform.set_xyz(103.0, 100.0, 100.0);
        let slave = Entity::new(obj_id, data);
        world.spawn(&slave, &server.view_ctx());
        let owner = WorldObject::from(&slave);
        super::buffs::add_buff_by_id(&server, &owner, 50001, &caster, 0);
        let index = slave.data().unit.buffs.effects[0].index;
        let mut body = PacketStream::new();
        body.write_bc(obj_id).write_u32(index).write_u8(0);
        let packet = CSRemoveBuffPacket::read(&mut body).expect("remove buff body");
        packet
            .handle(&server, &mut client.conn)
            .await
            .expect("remove buff");
        assert_eq!(slave.data().unit.buffs.check_buff(50001), !removable);
    }
}

#[test]
fn start_skill_rejects_unknown_caster_type() {
    // Unknown wire discriminants already fail before the skill-use permission branches.
    let mut body = PacketStream::from_bytes(vec![1, 0, 0, 0, 255]);
    assert!(super::handlers::CSStartSkillPacket::read(&mut body).is_err());
}

/// A `SkillData` whose client-cast sources come from an in-memory `compact.sqlite3` shaped
/// database: the real loader decides which skills are client sourced and item only.
fn loaded_skill_sources(rows: &str) -> crate::data::skills::SkillData {
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory compact");
    conn.execute_batch(&format!(
        "CREATE TABLE skills (id INTEGER, show TEXT, ability_id INTEGER, ability_level INTEGER, \
             level_step INTEGER, skill_points INTEGER, auto_learn TEXT, need_learn TEXT);
         CREATE TABLE default_skills (skill_id INTEGER, slot_index INTEGER, add_to_slot TEXT);
         CREATE TABLE passive_buffs (id INTEGER, ability_id INTEGER, level INTEGER, \
             buff_id INTEGER, req_points INTEGER, active TEXT);
         CREATE TABLE buffs (id INTEGER, kind_id INTEGER, duration INTEGER, level_duration INTEGER, \
             tick REAL, stack_rule_id INTEGER, max_stack INTEGER, init_min_charge INTEGER);
         CREATE TABLE effects (id INTEGER, actual_type TEXT, actual_id INTEGER);
         CREATE TABLE skill_effects (skill_id INTEGER, effect_id INTEGER);
         CREATE TABLE special_effects (id INTEGER, special_effect_type_id INTEGER);
         CREATE TABLE items (id INTEGER, use_skill_id INTEGER);
         CREATE TABLE item_grade_skills (skill_id INTEGER);
         CREATE TABLE doodad_funcs (func_skill_id INTEGER);
         {rows}"
    ))
    .expect("compact schema");
    crate::data::skills::SkillData::load(&conn).expect("skill data")
}

/// The `SCSkillStarted` result of `skill` when the packet carries a unit caster, a unit target
/// and no skill object (the shape these tests use).
fn started_result(client: &mut TestClient, skill: u32) -> Option<u8> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCSkillStartedPacket)
        .find_map(|(_, body)| {
            let mut s = PacketStream::from_bytes(body);
            if s.read_u32().ok() != Some(skill) {
                return None;
            }
            let _ = s.read_u16();
            let _ = s.read_u8();
            let _ = s.read_bc();
            let _ = s.read_u8();
            let _ = s.read_bc();
            let _ = s.read_u8();
            let _ = s.read_u16();
            let _ = s.read_u16();
            let _ = s.read_u8();
            let flags = s.read_u8().ok()?;
            if flags & 1 == 0 {
                return None;
            }
            s.read_u8().ok()
        })
}

/// Like [`started_result`] for a packet whose caster is an `Item` and whose target is a unit.
fn item_started_result(client: &mut TestClient, skill: u32) -> Option<u8> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCSkillStartedPacket)
        .find_map(|(_, body)| {
            let mut s = PacketStream::from_bytes(body);
            if s.read_u32().ok() != Some(skill) {
                return None;
            }
            let _ = s.read_u16();
            let caster_type = s.read_u8().ok()?;
            if caster_type != super::types::caster_type::ITEM {
                return None;
            }
            let _ = s.read_bc();
            let _ = s.read_u64();
            let _ = s.read_u32();
            let _ = s.read_u8();
            let _ = s.read_u32();
            let _ = s.read_u8();
            let _ = s.read_bc();
            let _ = s.read_u8();
            let _ = s.read_u16();
            let _ = s.read_u16();
            let _ = s.read_u8();
            let flags = s.read_u8().ok()?;
            if flags & 1 == 0 {
                return None;
            }
            s.read_u8().ok()
        })
}

/// Adds one item of `template_id` x `count` to the player's bag and registers its container.
fn give_test_item(
    server: &crate::GameServer,
    player: &crate::world::PlayerRef,
    obj_id: u64,
    template_id: u32,
    count: i32,
) {
    use crate::models::container::slot_type;
    use crate::models::item::Item;
    use crate::systems::economy::inventory::Inv;
    let template = server.game_data.items.template(template_id).expect("item");
    let mut item = Item::create(obj_id, template, count, 0, 1, &server.game_data.items, 0);
    item.owner_id = u64::from(player.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    let bag = Inv::of(player).bag();
    bag.lock().unwrap().items.push(item);
    server.items.add_container(bag);
}

/// 26611 "Full XP Restoration": its scrolls' use skill, reachable through nothing else.
#[tokio::test]
async fn item_only_skill_is_refused_as_a_plain_cast_and_consumed_on_an_item_cast() {
    use super::types::{caster_type, SkillCastTarget, SkillCaster, SkillObject};
    use crate::data::items::ItemTemplate;
    use crate::systems::economy::inventory::Inv;

    const ITEM_ONLY: u32 = 20_003;
    const SCROLL: u32 = 6_001;
    let mut data = mate_cast_data();
    let mut skill = super::tests::skill_template(ITEM_ONLY);
    skill.target_type = 0;
    skill.damage_type_id = 0;
    let mut effect = super::tests::skill_effect(900_001, super::data::EffectKind::HealEffect, 1);
    effect.consume_item_count = 1;
    skill.effects.push(effect);
    data.systems.skills.add_skill(skill);
    let mut item = ItemTemplate::plain(SCROLL, 10);
    item.use_skill_id = ITEM_ONLY;
    item.use_skill_as_reagent = true;
    data.items.templates.insert(SCROLL, item);
    data.skills = loaded_skill_sources(&format!(
        "INSERT INTO items (id, use_skill_id) VALUES ({SCROLL}, {ITEM_ONLY});"
    ));
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    {
        let mut c = a.data();
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        c.unit.mp = 100;
    }
    let mut packet = cast_packet_as(caster_type::UNIT, a.obj_id, a.obj_id);
    packet.skill_id = ITEM_ONLY;
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 500);
    assert_eq!(
        started_result(&mut client, ITEM_ONLY),
        Some(super::types::skill_result::NO_PERM)
    );
    // The real item path still runs the skill and uses the scroll up.
    give_test_item(&server, &a, 0x7001, SCROLL, 2);
    let packet = super::handlers::CSStartSkillPacket {
        skill_id: ITEM_ONLY,
        caster: SkillCaster::Item {
            obj_id: a.obj_id,
            item_id: 0x7001,
            // The client's hint is wrong on purpose: the server must use 6001.
            item_template_id: 0,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(a.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    };
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 650);
    assert_eq!(Inv::of(&a).items_count(SCROLL, -1), 1);
    // A second cast uses the last scroll; a third without a scroll is refused.
    let mut packet = cast_packet_as(caster_type::UNIT, a.obj_id, a.obj_id);
    packet.skill_id = ITEM_ONLY;
    packet.caster = SkillCaster::Item {
        obj_id: a.obj_id,
        item_id: 0x7001,
        item_template_id: 0,
        type1: 0,
        type2: 0,
    };
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 800);
    assert_eq!(Inv::of(&a).items_count(SCROLL, -1), 0);
    let mut packet = cast_packet_as(caster_type::UNIT, a.obj_id, a.obj_id);
    packet.skill_id = ITEM_ONLY;
    packet.caster = SkillCaster::Item {
        obj_id: a.obj_id,
        item_id: 0x7001,
        item_template_id: 0,
        type1: 0,
        type2: 0,
    };
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 800);
}

/// A bind-on-pickup item cannot authorize an item-only skill other than its own use skill.
#[tokio::test]
async fn bind_on_pickup_item_does_not_authorize_another_item_only_skill() {
    use super::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::data::items::{bind_type, ItemTemplate};

    const ITEM_ONLY: u32 = 20_003;
    const OTHER_ITEM_ONLY: u32 = 20_004;
    const BOP_ITEM: u32 = 6_002;
    let mut data = mate_cast_data();
    let mut skill = super::tests::skill_template(ITEM_ONLY);
    skill.target_type = 0;
    skill.damage_type_id = 0;
    skill.effects.push(super::tests::skill_effect(
        900_001,
        super::data::EffectKind::HealEffect,
        1,
    ));
    data.systems.skills.add_skill(skill);
    let mut item = ItemTemplate::plain(BOP_ITEM, 10);
    item.bind_type = bind_type::BIND_ON_PICKUP;
    item.use_skill_id = OTHER_ITEM_ONLY;
    item.use_skill_as_reagent = true;
    data.items.templates.insert(BOP_ITEM, item);
    data.skills = loaded_skill_sources(&format!(
        "INSERT INTO items (id, use_skill_id) VALUES              ({BOP_ITEM}, {OTHER_ITEM_ONLY}), (6_001, {ITEM_ONLY});"
    ));
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7003, BOP_ITEM, 1);
    let packet = super::handlers::CSStartSkillPacket {
        skill_id: ITEM_ONLY,
        caster: SkillCaster::Item {
            obj_id: a.obj_id,
            item_id: 0x7003,
            item_template_id: 0,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(a.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    };
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 500);
    assert_eq!(
        item_started_result(&mut client, ITEM_ONLY),
        Some(super::types::skill_result::NO_PERM)
    );
}

/// An item that leaves the bag during a cast time refuses the effect at consumption.
#[tokio::test]
async fn item_removed_during_the_cast_time_refuses_the_effect() {
    use super::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::data::items::ItemTemplate;
    use crate::systems::economy::inventory::Inv;

    const DELAYED: u32 = 20_005;
    const SCROLL: u32 = 6_003;
    let mut data = mate_cast_data();
    let mut skill = super::tests::skill_template(DELAYED);
    skill.target_type = 0;
    skill.damage_type_id = 0;
    skill.casting_time = 50;
    let mut effect = super::tests::skill_effect(900_001, super::data::EffectKind::HealEffect, 1);
    effect.consume_item_count = 1;
    skill.effects.push(effect);
    data.systems.skills.add_skill(skill);
    let mut item = ItemTemplate::plain(SCROLL, 10);
    item.use_skill_id = DELAYED;
    item.use_skill_as_reagent = true;
    data.items.templates.insert(SCROLL, item);
    data.skills = loaded_skill_sources(&format!(
        "INSERT INTO items (id, use_skill_id) VALUES ({SCROLL}, {DELAYED});"
    ));
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7004, SCROLL, 1);
    let packet = super::handlers::CSStartSkillPacket {
        skill_id: DELAYED,
        caster: SkillCaster::Item {
            obj_id: a.obj_id,
            item_id: 0x7004,
            item_template_id: 0,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(a.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    };
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 500);
    // The scroll leaves the bag before the cast completes.
    Inv::of(&a).bag().lock().unwrap().items.clear();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // The cast-time re-check stops the cast: no heal and no consumption.
    assert_eq!(a.data().unit.hp, 500);
    assert_eq!(Inv::of(&a).items_count(SCROLL, -1), 0);
}

/// A skill named by an item table and by another client source keeps its plain cast.
#[tokio::test]
async fn item_and_doodad_skill_still_casts_as_a_plain_unit_skill() {
    use super::types::caster_type;

    const SHARED: u32 = 20_001;
    let mut data = mate_cast_data();
    data.skills = loaded_skill_sources(&format!(
        "INSERT INTO items (id, use_skill_id) VALUES (6_002, {SHARED});
         INSERT INTO doodad_funcs (func_skill_id) VALUES ({SHARED});"
    ));
    assert!(data.skills.is_client_sourced_skill(SHARED));
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    {
        let mut c = a.data();
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        c.unit.mp = 100;
    }
    let mut packet = cast_packet_as(caster_type::UNIT, a.obj_id, a.obj_id);
    packet.skill_id = SHARED;
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert_eq!(a.data().unit.hp, 650);
}

const SAVE_PORTAL_SKILL: u32 = 11_215;
const RENAME_PORTAL_SKILL: u32 = 16_841;
const MEMORY_INK: u32 = 4_053;
const SAVE_PORTAL_EFFECT: u32 = 900;

/// Skill 11215 with the `SavePortal` special effect and its Memory Ink reagent.
fn save_portal_data() -> GameData {
    use super::data::effects::SpecialEffect;
    use super::data::{EffectKind, EffectTemplate, SkillItemAmount, SkillTemplate};
    let mut d = GameData::empty();
    d.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: SAVE_PORTAL_EFFECT,
            special_effect_type_id: 30,
            value1: 0,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        SAVE_PORTAL_EFFECT,
    );
    for skill_id in [SAVE_PORTAL_SKILL, RENAME_PORTAL_SKILL] {
        d.systems.skills.add_skill(SkillTemplate {
            id: skill_id,
            target_type: 0,
            damage_type_id: 0,
            casting_time: 0,
            effects: vec![super::tests::skill_effect(
                SAVE_PORTAL_EFFECT,
                EffectKind::SpecialEffect,
                SAVE_PORTAL_EFFECT,
            )],
            ..super::tests::skill_template(skill_id)
        });
    }
    d.items.templates.insert(
        MEMORY_INK,
        crate::data::items::ItemTemplate::plain(MEMORY_INK, 10),
    );
    d.systems.skills.skill_reagents.push(SkillItemAmount {
        id: 1,
        skill_id: SAVE_PORTAL_SKILL,
        item_id: MEMORY_INK,
        amount: 1,
    });
    d.skills = loaded_skill_sources(&format!(
        "INSERT INTO effects (id, actual_type, actual_id) \
             VALUES ({SAVE_PORTAL_EFFECT}, 'SpecialEffect', {SAVE_PORTAL_EFFECT});
         INSERT INTO skill_effects (skill_id, effect_id) \
             VALUES ({SAVE_PORTAL_SKILL}, {SAVE_PORTAL_EFFECT}),
                    ({RENAME_PORTAL_SKILL}, {SAVE_PORTAL_EFFECT});
         INSERT INTO special_effects (id, special_effect_type_id) \
             VALUES ({SAVE_PORTAL_EFFECT}, 30);"
    ));
    assert!(d.skills.is_client_sourced_skill(SAVE_PORTAL_SKILL));
    assert!(d.skills.is_client_sourced_skill(RENAME_PORTAL_SKILL));
    d
}

fn save_portal_packet(player: &crate::world::PlayerRef) -> super::handlers::CSStartSkillPacket {
    use super::types::{SkillCastTarget, SkillCaster, SkillObject};
    super::handlers::CSStartSkillPacket {
        skill_id: SAVE_PORTAL_SKILL,
        caster: SkillCaster::Unit(player.obj_id),
        target: SkillCastTarget::Unit(player.obj_id),
        flag: 0,
        skill_object: SkillObject::SavePortalInfo {
            id: 0,
            name: "Home".into(),
        },
    }
}

#[tokio::test]
async fn teleport_book_record_skill_is_accepted_and_consumes_memory_ink() {
    use crate::systems::economy::inventory::Inv;

    let server = Arc::new(test_server_with(save_portal_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Traveller", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    give_test_item(&server, &a, 0x7002, MEMORY_INK, 2);
    super::handlers::start_skill(&server, &mut client.conn, save_portal_packet(&a));
    let portals = a.data().portals.private_portals.clone();
    assert_eq!(portals.len(), 1);
    assert_eq!(portals[0].name, "Home");
    assert!(portals[0].id > 0);
    assert_eq!(Inv::of(&a).items_count(MEMORY_INK, -1), 1);
}

#[tokio::test]
async fn teleport_book_record_without_memory_ink_is_refused_by_the_reagent_check() {
    let server = Arc::new(test_server_with(save_portal_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Traveller", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    let mut packet = save_portal_packet(&a);
    packet.skill_object = super::types::SkillObject::None;
    super::handlers::start_skill(&server, &mut client.conn, packet);
    assert!(a.data().portals.private_portals.is_empty());
    assert_eq!(
        started_result(&mut client, SAVE_PORTAL_SKILL),
        Some(super::types::skill_result::NEED_REAGENT)
    );
}

/// 16841 has no reagent: it may rename a book entry but must never create one.
#[tokio::test]
async fn rename_portal_skill_cannot_record_a_location_and_renames_an_existing_one() {
    let server = Arc::new(test_server_with(save_portal_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Traveller", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    let rename = |id: i32, name: &str| {
        let mut packet = save_portal_packet(&a);
        packet.skill_id = RENAME_PORTAL_SKILL;
        packet.skill_object = super::types::SkillObject::SavePortalInfo {
            id,
            name: name.into(),
        };
        packet
    };
    // No ink and no reagent on 16841: nothing may be recorded.
    super::handlers::start_skill(&server, &mut client.conn, rename(0, "Free"));
    assert!(a.data().portals.private_portals.is_empty());
    // A real record, then rename it through 16841.
    give_test_item(&server, &a, 0x7005, MEMORY_INK, 1);
    super::handlers::start_skill(&server, &mut client.conn, save_portal_packet(&a));
    let portals = a.data().portals.private_portals.clone();
    assert_eq!(portals.len(), 1);
    super::handlers::start_skill(
        &server,
        &mut client.conn,
        rename(portals[0].id as i32, "Barn"),
    );
    let portals = a.data().portals.private_portals.clone();
    assert_eq!(portals.len(), 1);
    assert_eq!(portals[0].name, "Barn");
}

/// Real 1.2 vehicle bars: mate skill 21172 is `mount_skills` 209 (npc_mount_skills) and slave
/// skill 13350 is `mount_skills` 34 (`slave_mount_skills`). Only bar ids may be cast.
#[tokio::test]
async fn vehicle_bar_accepts_real_mount_and_slave_skills_and_refuses_other_ids() {
    use super::types::{caster_type, SkillObject};
    use crate::models::mate::Mate;
    use crate::systems::vehicles::data::MountSkills;

    const MOUNT_ID: u32 = 209;
    const MOUNT_SKILL: u32 = 21_172;
    const SLAVE_ID: u32 = 34;
    const SLAVE_SKILL: u32 = 13_350;
    const NPC_SKILL: u32 = 300_001;
    let mut data = mate_cast_data();
    for id in [MOUNT_SKILL, SLAVE_SKILL, NPC_SKILL] {
        let mut skill = super::tests::skill_template(id);
        skill.target_type = 0;
        skill.damage_type_id = 0;
        skill.mana_cost = 25;
        skill.effects.push(super::tests::skill_effect(
            900_001,
            super::data::EffectKind::HealEffect,
            1,
        ));
        data.systems.skills.add_skill(skill);
    }
    data.systems.vehicles.mount_skills.push(MountSkills {
        id: MOUNT_ID,
        name: "Mount".into(),
        skill_id: MOUNT_SKILL,
    });
    data.systems.vehicles.mount_skills.push(MountSkills {
        id: SLAVE_ID,
        name: "Slave".into(),
        skill_id: SLAVE_SKILL,
    });
    // Slave template 1 (the fixture's) mounts 34 -> 13350.
    data.systems
        .vehicles
        .slave_mount_skills
        .push((2, 1, SLAVE_ID));
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Rider", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    a.data().unit.mp = 100;
    let world = server.world.world(0).unwrap();
    let mut mate_data = Mate::new(1, 1);
    mate_data.skills = vec![MOUNT_ID];
    mate_data.owner_obj_id = a.obj_id;
    mate_data.unit.hp = 500;
    mate_data.unit.max_hp = 1000;
    mate_data.unit.mp = 100;
    let mate = Entity::new(0x201, mate_data);
    world.spawn(&mate, &server.view_ctx());
    let mut slave_data = Slave::new(0x300, 1);
    slave_data.summoner_obj_id = a.obj_id;
    slave_data.summoner_id = a.character_id;
    slave_data.unit.hp = 500;
    slave_data.unit.max_hp = 1000;
    slave_data.unit.mp = 100;
    let slave = Entity::new(0x300, slave_data);
    world.spawn(&slave, &server.view_ctx());
    let mut cast = |obj_id: u32, skill: u32| {
        let mut packet = cast_packet_as(caster_type::MOUNT, obj_id, obj_id);
        packet.skill_id = skill;
        packet.skill_object = SkillObject::None;
        super::handlers::start_skill(&server, &mut client.conn, packet);
    };
    cast(mate.obj_id, MOUNT_SKILL);
    assert_eq!(mate.data().unit.hp, 650);
    cast(slave.obj_id, SLAVE_SKILL);
    assert_eq!(slave.data().unit.mp, 75);
    // The player is not its own mount, and a vehicle bar does not authorize other skill ids.
    cast(a.obj_id, NPC_SKILL);
    assert_eq!(a.data().unit.hp, 500);
    cast(mate.obj_id, NPC_SKILL);
    assert_eq!(mate.data().unit.hp, 650);
}

/// A bind-on-pickup item cannot authorize an NPC or an unlearned ability skill.
#[tokio::test]
async fn bind_on_pickup_item_does_not_authorize_npc_or_ability_skills() {
    use super::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::data::items::{bind_type, ItemTemplate};

    const BOP_ITEM: u32 = 6_004;
    const NPC_SKILL: u32 = 300_001;
    const ABILITY_SKILL: u32 = 300_002;
    let mut data = mate_cast_data();
    for (id, ability_id) in [(NPC_SKILL, 0), (ABILITY_SKILL, 1)] {
        let mut skill = super::tests::skill_template(id);
        skill.target_type = 0;
        skill.damage_type_id = 0;
        skill.ability_id = ability_id;
        skill.need_learn = ability_id != 0;
        skill.effects.push(super::tests::skill_effect(
            900_001,
            super::data::EffectKind::HealEffect,
            1,
        ));
        data.systems.skills.add_skill(skill);
    }
    let mut item = ItemTemplate::plain(BOP_ITEM, 10);
    item.bind_type = bind_type::BIND_ON_PICKUP;
    item.use_skill_id = 0;
    data.items.templates.insert(BOP_ITEM, item);
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7006, BOP_ITEM, 1);
    for skill in [NPC_SKILL, ABILITY_SKILL] {
        let packet = super::handlers::CSStartSkillPacket {
            skill_id: skill,
            caster: SkillCaster::Item {
                obj_id: a.obj_id,
                item_id: 0x7006,
                item_template_id: 0,
                type1: 0,
                type2: 0,
            },
            target: SkillCastTarget::Unit(a.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        };
        super::handlers::start_skill(&server, &mut client.conn, packet);
        assert_eq!(a.data().unit.hp, 500, "skill {skill} must not run");
        assert_eq!(a.data().unit.skill_last_used_ms, 0, "skill {skill}");
    }
}

const FIREPLACE_SKILL: u32 = 16_387;
const FIREPLACE_ITEM: u32 = 6_005;
const FIREPLACE_DOODAD: u32 = 9_000;
const FIREPLACE_GROUP: u32 = 5_000;
const FIREPLACE_FUNC: u32 = 900;

/// 16387 "Save Location" with the fireplace's `DoodadFuncUse`, its `InteractionEffect` and the
/// effect-level Memory Ink (4053 x1). The doodad is 2 m in front of the player.
fn fireplace_data() -> GameData {
    use super::data::effects::SpecialEffect;
    use super::data::{EffectKind, EffectTemplate, SkillTemplate};
    use crate::systems::npcs::doodad::templates::{DoodadFunc, DoodadFuncUse, FuncTemplate};

    let mut d = mate_cast_data();
    d.skills.add_client_sourced_skill(FIREPLACE_SKILL);
    d.items.templates.insert(
        FIREPLACE_ITEM,
        crate::data::items::ItemTemplate {
            bind_type: crate::data::items::bind_type::BIND_ON_PICKUP,
            ..crate::data::items::ItemTemplate::plain(FIREPLACE_ITEM, 1)
        },
    );
    d.items.templates.insert(
        MEMORY_INK,
        crate::data::items::ItemTemplate::plain(MEMORY_INK, 10),
    );
    {
        let s = &mut d.systems.skills;
        s.add_effect(
            EffectKind::SpecialEffect,
            EffectTemplate::Special(SpecialEffect {
                id: 700,
                special_effect_type_id: 30,
                value1: 0,
                value2: 0,
                value3: 0,
                value4: 0,
            }),
            700,
        );
        s.add_effect(
            EffectKind::InteractionEffect,
            EffectTemplate::Interaction {
                id: 701,
                world_interaction: 0,
                doodad_id: 0,
            },
            701,
        );
        let mut skill = SkillTemplate {
            id: FIREPLACE_SKILL,
            target_type: 8, // Doodad
            target_selection: 2,
            max_range: 4,
            damage_type_id: 0,
            casting_time: 0,
            ..super::tests::skill_template(FIREPLACE_SKILL)
        };
        let mut consume = super::tests::skill_effect(700, EffectKind::SpecialEffect, 700);
        consume.consume_item_id = MEMORY_INK;
        consume.consume_item_count = 1;
        skill.effects.push(consume);
        skill.effects.push(super::tests::skill_effect(
            701,
            EffectKind::InteractionEffect,
            701,
        ));
        s.add_skill(skill);
    }
    let doodad_data = &mut d.systems.npcs.doodad;
    doodad_data.funcs_by_groups.insert(
        FIREPLACE_GROUP,
        vec![DoodadFunc {
            group_id: FIREPLACE_GROUP,
            func_id: FIREPLACE_FUNC,
            func_key: FIREPLACE_FUNC,
            func_type: "DoodadFuncUse".into(),
            next_phase: 1,
            sound_id: 0,
            skill_id: FIREPLACE_SKILL,
            perm_id: 0,
            count: 0,
        }],
    );
    doodad_data
        .func_templates
        .entry("DoodadFuncUse")
        .or_default()
        .insert(
            FIREPLACE_FUNC,
            FuncTemplate::Use(DoodadFuncUse {
                id: FIREPLACE_FUNC,
                skill_id: FIREPLACE_SKILL,
            }),
        );
    d
}

/// Spawns the fireplace doodad and returns it.
fn spawn_fireplace(server: &Arc<crate::GameServer>, x: f32) -> crate::world::DoodadRef {
    let mut doodad = crate::models::doodad::Doodad::new(FIREPLACE_DOODAD);
    doodad.func_group_id = FIREPLACE_GROUP;
    doodad.transform.set_xyz(x, 100.0, 100.0);
    let doodad = Entity::new(0x900, doodad);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&doodad, &server.view_ctx());
    doodad
}

fn fireplace_packet(
    player: &crate::world::PlayerRef,
    item_id: u64,
    target: u32,
) -> super::handlers::CSStartSkillPacket {
    use super::types::{SkillCastTarget, SkillCaster, SkillObject};
    super::handlers::CSStartSkillPacket {
        skill_id: FIREPLACE_SKILL,
        caster: if item_id == 0 {
            SkillCaster::Unit(player.obj_id)
        } else {
            SkillCaster::Item {
                obj_id: player.obj_id,
                item_id,
                item_template_id: 0,
                type1: 0,
                type2: 0,
            }
        },
        target: SkillCastTarget::Doodad(target),
        flag: 0,
        skill_object: SkillObject::SavePortalInfo {
            id: 0,
            name: "Hearth".into(),
        },
    }
}

#[tokio::test]
async fn fireplace_save_portal_requires_its_doodad_and_memory_ink() {
    use crate::systems::economy::inventory::Inv;

    for (ink, item_cast) in [(true, true), (false, true), (true, false)] {
        let server = Arc::new(test_server_with(fireplace_data()));
        server.set_self();
        super::bind(&server);
        crate::systems::economy::bridges::install(&server);
        crate::systems::npcs::bridges::install(&server);
        let mut client = TestClient::new(&server, 1, 1);
        let a = spawn_player(
            &server,
            &client,
            test_character(1, "Hearth", 100.0, 100.0),
            0x100,
        );
        client.conn.active_char = Some(a.clone());
        let doodad = spawn_fireplace(&server, 102.0);
        let item = if item_cast {
            give_test_item(&server, &a, 0x7007, FIREPLACE_ITEM, 1);
            give_test_item(&server, &a, 0x7008, MEMORY_INK, 2);
            0x7007
        } else {
            give_test_item(&server, &a, 0x7008, MEMORY_INK, 2);
            0
        };
        if !ink {
            // Remove the ink again: the fireplace must not save for free.
            Inv::of(&a).bag().lock().unwrap().items.clear();
        }
        super::handlers::start_skill(
            &server,
            &mut client.conn,
            fireplace_packet(&a, item, doodad.obj_id),
        );
        let portals = a.data().portals.private_portals.clone();
        if ink {
            assert_eq!(portals.len(), 1, "with ink, item_cast={item_cast}");
            assert_eq!(portals[0].name, "Hearth");
            assert_eq!(Inv::of(&a).items_count(MEMORY_INK, -1), 1);
        } else {
            assert!(portals.is_empty(), "no ink must not save");
            assert_eq!(Inv::of(&a).items_count(MEMORY_INK, -1), 0);
        }
        let _ = doodad;
    }
}

#[tokio::test]
async fn fireplace_save_portal_out_of_range_is_refused() {
    use crate::systems::economy::inventory::Inv;

    let server = Arc::new(test_server_with(fireplace_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Hearth", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    let doodad = spawn_fireplace(&server, 120.0);
    give_test_item(&server, &a, 0x7008, MEMORY_INK, 1);
    super::handlers::start_skill(
        &server,
        &mut client.conn,
        fireplace_packet(&a, 0, doodad.obj_id),
    );
    assert!(a.data().portals.private_portals.is_empty());
    assert_eq!(Inv::of(&a).items_count(MEMORY_INK, -1), 1, "no ink spent");
}

/// Plot-only skills bypass `apply_effects`; an Item caster must still own its source item.
#[tokio::test]
async fn plot_only_item_skill_requires_the_owned_item() {
    use super::state::SkillRun;
    use super::types::{SkillCastTarget, SkillCaster};
    let server = Arc::new(test_server_with(mate_cast_data()));
    server.set_self();
    super::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Plotter", 100.0, 100.0),
        0x100,
    );
    let skill = SkillRun::new(
        Arc::new(super::data::SkillTemplate {
            id: 20_006,
            target_type: 0,
            plot_id: Some(1),
            plot_only: true,
            ..super::tests::skill_template(20_006)
        }),
        1,
    );
    let mut target = SkillCastTarget::Unit(a.obj_id);
    let (result, _) = super::skill::use_skill(
        &server,
        &skill,
        &WorldObject::from(&a),
        SkillCaster::Item {
            obj_id: a.obj_id,
            item_id: 0x9999,
            item_template_id: 6_001,
            type1: 0,
            type2: 0,
        },
        &mut target,
        None,
        false,
    );
    assert_eq!(result, super::types::skill_result::INVALID_SOURCE);
    assert!(skill.cancelled());
}

const CAST_ITEM_SKILL: u32 = 20_010;
const CAST_ITEM: u32 = 6_010;

/// A non-reagent item skill with a 50 ms cast whose effect consumes the source item.
fn cast_item_data() -> GameData {
    use crate::data::items::ItemTemplate;
    let mut data = mate_cast_data();
    let mut skill = super::tests::skill_template(CAST_ITEM_SKILL);
    skill.target_type = 0;
    skill.damage_type_id = 0;
    skill.casting_time = 50;
    let mut effect = super::tests::skill_effect(900_001, super::data::EffectKind::HealEffect, 1);
    effect.consume_source_item = true;
    effect.consume_item_count = 1;
    skill.effects.push(effect);
    data.systems.skills.add_skill(skill);
    let mut item = ItemTemplate::plain(CAST_ITEM, 10);
    item.use_skill_id = CAST_ITEM_SKILL;
    item.use_skill_as_reagent = false;
    data.items.templates.insert(CAST_ITEM, item);
    data
}

fn cast_item_packet(
    player: &crate::world::PlayerRef,
    item_id: u64,
) -> super::handlers::CSStartSkillPacket {
    use super::types::{SkillCastTarget, SkillCaster, SkillObject};
    super::handlers::CSStartSkillPacket {
        skill_id: CAST_ITEM_SKILL,
        caster: SkillCaster::Item {
            obj_id: player.obj_id,
            item_id,
            item_template_id: CAST_ITEM,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(player.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

#[tokio::test]
async fn loot_box_destroyed_during_the_cast_grants_no_effect() {
    use crate::net::registry::ClientPacket;
    use crate::systems::economy::inventory::Inv;
    use crate::systems::economy::item_handlers::CSDestroyItemPacket;

    let server = Arc::new(test_server_with(cast_item_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7010, CAST_ITEM, 1);
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7010));
    CSDestroyItemPacket {
        item_id: 0x7010,
        slot_type: crate::models::container::slot_type::INVENTORY,
        slot: 0,
        count: 1,
    }
    .handle(&server, &mut client.conn)
    .await
    .expect("destroy");
    assert_eq!(Inv::of(&a).items_count(CAST_ITEM, -1), 0);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(a.data().unit.hp, 500, "the effect must not be granted");
}

#[tokio::test]
async fn loot_box_traded_during_the_cast_grants_no_effect_and_leaves_the_partner_item() {
    use crate::models::container::slot_type;
    use crate::systems::economy::inventory::Inv;

    let server = Arc::new(test_server_with(cast_item_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let other = TestClient::new(&server, 2, 2);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    let b = spawn_player(
        &server,
        &other,
        test_character(2, "Trader", 101.0, 100.0),
        0x101,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7011, CAST_ITEM, 1);
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7011));
    // The completed trade moves the same item id to the partner's bag.
    let item = {
        let bag = Inv::of(&a).bag();
        let mut bag = bag.lock().unwrap();
        let index = bag.items.iter().position(|i| i.id == 0x7011).expect("item");
        bag.items.remove(index)
    };
    let mut item = item;
    item.owner_id = u64::from(b.character_id);
    let bag = Inv::of(&b).bag();
    bag.lock().unwrap().items.push(item);
    server.items.add_container(bag);
    assert_eq!(Inv::of(&b).items_count(CAST_ITEM, -1), 1);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(a.data().unit.hp, 500, "the effect must not be granted");
    assert_eq!(
        Inv::of(&b).items_count(CAST_ITEM, -1),
        1,
        "the partner's item is untouched"
    );
    let _ = slot_type::INVENTORY;
}

const CAST_PRODUCT: u32 = 6_012;

/// The cast item's skill with a product: the old pipeline granted it before the source item
/// was checked again, so a moveless cast was not the only way to get it.
fn cast_item_with_product_data() -> GameData {
    use super::data::SkillItemAmount;
    use crate::data::items::ItemTemplate;
    let mut data = cast_item_data();
    data.items
        .templates
        .insert(CAST_PRODUCT, ItemTemplate::plain(CAST_PRODUCT, 10));
    data.systems.skills.skill_products.push(SkillItemAmount {
        id: 900_010,
        skill_id: CAST_ITEM_SKILL,
        item_id: CAST_PRODUCT,
        amount: 1,
    });
    data
}

fn cast_product_fixture() -> (Arc<crate::GameServer>, TestClient, crate::world::PlayerRef) {
    let server = Arc::new(test_server_with(cast_item_with_product_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    (server, client, a)
}

/// Move `item_id` out of the bag into another of the caster's containers, as a mail send or an
/// auction listing does mid-cast.
fn move_item_to_container(
    server: &crate::GameServer,
    player: &crate::world::PlayerRef,
    item_id: u64,
    st: u8,
    slot: i32,
) {
    use crate::systems::economy::inventory::Inv;
    let inv = Inv::of(player);
    let bag = inv.bag();
    let mut item = {
        let mut bag = bag.lock().unwrap();
        let index = bag
            .items
            .iter()
            .position(|i| i.id == item_id)
            .expect("item");
        bag.items.remove(index)
    };
    item.slot_type = st;
    item.slot = slot;
    let container = inv.get(st).expect("container");
    container.lock().unwrap().items.push(item);
    server.items.add_container(container);
}

#[tokio::test]
async fn cast_item_with_products_destroyed_mid_cast_grants_nothing() {
    use crate::net::registry::ClientPacket;
    use crate::systems::economy::inventory::Inv;
    use crate::systems::economy::item_handlers::CSDestroyItemPacket;

    let (server, mut client, a) = cast_product_fixture();
    give_test_item(&server, &a, 0x7020, CAST_ITEM, 1);
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7020));
    CSDestroyItemPacket {
        item_id: 0x7020,
        slot_type: crate::models::container::slot_type::INVENTORY,
        slot: 0,
        count: 1,
    }
    .handle(&server, &mut client.conn)
    .await
    .expect("destroy");
    assert_eq!(Inv::of(&a).items_count(CAST_ITEM, -1), 0);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(a.data().unit.hp, 500, "the effect must not be granted");
    assert_eq!(
        Inv::of(&a).items_count(CAST_PRODUCT, -1),
        0,
        "no product may be granted"
    );
}

#[tokio::test]
async fn cast_item_with_products_mailed_or_auctioned_mid_cast_grants_nothing() {
    use crate::models::container::slot_type;
    use crate::systems::economy::inventory::Inv;

    for (case, st) in [("mail", slot_type::MAIL), ("auction", slot_type::AUCTION)] {
        let (server, mut client, a) = cast_product_fixture();
        give_test_item(&server, &a, 0x7021, CAST_ITEM, 1);
        super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7021));
        move_item_to_container(&server, &a, 0x7021, st, 0);
        assert_eq!(Inv::of(&a).items_count_in(st, CAST_ITEM, -1), 1);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(a.data().unit.hp, 500, "{case}: no effect");
        assert_eq!(
            Inv::of(&a).items_count(CAST_PRODUCT, -1),
            0,
            "{case}: no product"
        );
        assert_eq!(
            Inv::of(&a).items_count_in(st, CAST_ITEM, -1),
            1,
            "{case}: the item is not taken from {case}"
        );
    }
}

#[tokio::test]
async fn cast_item_with_products_traded_mid_cast_grants_nothing() {
    use crate::systems::economy::inventory::Inv;

    let server = Arc::new(test_server_with(cast_item_with_product_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let mut other = TestClient::new(&server, 2, 2);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    let b = spawn_player(
        &server,
        &other,
        test_character(2, "Trader", 101.0, 100.0),
        0x101,
    );
    client.conn.active_char = Some(a.clone());
    other.conn.active_char = Some(b.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7022, CAST_ITEM, 1);
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7022));
    // The completed trade moves the same item id to the partner's bag.
    let mut item = {
        let bag = Inv::of(&a).bag();
        let mut bag = bag.lock().unwrap();
        let index = bag.items.iter().position(|i| i.id == 0x7022).expect("item");
        bag.items.remove(index)
    };
    item.owner_id = u64::from(b.character_id);
    let bag = Inv::of(&b).bag();
    bag.lock().unwrap().items.push(item);
    server.items.add_container(bag);
    assert_eq!(Inv::of(&b).items_count(CAST_ITEM, -1), 1);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(a.data().unit.hp, 500, "the effect must not be granted");
    assert_eq!(
        Inv::of(&a).items_count(CAST_PRODUCT, -1),
        0,
        "no product may be granted"
    );
    assert_eq!(
        Inv::of(&b).items_count(CAST_ITEM, -1),
        1,
        "the partner's item is untouched"
    );
}

#[tokio::test]
async fn kept_cast_item_grants_its_product_and_effect_and_is_consumed_once() {
    use crate::systems::economy::inventory::Inv;

    let (server, mut client, a) = cast_product_fixture();
    give_test_item(&server, &a, 0x7023, CAST_ITEM, 2);
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7023));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(a.data().unit.hp, 650, "the effect is granted once");
    assert_eq!(
        Inv::of(&a).items_count(CAST_PRODUCT, -1),
        1,
        "the product is granted once"
    );
    assert_eq!(
        Inv::of(&a).items_count(CAST_ITEM, -1),
        1,
        "exactly one of the two source units is consumed"
    );
    let bag = crate::systems::economy::inventory::Inv::of(&a).bag();
    let (source_slot, product_slot) = {
        let bag = bag.lock().unwrap();
        (
            bag.get_item_by_id(0x7023)
                .expect("the stack keeps its id")
                .slot,
            bag.items
                .iter()
                .find(|i| i.template_id == CAST_PRODUCT)
                .expect("product")
                .slot,
        )
    };
    assert_eq!(source_slot, 0, "the partial debit keeps the stack in place");
    assert_ne!(
        product_slot, source_slot,
        "the product must not be placed into the occupied source slot"
    );
}

#[tokio::test]
async fn channeled_cast_cancelled_after_a_tick_consumes_the_item_once() {
    use crate::systems::economy::inventory::Inv;

    // The same source-consuming effect on a channel that ticks every 50 ms.
    let mut data = cast_item_data();
    let mut channel_skill = (**data
        .systems
        .skills
        .skill(CAST_ITEM_SKILL)
        .expect("cast item skill"))
    .clone();
    channel_skill.casting_time = 0;
    channel_skill.channeling_time = 400;
    channel_skill.channeling_tick = 50;
    data.systems.skills.add_skill(channel_skill);
    let server = Arc::new(test_server_with(data));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(a.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    give_test_item(&server, &a, 0x7030, CAST_ITEM, 2);
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7030));
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let skill = a
        .data()
        .unit
        .skill_state
        .skill_task
        .as_ref()
        .expect("channel task")
        .skill
        .clone();
    super::skill::stop(&server, &skill, &crate::world::WorldObject::from(&a), 0);
    assert_eq!(
        a.data().unit.hp,
        650,
        "one tick was applied before the cancel"
    );
    assert_eq!(
        Inv::of(&a).items_count(CAST_ITEM, -1),
        1,
        "one of the two source units is consumed"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        a.data().unit.hp,
        650,
        "the cancelled channel applies no more ticks"
    );
    assert_eq!(
        Inv::of(&a).items_count(CAST_ITEM, -1),
        1,
        "the cancelled channel consumes no more units"
    );
}

/// One `SCItemTaskSuccessPacket` body, written by the server's own encoder.
fn item_task_body(action: u8, tasks: Vec<crate::models::item_task::ItemTask>) -> Vec<u8> {
    use crate::systems::economy::item_packets::SCItemTaskSuccessPacket;
    use aaemu_commons::{Marshal, PacketStream};
    let mut s = PacketStream::new();
    SCItemTaskSuccessPacket {
        action,
        tasks,
        force_remove: Vec::new(),
    }
    .write(&mut s);
    s.into_bytes()
}

fn item_task_bodies(frames: &[(u16, Vec<u8>)]) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCItemTaskSuccessPacket)
        .map(|(_, body)| body.clone())
        .collect()
}

#[tokio::test]
async fn full_cast_item_removal_precedes_the_product_in_the_same_slot() {
    use crate::models::item_task::{task_type, ItemTask};
    use crate::systems::economy::inventory::Inv;

    let (server, mut client, a) = cast_product_fixture();
    give_test_item(&server, &a, 0x7040, CAST_ITEM, 1);
    let source = Inv::of(&a)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id(0x7040)
        .expect("source")
        .clone();
    client.drain();
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7040));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let frames = client.drain();
    let product = Inv::of(&a)
        .bag()
        .lock()
        .unwrap()
        .items
        .iter()
        .find(|i| i.template_id == CAST_PRODUCT)
        .expect("product")
        .clone();
    assert_eq!(
        product.slot, source.slot,
        "the product takes the slot the full consume emptied"
    );
    assert_eq!(
        item_task_bodies(&frames),
        vec![
            item_task_body(
                task_type::SKILL_REAGENTS,
                vec![ItemTask::remove_slot(&source)]
            ),
            item_task_body(
                task_type::SKILL_EFFECT_GAIN_ITEM,
                vec![ItemTask::add(&product)]
            ),
        ],
        "the client removes the source before the product add"
    );
}

#[tokio::test]
async fn partial_cast_item_debits_in_place_before_the_product_add() {
    use crate::models::item_task::{task_type, ItemTask};
    use crate::systems::economy::inventory::Inv;

    let (server, mut client, a) = cast_product_fixture();
    give_test_item(&server, &a, 0x7041, CAST_ITEM, 2);
    let source = Inv::of(&a)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id(0x7041)
        .expect("source")
        .clone();
    client.drain();
    super::handlers::start_skill(&server, &mut client.conn, cast_item_packet(&a, 0x7041));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let frames = client.drain();
    let bag = Inv::of(&a).bag();
    let bag = bag.lock().unwrap();
    let source_after = bag.get_item_by_id(0x7041).expect("remainder").clone();
    let product = bag
        .items
        .iter()
        .find(|i| i.template_id == CAST_PRODUCT)
        .expect("product")
        .clone();
    assert_eq!((source_after.slot, source_after.count), (0, 1));
    assert_ne!(
        product.slot, source_after.slot,
        "the product takes a different slot than the kept stack"
    );
    assert_eq!(
        item_task_bodies(&frames),
        vec![
            item_task_body(
                task_type::SKILL_REAGENTS,
                vec![ItemTask::count_update(&source, -1)]
            ),
            item_task_body(
                task_type::SKILL_EFFECT_GAIN_ITEM,
                vec![ItemTask::add(&product)]
            ),
        ],
        "the count update precedes the product add"
    );
}

const CAST_EQUIP_ITEM: u32 = 6_013;

fn cast_equipped_item_data() -> GameData {
    use crate::data::items::{EquipTemplate, ItemTemplate, TemplateKind};
    let mut data = cast_item_data();
    let mut item = ItemTemplate::plain(CAST_EQUIP_ITEM, 1);
    item.kind = TemplateKind::Armor(EquipTemplate {
        slot_type_id: 3,
        coverage: 10,
        durability_ratio: 1.0,
        durability_multiplier: 100,
        ..EquipTemplate::default()
    });
    item.use_skill_id = CAST_ITEM_SKILL;
    data.items.templates.insert(CAST_EQUIP_ITEM, item);
    data
}

#[tokio::test]
async fn consuming_an_equipped_source_item_runs_the_equip_leave_path() {
    use crate::models::container::equip_slot;
    use crate::models::item_task::{task_type, ItemTask};
    use crate::systems::economy::inventory::Inv;
    use crate::systems::economy::item_packets::SCUnitEquipmentsChangedPacket;
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use aaemu_commons::{Marshal, PacketStream};

    let server = Arc::new(test_server_with(cast_equipped_item_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let mut other = TestClient::new(&server, 2, 2);
    let a = spawn_player(
        &server,
        &client,
        test_character(1, "Caster", 100.0, 100.0),
        0x100,
    );
    let b = spawn_player(
        &server,
        &other,
        test_character(2, "Watcher", 101.0, 100.0),
        0x101,
    );
    client.conn.active_char = Some(a.clone());
    other.conn.active_char = Some(b.clone());
    a.data().unit.hp = 500;
    a.data().unit.max_hp = 1000;
    let item = server
        .items
        .create(
            CAST_EQUIP_ITEM,
            1,
            0,
            server.config.id,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("equipped item");
    let item_id = item.id;
    let equipment = Inv::of(&a).equipment();
    equipment
        .lock()
        .unwrap()
        .add_new_item(item, equip_slot::CHEST, &server.game_data.items)
        .expect("equipped");
    server.items.add_container(equipment.clone());
    let equipped = equipment
        .lock()
        .unwrap()
        .get_item_by_id(item_id)
        .expect("equipped")
        .clone();
    client.drain();
    other.drain();
    super::handlers::start_skill(
        &server,
        &mut client.conn,
        super::handlers::CSStartSkillPacket {
            skill_id: CAST_ITEM_SKILL,
            caster: SkillCaster::Item {
                obj_id: a.obj_id,
                item_id,
                item_template_id: CAST_EQUIP_ITEM,
                type1: 0,
                type2: 0,
            },
            target: SkillCastTarget::Unit(a.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        equipment.lock().unwrap().get_item_by_id(item_id).is_none(),
        "the equipped item is consumed"
    );
    assert_eq!(
        item_task_bodies(&client.drain()),
        vec![item_task_body(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::remove_slot(&equipped)]
        )],
        "the owner sees the equipment-slot removal"
    );
    let mut expected = PacketStream::new();
    SCUnitEquipmentsChangedPacket {
        obj_id: a.obj_id,
        items: vec![(equip_slot::CHEST as u8, None)],
    }
    .write(&mut expected);
    let expected = expected.into_bytes();
    assert!(
        other.drain().iter().any(|(op, body)| *op
            == crate::opcodes::sc::SCUnitEquipmentsChangedPacket
            && *body == expected),
        "nearby players see the equipped slot empty"
    );
}
