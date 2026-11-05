//! Recipe learning / learned-craft gate tests: the data loader, the real `CSStartSkill` item
//! cast through the skills pipeline, `CSExecuteCraft` through its packet handler, the
//! `CanLearnCraft` unit requirement, and the MySQL round trip.

use std::sync::{Arc, Mutex};

use aaemu_commons::PacketStream;

use crate::data::items::ItemTemplate;
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::craft::{CSExecuteCraft, CraftHooks};
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::tests::{give, item_at, run, test_data, world_with, World};
use crate::systems::skills::data::{
    EffectKind, EffectTemplate, SkillEffect, SkillTemplate, UnitReq,
};
use crate::systems::skills::handlers::CSStartSkillPacket;
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::systems::skills::unit_reqs;
use crate::world::{PlayerRef, WorldObject};

use super::CraftRecipes;

const RECIPE_ITEM: u32 = 9292;
const RECIPE_CRAFT: u32 = 987;
const TRAIN_EFFECT: u32 = 305;
const USE_SKILL: u32 = 11144;
const CRAFT_SKILL: u32 = 11076;

const TRAIN_SKILL: u32 = 11145;
const TRAIN_CRAFT: u32 = 169;
const TRAIN_EFFECT_2: u32 = 306;

fn skill_effect(effect_id: u32, kind: EffectKind, template_id: u32) -> SkillEffect {
    SkillEffect {
        effect_id,
        template: Some((kind, template_id)),
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
    }
}

fn data() -> GameData {
    let mut d = test_data();
    d.items.templates.insert(
        RECIPE_ITEM,
        ItemTemplate {
            use_skill_id: USE_SKILL,
            use_skill_as_reagent: true,
            ..ItemTemplate::plain(RECIPE_ITEM, 20)
        },
    );
    // The real recipe item is consumed by its generic use skill (every recipe item has
    // `use_skill_as_reagent = 't'`); the craft comes from `item_recipes`, not the effect row.
    d.systems.economy.craft.crafts.insert(
        RECIPE_CRAFT,
        crate::systems::economy::craft_data::Craft {
            id: RECIPE_CRAFT,
            skill_id: CRAFT_SKILL,
            ..Default::default()
        },
    );
    d.systems.economy.craft.skills.insert(
        CRAFT_SKILL,
        crate::systems::economy::craft_data::CraftSkill {
            id: CRAFT_SKILL,
            consume_labor_power: 0,
            actability_group_id: 0,
            cooldown_time: 0,
        },
    );
    d.systems.economy.craft.crafts.insert(
        TRAIN_CRAFT,
        crate::systems::economy::craft_data::Craft {
            id: TRAIN_CRAFT,
            ..Default::default()
        },
    );
    d.systems.economy.craft_learning = CraftRecipes::default();
    d.systems
        .economy
        .craft_learning
        .gated
        .extend([RECIPE_CRAFT, TRAIN_CRAFT]);
    d.systems
        .economy
        .craft_learning
        .item_crafts
        .insert(RECIPE_ITEM, vec![RECIPE_CRAFT]);
    d.systems
        .economy
        .craft_learning
        .effect_crafts
        .insert(TRAIN_EFFECT, RECIPE_CRAFT);
    d.systems
        .economy
        .craft_learning
        .effect_crafts
        .insert(TRAIN_EFFECT_2, TRAIN_CRAFT);

    d.systems.skills.add_effect(
        EffectKind::TrainCraftEffect,
        EffectTemplate::TrainCraft {
            id: TRAIN_EFFECT,
            craft_id: RECIPE_CRAFT,
        },
        1750,
    );
    d.systems.skills.add_effect(
        EffectKind::TrainCraftEffect,
        EffectTemplate::TrainCraft {
            id: TRAIN_EFFECT_2,
            craft_id: TRAIN_CRAFT,
        },
        1751,
    );
    d.systems.skills.add_skill(SkillTemplate {
        id: USE_SKILL,
        target_type: 0,
        effects: vec![skill_effect(
            1750,
            EffectKind::TrainCraftEffect,
            TRAIN_EFFECT,
        )],
        ..SkillTemplate::default()
    });
    d.systems.skills.add_skill(SkillTemplate {
        id: TRAIN_SKILL,
        target_type: 0,
        effects: vec![skill_effect(
            1751,
            EffectKind::TrainCraftEffect,
            TRAIN_EFFECT_2,
        )],
        ..SkillTemplate::default()
    });
    d.systems
        .skills
        .unit_reqs
        .entry("Skill".into())
        .or_default()
        .push(UnitReq {
            id: 1,
            owner_id: TRAIN_SKILL,
            owner_type: "Skill".into(),
            kind: 0x13,
            value1: TRAIN_CRAFT,
            value2: 0,
        });
    d
}

