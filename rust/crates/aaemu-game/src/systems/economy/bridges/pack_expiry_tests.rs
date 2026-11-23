//! Dropped trade packs: the put-down backpack doodad expires on its own (`DoodadFuncFinal.After`,
//! 6 days for most packs), also after a restart, and the pack item expires with it instead of
//! staying behind in its owner's System container.

use std::sync::Arc;

use crate::data::items::{backpack_kind, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::character::Inventory;
use crate::models::container::{equip_slot, slot_type};
use crate::models::doodad::doodad_owner_type;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::item_effects::{self, SkillItem};
use crate::systems::economy::tests::{give, test_data, World};
use crate::systems::npcs::doodad::templates::*;
use crate::systems::npcs::doodad::{db, interactions, manager};
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{DoodadRef, WorldObject};

const PACK: u32 = 18_001;
const USE_SKILL: u32 = 17_001;
const DROPPED: u32 = 6_059;
const GROUP: u32 = 15_668;
const RECOVER_SKILL: u32 = 11_361;
/// `DoodadFuncFinal.After` of 254 of the 335 put-down backpack doodads: 6 days.
const SIX_DAYS_MS: u64 = 518_400_000;
const DAY_MS: u64 = 86_400_000;

/// The shape of doodad 6059 (a Mahadevi cloth bundle): one start group with a
/// `DoodadFuncRecoverItem` (pick the pack up) and a `DoodadFuncFinal` phase func.
fn data() -> GameData {
    let mut data = test_data();
    data.items.templates.insert(
        PACK,
        ItemTemplate {
            kind: TemplateKind::Backpack {
                backpack_type: backpack_kind::TRADE_PACK,
            },
            use_skill_id: USE_SKILL,
            ..ItemTemplate::plain(PACK, 1)
        },
    );
    // The pack's use skill puts it down as DROPPED.
    data.systems.skills.add_effect(
        EffectKind::PutDownBackpackEffect,
        EffectTemplate::PutDownBackpack {
            id: 100,
            backpack_doodad_id: DROPPED,
        },
        0,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: USE_SKILL,
        effects: vec![SkillEffect {
            effect_id: 100,
            template: Some((EffectKind::PutDownBackpackEffect, 100)),
            weight: 0,
            start_level: 0,
            end_level: 255,
            friendly: true,
            non_friendly: true,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            source_buff_tag_id: 0,
            source_no_buff_tag_id: 0,
            chance: 100,
            front: false,
            back: false,
            target_npc_tag_id: 0,
            application_method: 1,
            consume_source_item: false,
            consume_item_id: 0,
            consume_item_count: 1,
            always_hit: true,
            item_set_id: 0,
            interaction_success_hit: false,
        }],
        ..Default::default()
    });
    let group = DoodadFuncGroups {
        id: GROUP,
        almighty: DROPPED,
        group_kind_id: doodad_func_group_kind::START,
        ..Default::default()
    };
    let doodads = &mut data.systems.npcs.doodad;
    doodads.templates.insert(
        DROPPED,
        DoodadTemplate {
            id: DROPPED,
            func_groups: vec![group.clone()],
            ..Default::default()
        },
    );
    doodads.template_order.push(DROPPED);
    doodads
        .all_func_groups_index
        .insert(GROUP, doodads.all_func_groups.len());
    doodads.all_func_groups.push(group);
    doodads
        .phase_func_templates
        .entry("DoodadFuncFinal")
        .or_default()
        .insert(
            2969,
            FuncTemplate::Final(DoodadFuncFinal {
                id: 2969,
                after: SIX_DAYS_MS as i32,
                ..Default::default()
            }),
        );
    doodads.phase_funcs.insert(
        GROUP,
        vec![DoodadPhaseFunc {
            group_id: GROUP,
            func_id: 2969,
            func_type: "DoodadFuncFinal".into(),
        }],
    );
    doodads
        .func_templates
        .entry("DoodadFuncRecoverItem")
        .or_default()
        .insert(
            604,
            FuncTemplate::RecoverItem(DoodadFuncRecoverItem { id: 604 }),
        );
    doodads.funcs_by_groups.insert(
        GROUP,
        vec![DoodadFunc {
            group_id: GROUP,
            func_id: 604,
            func_key: 12_909,
            func_type: "DoodadFuncRecoverItem".into(),
            next_phase: -1,
            skill_id: RECOVER_SKILL,
            ..Default::default()
        }],
    );
    data
}

