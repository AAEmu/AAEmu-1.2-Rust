//! Authored 1.2 relation subsets; coordinates/world instances are synthetic geometry.
//! Native evidence: D:/aa-re/client12/decomp/x2game12.c
//! - 1646640..1646705: banned tag descriptor fields, including banned_periods_id.
//! - 1983345..1983346: SkillResult 58 = ZONE_BANNED.
//! - 1163648..1163649: ErrorMessage 440 = item_cannot_use_here.
//! - 1766270..1766291 + 1748773..1748807: started-skill body, flags and byte result.
//! - 1761305..1761336: two i16 errors, u32 type, bool notify in SCErrorMsg.
//! - 1765129..1765192: caster discriminator, three-byte object ID and item fields.
//!
//! These literal failure bodies are native-layout fixtures, not Rust encode/decode round trips.

use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::PacketStream;
use rusqlite::Connection;

use super::{current_group, ZoneRules};
use crate::data::items::ItemTemplate;
use crate::data::world_data::Zone;
use crate::data::world_templates::WorldTemplate;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillItemAmount, SkillTemplate};
use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::state::{SkillRef, SkillRun};
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::systems::skills::{self, skill};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{PlayerRef, WorldObject};
use crate::GameServer;

const GLIDER: u32 = 12373;
const FISHING: u32 = 21571;
const GUARD: u32 = 24133;
const MOUNT_ITEM: u32 = 34580;
const SHIP_ITEM: u32 = 34314;
const GLIDER_ITEM: u32 = 14677;
const LUMBER: u32 = 8337;
const PLACEMENT_ITEM: u32 = 32095;

fn relations() -> ZoneRules {
    let conn = Connection::open_in_memory().unwrap();
    // Exact authored tuples (id, group, tag, period) from compact 1.2. Library index
    // has banned-tag rows but NO tagged_items/tagged_skills rows in this export.
    conn.execute_batch(
        "CREATE TABLE zone_group_banned_tags (id, zone_group_id, tag_id, banned_periods_id);
         INSERT INTO zone_group_banned_tags VALUES
             (2,45,471,0),(3,45,472,0),(16,45,629,0),(21,45,296,0),
             (13,45,628,0),(185,45,1348,0),(380,45,1581,0),(337,1,1581,0),
             (271,70,472,0),(280,70,296,0),(281,70,629,0),(277,70,1348,0),
             (203,63,1338,24);
         CREATE TABLE tagged_items (tag_id, item_id);
         INSERT INTO tagged_items VALUES (471,34580),(472,34314),(629,14677),(628,32095);",
    )
    .unwrap();
    ZoneRules::load(&conn).unwrap()
}

struct Fixture {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
}

