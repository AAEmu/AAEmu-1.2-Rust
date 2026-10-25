//! Craft station authorization through CSExecuteCraft and the real skill/CraftEffect flow.
//! Source fixtures: read-only 1.2 compact, native `sub_398A9480`, and PACKETS.md CS 0xf8 /
//! SC 0x10a (D:/aa-re/client12/decomp). No packet bytes are derived from Rust encoders.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use super::{doodad_func_permission as permission, CSExecuteCraft};
use crate::data::{items::ItemTemplate, GameData};
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::models::house::{housing_permission, House};
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::bridges::{EconomyComposite, EconomyForSkills};
use crate::systems::economy::craft_data::{Craft, CraftProduct, CraftSkill};
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::templates::{
    DoodadFunc, DoodadFuncCraftPack, DoodadFuncParentInfo, FuncTemplate,
};
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillTemplate};
use crate::systems::skills::types::SkillTargetType;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{DoodadRef, Entity, HouseRef, PlayerRef};

const TAX: u32 = 76;
const CERTIFICATE: u32 = 31892;
const SKILL: u32 = 16767;
const BENCH: u32 = 0x300;
const RESTRICTED_GROUP: u32 = 90000;
const WRONG_PACK_GROUP: u32 = 90001;
const PORTABLE: u32 = 90002;
const OTHER_RECIPE: u32 = 90003;
const HOUSE_PERMISSION_GROUP: u32 = 90004;
const SAME_ACCOUNT_GROUP: u32 = 90005;

// One live tax CraftPack phase per plaque template (other authored phases are checked by
// the ignored real-data test below). The recipe's req_doodad_id is always canonical 2392.
const PLAQUES: [(u32, u32, u32); 8] = [
    (2392, 6172, 298),
    (973, 1240, 297),
    (5767, 14787, 313),
    (7340, 20054, 301),
    (7341, 20058, 305),
    (7342, 20062, 309),
    (8249, 23031, 325),
    (7308, 19956, 300),
];

fn add_pack(data: &mut GameData, group: u32, func_id: u32, pack: u32, perm: u8) {
    let doodads = &mut data.systems.npcs.doodad;
    doodads.funcs_by_groups.insert(
        group,
        vec![
            // ParentInfo precedes CraftPack on the real plaques. Its permission must not
            // authorize a different function (especially the owner-only pack below).
            DoodadFunc {
                group_id: group,
                func_id: 1,
                func_type: "DoodadFuncParentInfo".into(),
                ..Default::default()
            },
            DoodadFunc {
                group_id: group,
                func_id,
                func_type: "DoodadFuncCraftPack".into(),
                perm_id: u32::from(perm),
                ..Default::default()
            },
        ],
    );
    doodads
        .func_templates
        .entry("DoodadFuncParentInfo")
        .or_default()
        .insert(1, FuncTemplate::ParentInfo(DoodadFuncParentInfo { id: 1 }));
    doodads
        .func_templates
        .entry("DoodadFuncCraftPack")
        .or_default()
        .insert(
            func_id,
            FuncTemplate::CraftPack(DoodadFuncCraftPack {
                id: func_id,
                craft_pack_id: pack,
            }),
        );
}