fn world() -> World {
    let server = Arc::new(test_server_with(data()));
    server.set_self();
    // The doodad timers run on a virtual clock.
    server.systems.npcs.doodads.tasks.set_manual(true);
    crate::systems::economy::bridges::install(&server);
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    let mut alice = test_character(1, "Alice", 100.0, 100.0);
    alice.inventory = Inventory::new(1, 50, 50, &server.items, &server.ids.container);
    let mut bob = test_character(2, "Bob", 101.0, 100.0);
    bob.inventory = Inventory::new(2, 50, 50, &server.items, &server.ids.container);
    let pa = spawn_player(&server, &a, alice, server.ids.object.get_next_id().unwrap());
    let pb = spawn_player(&server, &b, bob, server.ids.object.get_next_id().unwrap());
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    a.drain();
    b.drain();
    World {
        server,
        a,
        pa,
        b,
        pb,
    }
}

fn equip_pack(w: &World) -> u64 {
    give(
        w,
        &w.pa.clone(),
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    )
}

fn put_down(w: &World, pack: u64) -> DoodadRef {
    item_effects::put_down_backpack(
        &w.server,
        &w.pa,
        DROPPED,
        Some(SkillItem {
            item_id: pack,
            item_template_id: PACK,
        }),
    );
    standing_pack(w)
}

fn standing_pack(w: &World) -> DoodadRef {
    let doodads = w.server.world.world(0).unwrap().all_doodads();
    assert_eq!(doodads.len(), 1, "exactly one pack doodad stands");
    doodads.into_iter().next().unwrap()
}

fn in_system(w: &World, pack: u64) -> bool {
    let system = Inv::of(&w.pa).get(slot_type::SYSTEM).unwrap();
    let held = system.lock().unwrap().get_item_by_id(pack).is_some();
    held
}

fn advance(w: &World, ms: u64) {
    w.server.systems.npcs.doodads.tasks.advance(&w.server, ms);
}

fn standing(w: &World) -> usize {
    w.server.world.world(0).unwrap().all_doodads().len()
}

#[tokio::test]
async fn put_down_pack_expires_after_six_days_and_takes_the_pack_with_it() {
    let w = world();
    let pack = equip_pack(&w);
    let doodad = put_down(&w, pack);
    assert!(doodad.data().is_persistent);
    assert!(in_system(&w, pack));
    assert_eq!(
        w.server.systems.npcs.doodads.tasks.pending_delays(),
        vec![SIX_DAYS_MS]
    );
    // The client is told the remaining lifetime (`TimeLeft`, "scheduled to disappear").
    let left = u64::from(doodad.data().time_left(crate::world::now_ms()));
    assert!(
        (SIX_DAYS_MS - 5_000..=SIX_DAYS_MS).contains(&left),
        "{left}"
    );

    advance(&w, SIX_DAYS_MS - 1);
    assert_eq!(standing(&w), 1);
    assert!(in_system(&w, pack));

    advance(&w, 1);
    assert_eq!(standing(&w), 0);
    assert!(doodad.data().phase.deleted);
    assert!(!in_system(&w, pack));
    assert!(w.server.items.item_by_id(pack).is_none());
    assert!(w.server.items.removed_items().contains(&pack));
}

#[tokio::test]
async fn pack_dropped_on_death_expires_like_a_put_down_pack() {
    let w = world();
    let pack = equip_pack(&w);
    // `Character.DoDie` -> `DropTradePackToFloor`
    item_effects::drop_trade_pack_to_floor(&w.server, &w.pa);
    assert!(Inv::of(&w.pa).equipped(equip_slot::BACKPACK).is_none());
    let doodad = standing_pack(&w);
    assert_eq!(doodad.data().template_id, DROPPED);
    assert!(doodad.data().is_persistent);
    assert_eq!(doodad.data().item_id, pack);
    assert_eq!(doodad.data().owner_type, doodad_owner_type::CHARACTER);
    assert!(in_system(&w, pack));
    assert_eq!(
        w.server.systems.npcs.doodads.tasks.pending_delays(),
        vec![SIX_DAYS_MS]
    );

    advance(&w, SIX_DAYS_MS);
    assert_eq!(standing(&w), 0);
    assert!(w.server.items.item_by_id(pack).is_none());
    assert!(w.server.items.removed_items().contains(&pack));
}

