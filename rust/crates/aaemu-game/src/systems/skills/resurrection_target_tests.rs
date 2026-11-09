//! The 1.2 resurrection scroll skills must accept a DEAD target.
//!
//! `special_effects` type 22 (`Resurrection`) is used by 9 skills, among them 15066 / 25518
//! ("부활" / "전투 중 부활", items 18456 / 32568 / 34022) and 26647 ("저승의 문턱 탈출
//! 주문서", items 34018 / 34021). The template flags come from compact.sqlite3:
//! 15066 / 25518 are `target_type_id 1` (Friendly), `target_alive 'f'`, `target_dead 't'`,
//! `source_alive 't'`; 26647 is `target_type_id 0` (SelfTarget) with `source_dead 't'`, i.e. the
//! dead character casts it on itself. The effect values are `special_effects` 3269 (5 / 5),
//! 15974 (80 / 80) and 16520 (80 / 80).
//!
//! The cast goes through the real `CSStartSkillPacket` handler. The production source is the
//! scroll item (`items.use_skill_id`); the fixture marks the skills as client-sourced because
//! the hand-built data has no item table, so the same handler branch reaches `use_skill`.

use std::sync::Arc;

use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::skills::data::effects::SpecialEffect;
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

/// `(skill id, special_effects id, target is the dead player, hp %, mp %)`.
const SCROLLS: [(u32, u32, bool, i32, i32); 3] = [
    (15066, 3269, true, 5, 5),
    (25518, 15974, true, 80, 80),
    (26647, 16520, false, 80, 80),
];

fn data() -> GameData {
    let mut data = GameData::empty();
    for (skill_id, effect_id, friendly_target, hp, mp) in SCROLLS {
        let (target_type, target_selection) = if friendly_target {
            (1, 2) // Friendly, target selection
        } else {
            (0, 1) // SelfTarget, source selection
        };
        let (source_alive, source_dead, target_alive) = if friendly_target {
            (true, false, false)
        } else {
            (false, true, true)
        };
        data.systems.skills.add_effect(
            EffectKind::SpecialEffect,
            EffectTemplate::Special(SpecialEffect {
                id: effect_id,
                special_effect_type_id: 22,
                value1: 1,
                value2: hp,
                value3: mp,
                value4: 0,
            }),
            0,
        );
        data.systems.skills.add_skill(SkillTemplate {
            id: skill_id,
            target_type,
            target_selection,
            target_alive,
            target_dead: true,
            source_alive,
            source_dead,
            max_range: if friendly_target { 25 } else { 4 },
            // The real cast times are 7 s (15066/25518) and 0 (26647); the fixture casts
            // instantly so the effect runs in the same call.
            casting_time: 0,
            effects: vec![super::tests::skill_effect(
                effect_id,
                EffectKind::SpecialEffect,
                effect_id,
            )],
            ..SkillTemplate::default()
        });
        // Production reaches these through `items.use_skill_id`; the fixture has no item
        // table, so the handler's "the static data hands this skill to players" gate is
        // satisfied by marking it client-sourced.
        data.skills.add_client_sourced_skill(skill_id);
    }
    data
}

struct Fixture {
    server: Arc<GameServer>,
    caster_client: TestClient,
    caster: PlayerRef,
    dead_client: TestClient,
    dead: PlayerRef,
}

fn fixture() -> Fixture {
    let server = Arc::new(test_server_with(data()));
    super::bind(&server);
    let mut caster_client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Caster", 100.0, 100.0);
    c.unit.level = 50;
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    c.unit.mp = 1000;
    c.unit.max_mp = 1000;
    let caster = spawn_player(&server, &caster_client, c, 0x100);
    caster_client.conn.active_char = Some(caster.clone());
    let mut dead_client = TestClient::new(&server, 2, 2);
    let mut d = test_character(2, "Dead", 105.0, 100.0);
    d.unit.level = 50;
    d.unit.hp = 0;
    d.unit.max_hp = 1000;
    d.unit.mp = 0;
    d.unit.max_mp = 1000;
    let dead = spawn_player(&server, &dead_client, d, 0x101);
    dead_client.conn.active_char = Some(dead.clone());
    Fixture {
        server,
        caster_client,
        caster,
        dead_client,
        dead,
    }
}