impl Fixture {
    fn new() -> Self {
        let mut data = crate::systems::economy::tests::test_data();
        for (id, key, group) in [(1, 129, 1), (168, 236, 45), (200, 288, 63), (209, 296, 70)] {
            data.zones.zones.insert(
                key,
                Zone {
                    id,
                    zone_key: key,
                    group_id: group,
                    ..Default::default()
                },
            );
            data.zones.zone_id_to_key.insert(id, key);
        }
        let skills = &mut data.systems.skills;
        skills.zone_rules = relations();
        // Use the existing skill tag index; do not create another skill-tag loader.
        skills.skill_tags.insert(GLIDER, vec![296]);
        skills.skill_tags.insert(FISHING, vec![1348]);
        skills.skill_tags.insert(GUARD, vec![1338]);
        skills.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(skills::tests::damage_effect(1, 100))),
            1,
        );
        for id in [GLIDER, FISHING, GUARD, 10602, 15802] {
            let mut effect = skills::tests::skill_effect(1, EffectKind::DamageEffect, 1);
            effect.always_hit = true;
            skills.add_skill(SkillTemplate {
                // Authored IDs/tags with synthetic costs and a damage sentinel to expose
                // accidental side effects in a denied cast.
                id,
                target_type: 0,
                max_range: 30,
                mana_cost: 25,
                consume_labor_power: 10,
                gain_life_point: 5,
                cooldown_time: 1000,
                default_gcd: true,
                damage_type_id: 1,
                level_rule_no_consideration: true,
                effects: vec![effect],
                ..Default::default()
            });
            skills.skill_reagents.push(SkillItemAmount {
                id,
                skill_id: id,
                item_id: LUMBER,
                amount: 1,
            });
            data.skills.add_client_sourced_skill(id);
        }
        for (item, skill) in [
            (MOUNT_ITEM, 10602),
            (SHIP_ITEM, 15802),
            (GLIDER_ITEM, GLIDER),
        ] {
            let mut template = ItemTemplate::plain(item, 10);
            template.use_skill_id = skill;
            template.use_skill_as_reagent = true;
            data.items.templates.insert(item, template);
        }
        data.items
            .templates
            .insert(LUMBER, ItemTemplate::plain(LUMBER, 10));
        data.items
            .templates
            .insert(PLACEMENT_ITEM, ItemTemplate::plain(PLACEMENT_ITEM, 10));
        let server = Arc::new(test_server_with(data));
        server.set_self();
        skills::bind(&server);
        crate::systems::economy::bridges::install(&server);
        server.world.remove_world(0);
        server.world.create_world_instance(
            Arc::new(WorldTemplate::synthetic(0, "main_world", 2, 2, 129)),
            0,
        );
        let dungeon = Arc::new(WorldTemplate::synthetic(
            1,
            "instance_burntcastle_armory",
            2,
            2,
            236,
        ));
        server.world.create_world_instance(dungeon.clone(), 1);
        // An instanced copy has an ephemeral instance ID, NOT its world-template ID.
        server.world.create_world_instance(dungeon, 9001);
        server.world.create_world_instance(
            Arc::new(WorldTemplate::synthetic(2, "instance_library_1", 2, 2, 296)),
            9002,
        );
        let mut client = TestClient::new(&server, 1, 1);
        let mut character = test_character(1, "Caster", 100.0, 100.0);
        character.unit.hp = 1000;
        character.unit.max_hp = 1000;
        character.unit.mp = 100;
        character.unit.max_mp = 100;
        character.labor_power = 100;
        let player = spawn_player(&server, &client, character, 0x100);
        client.conn.active_char = Some(player.clone());
        server.items.add_container(Inv::of(&player).bag());
        let result = Self {
            server,
            client,
            player,
        };
        result.give(LUMBER, 0);
        result
    }

    fn give(&self, template: u32, slot: i32) -> u64 {
        let item = self
            .server
            .items
            .create(
                template,
                2,
                0,
                1,
                &self.server.game_data.items,
                &self.server.ids.item,
                0,
            )
            .unwrap();
        let id = item.id;
        Inv::of(&self.player)
            .bag()
            .lock()
            .unwrap()
            .add_new_item(item, slot, &self.server.game_data.items)
            .unwrap();
        id
    }

    fn enter(&mut self, instance: u32) {
        self.player.data().unit.transform.instance_id = instance;
        let obj = WorldObject::from(&self.player);
        self.server.world.leave_other_instance(&obj, instance);
        self.server
            .world
            .world(instance)
            .unwrap()
            .spawn(&self.player, &self.server.view_ctx());
        // Deliberately stale; only the actual world's sector mapping is authoritative.
        let mut c = self.player.data();
        c.unit.transform.zone_id = if instance == 0 { 236 } else { 129 };
        c.unit.transform.world_id = 0;
        drop(c);
        self.client.drain();
    }

    fn assert_no_costs_or_effects(&self) {
        let c = self.player.data();
        assert_eq!(
            (c.unit.hp, c.unit.mp, c.labor_power, c.vocation_point),
            (1000, 100, 100, 0)
        );
        assert!(c.unit.cooldowns.cooldowns.is_empty());
        assert_eq!(c.unit.global_cooldown_ms, 0);
        assert!(c.unit.skill_state.skill_task.is_none());
        drop(c);
        assert_eq!(Inv::of(&self.player).items_count(LUMBER, -1), 2);
    }

    fn run(&self, id: u32, source: SkillCaster, casting_time: i32) -> (SkillRef, (u8, u32)) {
        let mut template = (**self.server.game_data.systems.skills.skill(id).unwrap()).clone();
        template.casting_time = casting_time;
        let run = SkillRun::new(Arc::new(template), 1);
        let result = skill::use_skill(
            &self.server,
            &run,
            &WorldObject::from(&self.player),
            source,
            &mut SkillCastTarget::Unit(self.player.obj_id),
            Some(SkillObject::None),
            false,
        );
        (run, result)
    }
}

#[test]
fn zone_rules_preserve_conditional_rows_without_widening_bans() {
    let rules = relations();
    assert_eq!(rules.banned_tags[&63][0].banned_periods_id, 24);
    assert!(!rules.bans_any(63, &[1338]));
    assert!(rules.bans_any(45, &[296]));
    assert!(!rules.bans_any(45, &[1338]));
    assert!(rules.bans_any(1, &[1581]));
    assert!(!rules.bans_any(70, &[1581]));
    assert!(!rules.item_tags.values().any(|tags| tags.contains(&1581)));
}