fn data(cast_time: i32) -> GameData {
    let mut data = crate::systems::economy::tests::test_data();
    data.items
        .templates
        .insert(CERTIFICATE, ItemTemplate::plain(CERTIFICATE, 1000));
    for (id, skill_id, req_doodad_id, is_pack) in [
        (TAX, SKILL, 2392, true),
        (PORTABLE, PORTABLE, 0, false),
        (OTHER_RECIPE, SKILL, 2392, true),
    ] {
        data.systems.economy.craft.crafts.insert(
            id,
            Craft {
                id,
                skill_id,
                req_doodad_id,
                is_pack,
                craft_products: vec![CraftProduct {
                    craft_id: id,
                    item_id: CERTIFICATE,
                    amount: 5,
                    rate: 100,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
    }
    data.systems
        .economy
        .craft
        .craft_pack_crafts
        .insert(3, [TAX].into());
    data.systems
        .economy
        .craft
        .craft_pack_crafts
        .insert(4, [OTHER_RECIPE].into());
    for (id, target_type) in [
        (SKILL, SkillTargetType::Doodad),
        (PORTABLE, SkillTargetType::SelfTarget),
    ] {
        data.systems.economy.craft.skills.insert(
            id,
            CraftSkill {
                id,
                consume_labor_power: 200,
                actability_group_id: 19,
                // Give the test time to change permissions between completed batch steps.
                cooldown_time: 50,
            },
        );
        data.systems.skills.add_skill(SkillTemplate {
            target_type: target_type as i32,
            max_range: 12,
            consume_labor_power: 200,
            actability_group_id: 19,
            casting_time: cast_time,
            target_selection: 2,
            target_area_count: 1,
            effects: vec![crate::systems::skills::tests::skill_effect(
                1,
                EffectKind::CraftEffect,
                1,
            )],
            ..crate::systems::skills::tests::skill_template(id)
        });
    }
    data.systems.skills.add_effect(
        EffectKind::CraftEffect,
        EffectTemplate::Craft {
            id: 1,
            world_interaction: 37,
        },
        1,
    );
    data.systems
        .npcs
        .area_trigger
        .world_interaction_groups
        .insert(
            37,
            crate::systems::npcs::area_trigger::world_interaction_group::CRAFT,
        );
    for (_, group, func) in PLAQUES {
        add_pack(&mut data, group, func, 3, permission::ANY);
    }
    add_pack(
        &mut data,
        RESTRICTED_GROUP,
        90000,
        3,
        permission::OWNER_ONLY,
    );
    add_pack(&mut data, WRONG_PACK_GROUP, 90001, 4, permission::ANY);
    add_pack(
        &mut data,
        SAME_ACCOUNT_GROUP,
        90005,
        3,
        permission::SAME_ACCOUNT,
    );
    add_pack(
        &mut data,
        HOUSE_PERMISSION_GROUP,
        90004,
        3,
        permission::PERMISSION1,
    );
    data
}

struct Fixture {
    server: Arc<GameServer>,
    client: TestClient,
    visitor: PlayerRef,
    owner_client: TestClient,
    owner: PlayerRef,
    house: HouseRef,
    bench: DoodadRef,
}

fn fixture(template: u32, group: u32, cast_time: i32) -> Fixture {
    let server = Arc::new(test_server_with(data(cast_time)));
    server.set_self();
    crate::systems::skills::install_hooks(&server);
    server
        .systems
        .skills
        .ext
        .items
        .install(Arc::new(EconomyForSkills));
    server
        .systems
        .economy
        .craft
        .hooks
        .install(Arc::new(EconomyComposite));
    let mut client = TestClient::new(&server, 1, 1001);
    let mut owner_client = TestClient::new(&server, 2, 1002);
    let visitor = spawn_player(
        &server,
        &client,
        test_character(1, "Visitor", 100.0, 100.0),
        0x100,
    );
    let owner = spawn_player(
        &server,
        &owner_client,
        test_character(2, "Owner", 101.0, 100.0),
        0x101,
    );
    client.conn.active_char = Some(visitor.clone());
    owner_client.conn.active_char = Some(owner.clone());
    // Real characters have one entry per actability group. Construction (19) is
    // required by ChangeLabor; omitting it makes the production path reject the debit.
    for player in [&visitor, &owner] {
        let mut c = player.data();
        c.labor_power = 1000;
        c.actability
            .actabilities
            .push(crate::models::char_data::Actability {
                id: 19,
                point: 0,
                step: 0,
            });
    }
    server.names.add_character(1, "Visitor", 1001);
    server.names.add_character(2, "Owner", 1002);
    let mut house = House::new(1, 267);
    house.current_step = -1;
    house.unit.owner_id = 2;
    house.account_id = 1002;
    let house = Entity::new(0x200, house);
    assert!(server.systems.housing.manager.insert(&house));
    let mut bench = Doodad::new(template);
    bench.owner_type = doodad_owner_type::HOUSING;
    bench.owner_db_id = 1;
    bench.owner_id = 2;
    bench.func_group_id = group;
    bench.transform.set_xyz(101.0, 100.0, 100.0);
    let bench = Entity::new(BENCH, bench);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&bench, &server.view_ctx());
    client.drain();
    owner_client.drain();
    Fixture {
        server,
        client,
        visitor,
        owner_client,
        owner,
        house,
        bench,
    }
}

async fn request(
    server: &GameServer,
    client: &mut TestClient,
    craft: u32,
    target: u32,
    count: i32,
) {
    // Native 1.2 CS 0xf8: recipe:u32, object:BC (3 bytes), count:i32.
    let body = [
        craft.to_le_bytes().as_slice(),
        &target.to_le_bytes()[..3],
        &count.to_le_bytes(),
    ]
    .concat();
    let packet = CSExecuteCraft::read(&mut PacketStream::from_bytes(body)).unwrap();
    packet.handle(server, &mut client.conn).await.unwrap();
}

fn certificates(player: &PlayerRef) -> i32 {
    Inv::of(player).items_count(CERTIFICATE, -1)
}

fn assert_denied(f: &mut Fixture) {
    assert!(!f.server.systems.economy.craft.is_crafting(1));
    assert_eq!(certificates(&f.visitor), 0);
    // Native 1.2 SC 0x10a: i16 148, i16 135, u32 0, bool false (PACKETS.md).
    assert!(f
        .client
        .drain()
        .contains(&(sc::SCErrorMsgPacket, vec![148, 0, 135, 0, 0, 0, 0, 0, 0])));
}

#[tokio::test]
async fn private_farm_visitor_crafts_tax_at_all_eight_authored_plaques_for_200_labor() {
    for (template, group, _) in PLAQUES {
        let mut f = fixture(template, group, 0);
        assert!(!crate::systems::housing::allowed_to_interact(
            &f.server, &f.house, &f.visitor
        ));
        request(&f.server, &mut f.client, TAX, BENCH, 1).await;
        assert_eq!(certificates(&f.visitor), 5, "plaque {template}");
        assert_eq!(f.visitor.data().labor_power, 800, "plaque {template}");
        assert_eq!(f.visitor.data().actability.actabilities[0].point, 200);
    }
}

#[tokio::test]
async fn current_craft_pack_permission_beats_first_parent_info_and_public_house() {
    let mut f = fixture(2392, RESTRICTED_GROUP, 0);
    request(&f.server, &mut f.client, TAX, BENCH, 1).await;
    assert_denied(&mut f);
    f.house.data().permission = housing_permission::PUBLIC;
    request(&f.server, &mut f.client, TAX, BENCH, 1).await;
    assert_denied(&mut f);
    assert_eq!(f.visitor.data().labor_power, 1000);
    request(&f.server, &mut f.owner_client, TAX, BENCH, 1).await;
    assert_eq!(certificates(&f.owner), 5);
    assert_eq!(f.owner.data().labor_power, 800);
}

#[tokio::test]
async fn same_account_crafting_requires_a_resolved_matching_owner_account() {
    for case in 0..3 {
        let mut f = fixture(2392, SAME_ACCOUNT_GROUP, 0);
        f.house.data().permission = housing_permission::PUBLIC;
        match case {
            0 => {} // Public housing does not waive the function's account requirement.
            1 => f.house.data().account_id = f.visitor.data().account_id,
            2 => f.bench.data().owner_type = 0, // No authoritative account to compare.
            _ => unreachable!(),
        }
        request(&f.server, &mut f.client, TAX, BENCH, 1).await;
        if case == 1 {
            assert_eq!(certificates(&f.visitor), 5);
            assert_eq!(f.visitor.data().labor_power, 800);
        } else {
            assert_denied(&mut f);
            assert_eq!(f.visitor.data().labor_power, 1000);
        }
    }
}

#[tokio::test]
async fn craft_packet_rejects_wrong_recipe_pack_and_inactive_phase_even_at_canonical_id() {
    for (recipe, group) in [
        (OTHER_RECIPE, 6172),
        (TAX, WRONG_PACK_GROUP),
        (TAX, 0),
        (PORTABLE, 6172),
    ] {
        let mut f = fixture(2392, group, 0);
        request(&f.server, &mut f.client, recipe, BENCH, 1).await;
        assert_denied(&mut f);
        assert_eq!(f.visitor.data().labor_power, 1000);
    }
}

#[tokio::test]
async fn craft_packet_rejects_unknown_deleted_missing_house_far_and_cross_instance_benches() {
    for case in 0..9 {
        let mut f = fixture(973, 1240, 0);
        let target = match case {
            0 => 0x999,
            1 => 0, // tax certificates require a station
            2 => {
                f.bench.data().phase.deleted = true;
                BENCH
            }
            3 => {
                f.bench.data().owner_db_id = 999;
                BENCH
            }
            4 => {
                f.bench.data().transform.set_xyz(200.0, 100.0, 100.0);
                BENCH
            }
            5 => {
                f.bench.data().transform.instance_id = 1;
                BENCH
            }
            6 => {
                f.house.data().unit.transform.instance_id = 1;
                BENCH
            }
            7 => {
                f.bench.data().transform.x = f32::NAN;
                BENCH
            }
            _ => f.owner.obj_id, // a unit is not a station
        };
        request(&f.server, &mut f.client, TAX, target, 1).await;
        assert_denied(&mut f);
        assert_eq!(f.visitor.data().labor_power, 1000, "case {case}");
    }
}

#[tokio::test]
async fn portable_craft_still_completes_without_a_doodad() {
    for target in [0, 0x100] {
        let mut f = fixture(973, 1240, 0);
        request(&f.server, &mut f.client, PORTABLE, target, 1).await;
        assert_eq!(certificates(&f.visitor), 5);
        assert_eq!(f.visitor.data().labor_power, 800);
    }
}

async fn wait_for_batch_stop(f: &Fixture) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f
            .server
            .systems
            .economy
            .craft
            .state(1)
            .current_craft
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("craft must finish or refuse the remaining batch");
}

#[tokio::test]
async fn changing_craft_function_during_a_batch_rechecks_before_product_commit() {
    for group in [RESTRICTED_GROUP, WRONG_PACK_GROUP] {
        // The real skill timer leaves a window to change the active phase during casting.
        let mut f = fixture(973, 1240, 100);
        crate::systems::skills::bind(&f.server);
        request(&f.server, &mut f.client, TAX, BENCH, 2).await;
        assert!(f.server.systems.economy.craft.is_crafting(1));
        assert_eq!(certificates(&f.visitor), 0);
        f.bench.data().func_group_id = group;
        wait_for_batch_stop(&f).await;
        assert_denied(&mut f);
    }
}

#[tokio::test]
async fn changing_craft_function_between_batch_steps_refuses_the_next_cast() {
    let mut f = fixture(973, 1240, 0);
    request(&f.server, &mut f.client, TAX, BENCH, 2).await;
    assert_eq!(certificates(&f.visitor), 5);
    assert_eq!(f.visitor.data().labor_power, 800);
    f.bench.data().func_group_id = RESTRICTED_GROUP;
    wait_for_batch_stop(&f).await;
    assert_eq!(certificates(&f.visitor), 5);
    assert_eq!(f.visitor.data().labor_power, 800);
    assert!(f
        .client
        .drain()
        .contains(&(sc::SCErrorMsgPacket, vec![148, 0, 135, 0, 0, 0, 0, 0, 0])));
}

#[tokio::test]
async fn changing_house_permission_during_a_batch_rechecks_before_product_commit() {
    let mut f = fixture(973, HOUSE_PERMISSION_GROUP, 100);
    f.house.data().permission = housing_permission::PUBLIC;
    crate::systems::skills::bind(&f.server);
    request(&f.server, &mut f.client, TAX, BENCH, 2).await;
    assert!(f.server.systems.economy.craft.is_crafting(1));
    f.house.data().permission = housing_permission::PRIVATE;
    wait_for_batch_stop(&f).await;
    assert_denied(&mut f);
}

#[tokio::test]
#[ignore]
async fn real_compact_db_bench_craft_packs_include_every_tax_plaque_phase() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let files =
        crate::data::client_files::ClientFileManager::from_sources(&[], std::path::Path::new("."));
    let ctx = crate::systems::DataLoadCtx {
        conn: &conn,
        files: &files,
        app_path: std::path::Path::new("."),
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.systems.economy.craft =
        crate::systems::economy::craft_data::CraftData::load(&ctx).unwrap();
    data.systems.npcs.doodad =
        crate::systems::npcs::doodad::data::DoodadData::load_from(&conn).unwrap();
    assert_eq!(
        data.systems.economy.craft.craft(TAX).unwrap().req_doodad_id,
        2392
    );
    assert!(data.systems.economy.craft.craft_pack_crafts[&3].contains(&TAX));
    // Exercise all phases that actually contain CraftPack (not the Garden's loot phase).
    let phases: Vec<(u32, u32)> = conn.prepare(
        "SELECT g.doodad_almighty_id, g.id FROM doodad_func_groups g JOIN doodad_funcs f ON f.doodad_func_group_id = g.id WHERE f.actual_func_type = 'DoodadFuncCraftPack' AND g.doodad_almighty_id IN (2392,973,5767,7340,7341,7342,8249,7308)"
    ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(phases.len(), 23);
    let server = test_server_with(data);
    for (template, group) in phases {
        let mut doodad = Doodad::new(template);
        doodad.func_group_id = group;
        assert_eq!(
            crate::systems::npcs::bridges::doodad_craft_permission(
                &server,
                &Entity::new(BENCH, doodad),
                TAX
            ),
            Some(permission::ANY)
        );
    }
}