#[derive(Default)]
struct CraftRecorder {
    casts: Mutex<Vec<u32>>,
}

impl CraftHooks for CraftRecorder {
    fn start_craft_cast(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        skill_id: u32,
        _doodad_obj_id: u32,
        _cast_time_multiplier: f32,
    ) -> u8 {
        self.casts.lock().unwrap().push(skill_id);
        skill_result::SUCCESS
    }
}

fn fixture(require_learned_recipes: bool) -> (World, Arc<CraftRecorder>) {
    let mut w = world_with(data());
    w.server.config.update(|c| {
        c.crafting.require_learned_recipes = require_learned_recipes;
    });
    crate::systems::skills::bind(&w.server);
    crate::systems::economy::bridges::install(&w.server);
    let recorder = Arc::new(CraftRecorder::default());
    w.server
        .systems
        .economy
        .craft
        .hooks
        .install(recorder.clone());
    for p in [&w.pa, &w.pb] {
        w.server.items.add_container(Inv::of(p).bag());
    }
    w.a.drain();
    w.b.drain();
    (w, recorder)
}

async fn cast_recipe(w: &mut World, item_id: u64) {
    CSStartSkillPacket {
        skill_id: USE_SKILL,
        caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id,
            item_template_id: RECIPE_ITEM,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(w.pa.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
    .handle(&w.server, &mut w.a.conn)
    .await
    .unwrap();
}

fn execute_craft(w: &mut World) {
    let mut s = PacketStream::new();
    s.write_u32(RECIPE_CRAFT).write_bc(0).write_i32(1);
    run::<CSExecuteCraft>(w, 0, s.as_bytes());
}

fn error_pairs(w: &mut World) -> Vec<(i16, i16)> {
    w.a.drain()
        .iter()
        .filter(|(op, _)| *op == sc::SCErrorMsgPacket)
        .filter_map(|(_, body)| {
            let mut s = PacketStream::from_bytes(body.clone());
            Some((s.read_i16().ok()?, s.read_i16().ok()?))
        })
        .collect()
}

#[tokio::test]
async fn recipe_item_learns_its_craft_and_consumes_one() {
    let (mut w, _) = fixture(false);
    let item = give(&w, &w.pa, slot_type::INVENTORY, RECIPE_ITEM, 1, 0);
    cast_recipe(&mut w, item).await;
    assert!(w.pa.data().learned_crafts.contains(&RECIPE_CRAFT));
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_none());
    assert!(error_pairs(&mut w).is_empty());
}

#[tokio::test]
async fn second_recipe_use_is_refused_and_keeps_the_item() {
    let (mut w, _) = fixture(false);
    let item = give(&w, &w.pa, slot_type::INVENTORY, RECIPE_ITEM, 2, 0);
    cast_recipe(&mut w, item).await;
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 0).unwrap().count, 1);
    w.a.drain();

    cast_recipe(&mut w, item).await;
    // `craft_already_learned` under `CraftCantActAnyMore`? No: the effect sends the single
    // client error 0x81 (129), and the cancelled cast refunds the pre-debited item.
    assert_eq!(error_pairs(&mut w), vec![(129, 129)]);
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 0).unwrap().count, 1);
}

#[tokio::test]
async fn gate_off_allows_a_gated_craft() {
    let (mut w, recorder) = fixture(false);
    execute_craft(&mut w);
    assert!(!error_pairs(&mut w).iter().any(|(e1, _)| *e1 == 148));
    assert_eq!(recorder.casts.lock().unwrap().as_slice(), &[CRAFT_SKILL]);
}