#[test]
fn zone_rules_missing_period_cannot_silently_become_an_unconditional_ban() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE zone_group_banned_tags (id, zone_group_id, tag_id, banned_periods_id);
         INSERT INTO zone_group_banned_tags VALUES (203,63,1338,NULL);
         CREATE TABLE tagged_items (tag_id, item_id);",
    )
    .unwrap();
    assert!(ZoneRules::load(&conn).is_err());
}

#[tokio::test]
async fn zone_rules_attached_caster_uses_composed_position_in_actual_world() {
    use crate::models::slave::Slave;
    use crate::world::Entity;
    let f = Fixture::new();
    let mut template = WorldTemplate::synthetic(0, "main_world", 2, 2, 129);
    // Synthetic sector boundary using real zone keys: local (100,100) is open, but
    // the parent's (1000,100) offset places the player at (1100,200), sector (17,3).
    assert!(template.set_zone_key_at(17, 3, 236));
    let world = f
        .server
        .world
        .create_world_instance(Arc::new(template), 9003);
    f.player.data().unit.transform.instance_id = 9003;
    let obj = WorldObject::from(&f.player);
    f.server.world.leave_other_instance(&obj, 9003);
    world.spawn(&f.player, &f.server.view_ctx());
    let mut parent = Slave::new(0x200, 1);
    parent.unit.transform.set_xyz(1000.0, 100.0, 0.0);
    let parent = Entity::new(0x200, parent);
    world.spawn(&parent, &f.server.view_ctx());
    world.attach_child(&WorldObject::from(&parent), &obj);
    assert_eq!(f.player.data().unit.transform.zone_id, 129);
    assert_eq!(current_group(&f.server, &obj), Some(45));
    assert_eq!(
        f.run(GLIDER, SkillCaster::Unit(f.player.obj_id), 0).1,
        (58, 0)
    );
    f.assert_no_costs_or_effects();
}

#[tokio::test]
async fn zone_rules_direct_skill_packet_is_denied_in_actual_instance_for_players_and_gms() {
    for instance in [1, 9001, 9002] {
        for access in [0, 100] {
            let mut f = Fixture::new();
            f.enter(instance);
            f.player.data().access_level = access;
            assert_eq!(
                current_group(&f.server, &WorldObject::from(&f.player)),
                Some(if instance == 9002 { 70 } else { 45 })
            );
            // Native CSStartSkill (0x052): u32 skill, unit caster + BC id, unit target + BC id,
            // flag zero. IDs are glider 12373 and this fixture's player object 0x100.
            let mut body =
                PacketStream::from_bytes(vec![0x55, 0x30, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0]);
            let packet = CSStartSkillPacket::read(&mut body).unwrap();
            start_skill(&f.server, &mut f.client.conn, packet);
            let frames = f.client.drain();
            assert_eq!(
                frames,
                vec![(
                    sc::SCSkillStartedPacket,
                    vec![0x55, 0x30, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 58,]
                )]
            );
            f.assert_no_costs_or_effects();
            assert_eq!(f.player.data().unit.skill_last_used_ms, 0);
        }
    }
}

#[tokio::test]
async fn zone_rules_item_request_uses_owned_template_before_costs_or_summons() {
    for (item, skill) in [
        (MOUNT_ITEM, 10602),
        (SHIP_ITEM, 15802),
        (GLIDER_ITEM, GLIDER),
    ] {
        let mut f = Fixture::new();
        f.enter(9001);
        let id = f.give(item, 1);
        start_skill(
            &f.server,
            &mut f.client.conn,
            CSStartSkillPacket {
                skill_id: skill,
                caster: SkillCaster::Item {
                    obj_id: f.player.obj_id,
                    item_id: id,
                    item_template_id: LUMBER, // forged untagged template hint
                    type1: 0,
                    type2: 0,
                },
                target: SkillCastTarget::Unit(f.player.obj_id),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
        let frames = f.client.drain();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0],
            (sc::SCErrorMsgPacket, vec![0xb8, 1, 0xb8, 1, 0, 0, 0, 0, 1])
        );
        assert_eq!(frames[1].0, sc::SCSkillStartedPacket);
        assert_eq!(frames[1].1.last(), Some(&58));
        assert_eq!(Inv::of(&f.player).items_count(item, -1), 2);
        f.assert_no_costs_or_effects();
        assert_eq!(f.player.data().unit.skill_last_used_ms, 0);
        let world = f.server.world.world(9001).unwrap();
        assert!(world.all_of_kind(crate::world::ObjectKind::Mate).is_empty());
        assert!(world.all_slaves().is_empty());
        assert!(world.all_doodads().is_empty());
    }
}