fn cast(server: &GameServer, who: &PlayerRef, client: &mut TestClient, skill_id: u32, target: u32) {
    who.data().unit.skill_last_used_ms = 0;
    who.data().unit.global_cooldown_ms = 0;
    start_skill(
        server,
        &mut client.conn,
        CSStartSkillPacket {
            skill_id,
            caster: SkillCaster::Unit(who.obj_id),
            target: SkillCastTarget::Unit(target),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
}

#[tokio::test]
async fn resurrection_scrolls_cast_on_a_dead_player() {
    let mut f = fixture();
    let _ = f.caster_client.drain();
    let _ = f.dead_client.drain();
    for (skill_id, _effect, friendly_target, hp, mp) in SCROLLS {
        {
            let mut d = f.dead.data();
            d.skill_char.resurrect_offer = false;
        }
        if friendly_target {
            let dead_obj = f.dead.obj_id;
            cast(
                &f.server,
                &f.caster,
                &mut f.caster_client,
                skill_id,
                dead_obj,
            );
        } else {
            // 26647 is the dead character's own escape scroll.
            let dead = f.dead.clone();
            cast(&f.server, &dead, &mut f.dead_client, skill_id, dead.obj_id);
        }
        let frames = f.dead_client.drain();
        assert!(
            frames
                .iter()
                .any(|(op, _)| *op == sc::SCNotifyResurrectionPacket),
            "skill {skill_id} must offer the resurrection to the dead player, got {:?}",
            frames.iter().map(|(op, _)| *op).collect::<Vec<_>>()
        );
        let d = f.dead.data();
        assert!(d.skill_char.resurrect_offer, "skill {skill_id}");
        assert_eq!(
            (
                d.skill_char.resurrect_hp_percent,
                d.skill_char.resurrect_mp_percent
            ),
            (hp as u32, mp as u32),
            "skill {skill_id}"
        );
        assert_eq!(d.unit.hp, 0, "the offer alone does not revive");
    }
}

#[tokio::test]
async fn a_living_target_does_not_get_a_resurrection_offer() {
    let mut f = fixture();
    // The friendly-target scrolls refuse a living target (`target_alive 'f'`).
    let _ = f.caster_client.drain();
    let _ = f.dead_client.drain();
    f.dead.data().unit.hp = 1000;
    let dead_obj = f.dead.obj_id;
    cast(&f.server, &f.caster, &mut f.caster_client, 25518, dead_obj);
    assert!(!f
        .dead_client
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCNotifyResurrectionPacket));
    assert!(!f.dead.data().skill_char.resurrect_offer);
}

/// The real scroll items and skill templates from compact.sqlite3. The hand-built test above
/// uses the same targeting flags; this one drives the production `SkillCaster::Item` path with
/// the shipped items 32568 (25518) and 34018 (26647), including their real cast times (set to
/// 0 after loading so the effect runs in the test call).
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_scroll_items_cast_on_a_dead_player() {
    let Some(mut data) = real_data() else {
        return;
    };
    // Only the 7 s cast time is shortened; the targeting flags, effect values and item links
    // stay as shipped.
    for skill_id in [15066, 25518] {
        let mut t = (**data.systems.skills.skill(skill_id).expect("skill")).clone();
        t.casting_time = 0;
        data.systems.skills.add_skill(t);
    }
    let server = real_server(data);
    let mut caster_client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Caster", 100.0, 100.0);
    c.unit.level = 50;
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    c.unit.mp = 1000;
    c.unit.max_mp = 1000;
    let caster = spawn_player(&server, &caster_client, c, 0x100);
    caster_client.conn.active_char = Some(caster.clone());
    let mut dead_client = TestClient::new(&server, 2, 2);
    let mut d = test_character(2, "Dead", 105.0, 100.0);
    d.unit.level = 50;
    d.unit.hp = 0;
    d.unit.max_hp = 1000;
    d.unit.mp = 0;
    d.unit.max_mp = 1000;
    let dead = spawn_player(&server, &dead_client, d, 0x101);
    dead_client.conn.active_char = Some(dead.clone());
    let _ = caster_client.drain();
    let _ = dead_client.drain();
    // The item hook resolves owned items through the item manager's container registry.
    server
        .items
        .add_container(crate::systems::economy::inventory::Inv::of(&caster).bag());
    server
        .items
        .add_container(crate::systems::economy::inventory::Inv::of(&dead).bag());

    // 32568 "전투 중 부활 주문서" (use skill 25518), cast on the dead player.
    let item = give_item(&server, &caster, 32568);
    cast_item(
        &server,
        &mut caster_client.conn,
        &caster,
        25518,
        32568,
        item,
        dead.obj_id,
    );
    assert!(
        dead_client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCNotifyResurrectionPacket),
        "item 32568 must offer the resurrection"
    );
    {
        let d = dead.data();
        assert!(d.skill_char.resurrect_offer);
        assert_eq!(
            (
                d.skill_char.resurrect_hp_percent,
                d.skill_char.resurrect_mp_percent
            ),
            (80, 80)
        );
        assert_eq!(d.unit.hp, 0);
    }

    // 34018 "저승의 문턱 탈출 주문서" (use skill 26647), used by the dead player on itself.
    let item = give_item(&server, &dead, 34018);
    cast_item(
        &server,
        &mut dead_client.conn,
        &dead,
        26647,
        34018,
        item,
        dead.obj_id,
    );
    assert!(
        dead_client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCNotifyResurrectionPacket),
        "item 34018 must offer the resurrection"
    );
    assert!(dead.data().skill_char.resurrect_offer);
}