/// What the doodad row looks like after the server restarted: the pack doodad is loaded from
/// `doodads` (owner type 254) with its saved `phase_time`, and `SpawnAll` starts its phase.
async fn restart(w: &World, doodad: &DoodadRef, down_for_ms: i64) {
    use crate::systems::npcs::spawn_manager::PersistentDoodads;
    let instance = w.server.world.world(0).unwrap();
    let mut row = db::snapshot(&w.server, doodad).unwrap();
    assert_eq!(row.owner_type, doodad_owner_type::CHARACTER);
    // The pack stood for `down_for_ms` of its life when the server was shut down...
    row.phase_time -= chrono::Duration::milliseconds(down_for_ms);
    // ...and the old process (its timers, its world) is gone. Only the world object goes: the
    // pack item stays in the System container, as it does in the database.
    instance.delete(doodad);
    assert_eq!(standing(w), 0);
    w.server.systems.npcs.doodads.tasks.set_manual(true);
    let server = w.server.clone();
    *w.server
        .systems
        .npcs
        .spawns
        .test_persistent_loader
        .lock()
        .unwrap() = Some(Arc::new(move |_type, _id| {
        let (server, row) = (server.clone(), row.clone());
        Box::pin(async move { PersistentDoodads::prepare(&server, vec![row]) })
    }));
    let spawns = w.server.systems.npcs.spawns.get_or_create(&instance);
    let loaded = spawns
        .spawn_persistent_doodads(&w.server, doodad_owner_type::CHARACTER, -1, None, false)
        .await
        .unwrap();
    assert_eq!(loaded, 1);
    spawns.spawn_player_doodads(&w.server);
}

#[tokio::test]
async fn pack_restored_after_a_restart_keeps_its_remaining_lifetime() {
    let w = world();
    let pack = equip_pack(&w);
    let doodad = put_down(&w, pack);
    // Down for two days: four days left, not a fresh six.
    restart(&w, &doodad, 2 * DAY_MS as i64).await;
    assert_eq!(standing(&w), 1);
    let delays = w.server.systems.npcs.doodads.tasks.pending_delays();
    let left = delays.last().copied().unwrap();
    assert!(
        (4 * DAY_MS - 5_000..=4 * DAY_MS).contains(&left),
        "{delays:?}"
    );
    advance(&w, left - 1);
    assert_eq!(standing(&w), 1);
    assert!(in_system(&w, pack));
    advance(&w, 1);
    assert_eq!(standing(&w), 0);
    assert!(w.server.items.item_by_id(pack).is_none());
}

#[tokio::test]
async fn pack_that_expired_while_the_server_was_down_is_removed_at_startup() {
    let w = world();
    let pack = equip_pack(&w);
    let doodad = put_down(&w, pack);
    restart(&w, &doodad, 7 * DAY_MS as i64).await;
    assert_eq!(standing(&w), 1);
    // Due at once (1 ms): the first tick of the task scheduler.
    advance(&w, 10);
    assert_eq!(standing(&w), 0);
    assert!(w.server.items.item_by_id(pack).is_none());
    assert!(w.server.items.removed_items().contains(&pack));
}