#[tokio::test]
async fn gate_on_refuses_until_learned() {
    let (mut w, recorder) = fixture(true);
    execute_craft(&mut w);
    assert!(
        error_pairs(&mut w).contains(&(148, 130)),
        "expected CraftCantActAnyMore/craft_not_learned"
    );
    assert!(recorder.casts.lock().unwrap().is_empty());

    let item = give(&w, &w.pa, slot_type::INVENTORY, RECIPE_ITEM, 1, 0);
    cast_recipe(&mut w, item).await;
    assert!(w.pa.data().learned_crafts.contains(&RECIPE_CRAFT));
    w.a.drain();

    execute_craft(&mut w);
    assert!(
        !error_pairs(&mut w)
            .iter()
            .any(|(e1, e2)| *e1 == 148 && *e2 == 130),
        "the learned craft must pass the gate"
    );
    assert_eq!(recorder.casts.lock().unwrap().as_slice(), &[CRAFT_SKILL]);
}

#[tokio::test]
async fn learned_craft_refuses_the_training_skill_requirement() {
    let (w, _) = fixture(false);
    let template = w
        .server
        .game_data
        .systems
        .skills
        .skill(TRAIN_SKILL)
        .unwrap();
    let caster = SkillCaster::Unit(w.pa.obj_id);
    let owner = WorldObject::from(&w.pa);
    assert_eq!(
        unit_reqs::can_use_skill(&w.server, template, &owner, &caster),
        unit_reqs::ReqResult::OK
    );
    w.pa.data().learned_crafts.insert(TRAIN_CRAFT);
    let refused = unit_reqs::can_use_skill(&w.server, template, &owner, &caster);
    assert_eq!(refused.key, "skill_urk_can_learn_craft");
    assert_eq!(refused.value, TRAIN_CRAFT);
}

#[test]
fn recipe_loader_skips_unknown_crafts() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE crafts (id);
        INSERT INTO crafts VALUES (987), (169), (988);
        CREATE TABLE item_recipes (id, item_id, craft_id);
        INSERT INTO item_recipes VALUES (1, 9292, 987), (2, 9292, 988), (3, 10987, 2682);
        CREATE TABLE train_craft_effects (id, craft_id);
        INSERT INTO train_craft_effects VALUES (305, 169), (281, 275);",
    )
    .unwrap();
    let files =
        crate::data::client_files::ClientFileManager::from_sources(&[], std::path::Path::new("."));
    let recipes = CraftRecipes::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: std::path::Path::new("."),
        files: &files,
        language: "en_us",
    })
    .unwrap();
    assert_eq!(
        recipes.gated,
        std::collections::HashSet::from([987u32, 988, 169])
    );
    assert_eq!(recipes.crafts_for_item(9292), Some(&[987u32, 988][..]));
    assert!(recipes.crafts_for_item(10987).is_none());
    assert_eq!(recipes.craft_for_effect(305), Some(169));
    assert!(recipes.craft_for_effect(281).is_none());
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_data_links_recipe_items_and_train_effects() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let files =
        crate::data::client_files::ClientFileManager::from_sources(&[], std::path::Path::new("."));
    let recipes = CraftRecipes::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: std::path::Path::new("."),
        files: &files,
        language: "en_us",
    })
    .unwrap();
    assert_eq!(recipes.gated.len(), 2762);
    assert_eq!(recipes.crafts_for_item(9292), Some(&[987u32][..]));
    assert_eq!(recipes.craft_for_effect(305), Some(169));
    // The 48 recipe rows / 1 effect row whose craft is missing from `crafts` are skipped.
    assert!(recipes.crafts_for_item(10987).is_none());
    assert!(recipes.craft_for_effect(281).is_none());
    assert!(recipes.requires_learning(987));
    assert!(!recipes.requires_learning(8000001));
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_learned_crafts_round_trip() {
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = sqlx::mysql::MySqlConnectOptions::from_str(&url).unwrap();
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let (_, ddl): (String, String) = sqlx::query_as("SHOW CREATE TABLE `character_crafts`")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(ddl.starts_with("CREATE TABLE"));
    sqlx::query(&ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1))
        .execute(&pool)
        .await
        .unwrap();

    let mut crafts = std::collections::HashSet::from([RECIPE_CRAFT, TRAIN_CRAFT]);
    let mut tx = pool.begin().await.unwrap();
    super::save_character_crafts(&mut tx, 424242, &crafts)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        super::load_character_crafts(&pool, 424242).await.unwrap(),
        crafts
    );

    crafts.insert(988);
    let mut tx = pool.begin().await.unwrap();
    super::save_character_crafts(&mut tx, 424242, &crafts)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        super::load_character_crafts(&pool, 424242).await.unwrap(),
        crafts
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `character_crafts` WHERE `character_id` = 424242")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 3);
}