fn real_data() -> Option<GameData> {
    let path = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], &app);
    let localization = crate::data::Localization::load(&conn, "en_us").unwrap();
    let mut data = GameData::empty();
    data.experience = crate::test_support::test_experience(55);
    data.items = crate::data::items::ItemData::load(&conn).unwrap();
    data.factions = crate::data::FactionData::load(&conn, &localization).unwrap();
    data.skills = crate::data::skills::SkillData::load(&conn).unwrap();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).unwrap();
    data.systems = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    })
    .unwrap();
    Some(data)
}

fn real_server(mut data: GameData) -> Arc<GameServer> {
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::world_templates::{WorldTemplate, WorldTemplates};

    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    data.worlds = WorldTemplates::from_templates(
        vec![WorldTemplate::synthetic(0, "main_world", 2, 2, 100)],
        Vec::new(),
    )
    .expect("world");
    for id in crate::world::unit_state::UNIT_STATE_FORCED_BUFFS {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 0;
        data.skills.buffs.insert(id, t);
    }
    let mut config = crate::config::AppConfig {
        id: 1,
        ..crate::config::AppConfig::default()
    };
    config.app_path = app;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, pool, data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::ai::hooks::install(&server);
    server
}

fn give_item(server: &GameServer, owner: &PlayerRef, template_id: u32) -> u64 {
    let item = server
        .items
        .create(
            template_id,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("scroll item");
    let id = item.id;
    let bag = crate::systems::economy::inventory::Inv::of(owner).bag();
    let mut bag = bag.lock().unwrap();
    bag.add_new_item(item, -1, &server.game_data.items)
        .expect("bag space");
    id
}

fn cast_item(
    server: &GameServer,
    conn: &mut crate::net::connection::GameConnection,
    who: &PlayerRef,
    skill_id: u32,
    template_id: u32,
    item_id: u64,
    target: u32,
) {
    who.data().unit.skill_last_used_ms = 0;
    who.data().unit.global_cooldown_ms = 0;
    start_skill(
        server,
        conn,
        CSStartSkillPacket {
            skill_id,
            caster: SkillCaster::Item {
                obj_id: who.obj_id,
                item_id,
                item_template_id: template_id,
                type1: 0,
                type2: 0,
            },
            target: SkillCastTarget::Unit(target),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
}