#[tokio::test]
async fn recovered_pack_is_not_deleted_when_its_doodad_goes() {
    let w = world();
    let pack = equip_pack(&w);
    let doodad = put_down(&w, pack);
    assert!(Inv::of(&w.pa).equipped(equip_slot::BACKPACK).is_none());
    // The owner picks the pack up again (`WorldInteraction.RecoverItem`).
    assert!(interactions::execute_interaction(
        &w.server,
        interactions::world_interaction_type::RECOVER_ITEM,
        &WorldObject::from(&w.pa),
        Some(&WorldObject::from(&doodad)),
        RECOVER_SKILL,
        0,
        None
    ));
    assert_eq!(standing(&w), 0, "the doodad goes with the pick-up");
    assert_eq!(
        Inv::of(&w.pa).equipped(equip_slot::BACKPACK).unwrap().id,
        pack
    );
    assert!(w.server.items.item_by_id(pack).is_some());
    assert!(!w.server.items.removed_items().contains(&pack));
    // The expiry timer of the gone doodad leaves the pack alone.
    advance(&w, SIX_DAYS_MS);
    assert_eq!(
        Inv::of(&w.pa).equipped(equip_slot::BACKPACK).unwrap().id,
        pack
    );
    assert!(!w.server.items.removed_items().contains(&pack));
}

#[tokio::test]
async fn pack_still_referenced_by_another_live_doodad_is_kept() {
    let w = world();
    let pack = equip_pack(&w);
    let doodad = put_down(&w, pack);
    // A duplicate of the doodad that also stands for this pack.
    let instance = w.server.world.world(0).unwrap();
    let twin = manager::create(
        &w.server,
        &instance,
        0,
        DROPPED,
        Some(&WorldObject::from(&w.pa)),
        true,
    )
    .unwrap();
    twin.data().item_id = pack;
    twin.data().item_template_id = PACK;
    twin.data().transform.set_xyz(120.0, 130.0, 100.0);
    manager::spawn(&w.server, &instance, &twin);
    manager::delete(&w.server, &instance, &doodad);
    assert!(in_system(&w, pack));
    assert!(w.server.items.item_by_id(pack).is_some());
    // The last doodad that stands for it takes it along.
    manager::delete(&w.server, &instance, &twin);
    assert!(!in_system(&w, pack));
    assert!(w.server.items.removed_items().contains(&pack));
}

/// The 335 `put_down_backpack_effects` doodads of the real data: every trade pack expires on its
/// own (the lifetime is data: `doodad_func_finals.after`), except the ones the data gives no
/// expiry at all.
#[tokio::test]
#[ignore]
async fn real_compact_db_put_down_pack_doodads_expire() {
    use crate::systems::npcs::doodad::data::DoodadData;
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let ids: Vec<u32> = conn
        .prepare("SELECT DISTINCT backpack_doodad_id FROM put_down_backpack_effects ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut gd = GameData::empty();
    gd.systems.npcs.doodad = DoodadData::load_from(&conn).unwrap();
    let server = Arc::new(test_server_with(gd));
    server.set_self();
    server.systems.npcs.doodads.tasks.set_manual(true);
    let world = server.world.world(0).unwrap();
    let mut made = Vec::new();
    for id in ids {
        // A few put-down effects name doodad templates that are not in the data.
        let Some(d) = manager::create(&server, &world, 0, id, None, true) else {
            continue;
        };
        d.data().transform.set_xyz(120.0, 130.0, 100.0);
        manager::init_doodad(&server, &world, &d);
        manager::spawn(&server, &world, &d);
        made.push((id, d));
    }
    assert!(made.len() > 300, "{}", made.len());
    // The cloth bundle of the shape above: Final.After = 6 days.
    let bundle = made.iter().find(|(id, _)| *id == 6059).unwrap();
    assert!(!bundle.1.data().phase.deleted);
    let left = u64::from(bundle.1.data().time_left(crate::world::now_ms()));
    assert!(
        (SIX_DAYS_MS - 5_000..=SIX_DAYS_MS).contains(&left),
        "{left}"
    );
    server
        .systems
        .npcs
        .doodads
        .tasks
        .advance(&server, 7 * DAY_MS);
    let mut forever: Vec<u32> = made
        .iter()
        .filter(|(_, d)| !d.data().phase.deleted)
        .map(|(id, _)| *id)
        .collect();
    forever.sort_unstable();
    // The siege declaration flag (3304) and four aged crafts (phonograph, honey, wine, cheese)
    // have no Final in the data.
    assert_eq!(forever, vec![3059, 3304, 7000, 7001, 7002]);
}