#[tokio::test]
async fn zone_rules_direct_doodad_item_request_is_denied_before_placement() {
    use crate::systems::npcs::doodad::packets::CSCreateDoodadPacket;
    let mut f = Fixture::new();
    let item = f.give(PLACEMENT_ITEM, 1);
    // This item is otherwise visible to the placement bridge in the normal world.
    assert!(f
        .server
        .systems
        .npcs
        .hooks
        .economy
        .get()
        .unwrap()
        .doodad_source_item(&f.server, &f.player, item, true)
        .is_some());
    f.enter(9001);
    CSCreateDoodadPacket {
        id: 7876,
        x: 100.0,
        y: 100.0,
        z: 100.0,
        z_rot: 0.0,
        scale: 1.0,
        item_id: item,
    }
    .handle(&f.server, &mut f.client.conn)
    .await
    .unwrap();
    assert_eq!(
        f.client.drain(),
        vec![(sc::SCErrorMsgPacket, vec![0xb8, 1, 0xb8, 1, 0, 0, 0, 0, 1])]
    );
    f.assert_no_costs_or_effects();
    assert_eq!(Inv::of(&f.player).items_count(PLACEMENT_ITEM, -1), 2);
    assert!(f.server.world.world(9001).unwrap().all_doodads().is_empty());
}

#[tokio::test]
async fn zone_rules_fishing_denies_but_unrelated_tag_and_open_world_allow_casts() {
    let mut f = Fixture::new();
    f.enter(1);
    assert_eq!(
        f.run(FISHING, SkillCaster::Unit(f.player.obj_id), 0).1,
        (58, 0)
    );
    f.assert_no_costs_or_effects();
    // Guard skill has a different tag; a conditional ban in another group cannot block it.
    let (_, result) = f.run(GUARD, SkillCaster::Unit(f.player.obj_id), 0);
    assert_eq!(result, (0, 0));
    assert!(f.player.data().unit.mp < 100);
    assert!(f.player.data().unit.hp < 1000);
    assert_eq!(Inv::of(&f.player).items_count(LUMBER, -1), 1);
    let mut f = Fixture::new();
    f.enter(0); // stale zone 236 must not ban gliders in actual Gweonid sector 129.
    assert_eq!(
        current_group(&f.server, &WorldObject::from(&f.player)),
        Some(1)
    );
    assert_eq!(
        f.run(GLIDER, SkillCaster::Unit(f.player.obj_id), 0).1,
        (0, 0)
    );
}

#[tokio::test]
async fn zone_rules_pending_skill_and_item_casts_recheck_zone_before_payment() {
    for item_cast in [false, true] {
        let mut f = Fixture::new();
        f.enter(0);
        let source = if item_cast {
            SkillCaster::Item {
                obj_id: f.player.obj_id,
                item_id: f.give(MOUNT_ITEM, 1),
                item_template_id: MOUNT_ITEM,
                type1: 0,
                type2: 0,
            }
        } else {
            SkillCaster::Unit(f.player.obj_id)
        };
        let id = if item_cast { 10602 } else { GLIDER };
        let (run, result) = f.run(id, source, 50);
        assert_eq!(result, (0, 0));
        assert_ne!(run.tl_id(), 0);
        assert!(f.player.data().unit.skill_state.skill_task.is_some());
        f.enter(9001);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while run.tl_id() != 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(run.cancelled());
        assert_eq!(run.tl_id(), 0);
        f.assert_no_costs_or_effects();
        let frames = f.client.drain();
        assert!(frames
            .iter()
            .any(|(op, body)| *op == sc::SCSkillStartedPacket && body.last() == Some(&58)));
        assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
        if item_cast {
            assert_eq!(Inv::of(&f.player).items_count(MOUNT_ITEM, -1), 2);
        }
    }
}

#[test]
#[ignore = "requires AAEMU_COMPACT_DB pointing to the native 1.2 reference data"]
fn zone_rules_real_compact_db_preserves_all_authored_relations() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let rules = ZoneRules::load(&conn).unwrap();
    let rows: Vec<_> = rules.banned_tags.values().flatten().collect();
    assert_eq!(rows.len(), 333);
    assert_eq!(
        rows.iter().filter(|r| r.banned_periods_id == 0).count(),
        324
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r.banned_periods_id == 24 && r.tag_id == 1338)
            .count(),
        9
    );
    assert!(rules.bans_item(45, MOUNT_ITEM));
    assert!(rules.bans_item(45, SHIP_ITEM));
    assert!(rules.bans_item(45, GLIDER_ITEM));
    assert!(rules.bans_any(45, &[1348]));
    assert!(!rules.bans_any(63, &[1338]));
    assert!(rules.bans_any(1, &[1581]));
    assert!(!rules.bans_any(70, &[1581]));
    assert!(!rules.item_tags.values().any(|tags| tags.contains(&1581)));
}
