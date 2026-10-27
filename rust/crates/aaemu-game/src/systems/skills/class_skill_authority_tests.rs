//! Client casts of ability-tree skills need an active ability and (when `need_learn`) a learned
//! skill; the 1.2 client enforces this locally, the server must too.
//!
//! The tests drive `CSStartSkillPacket` so they run against a server that still accepts any
//! client skill id: the three `refused_*` tests fail there, every other source keeps casting.

use std::sync::Arc;

use crate::data::GameData;
use crate::models::char_data::Skill;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

use crate::systems::skills::data::{BuffTemplate, SkillTemplate};
use crate::systems::skills::types::skill_result;

use super::{class_skill_denial, CSStartSkillPacket, SkillCastTarget, SkillCaster, SkillObject};

const ABILITY: u8 = 3;
const TREE_SKILL: u32 = 31001;
const VARIANT_SKILL: u32 = 31002;
const BUFF_SKILL: u32 = 31003;
const QUEST_SKILL: u32 = 31004;
const DOODAD_SKILL: u32 = 31005;
const ITEM_SKILL: u32 = 31006;
const TOGGLE_SKILL: u32 = 31007;
const TOGGLE_BUFF: u32 = 51007;
const ITEM_TEMPLATE: u32 = crate::systems::economy::tests::STACK;

fn tree_skill(id: u32) -> SkillTemplate {
    SkillTemplate {
        id,
        ability_id: ABILITY,
        ability_level: 1,
        level_step: 10,
        need_learn: true,
        target_type: 9,
        max_range: 4,
        ..SkillTemplate::default()
    }
}

fn plain_skill(id: u32) -> SkillTemplate {
    // Ability-less, like the buff / quest / doodad interaction skills of the 1.2 data.
    SkillTemplate {
        id,
        target_type: 9,
        max_range: 4,
        ..SkillTemplate::default()
    }
}

fn data() -> GameData {
    let mut data = GameData::empty();
    data.systems.skills.add_skill(tree_skill(TREE_SKILL));
    data.systems.skills.add_skill(tree_skill(VARIANT_SKILL));
    for id in [BUFF_SKILL, QUEST_SKILL, DOODAD_SKILL] {
        data.systems.skills.add_skill(plain_skill(id));
        // The loader marks these ids from `buff_skills` / `quest_*` / `doodad_func_*`.
        data.skills.add_client_sourced_skill(id);
    }
    let mut toggle = tree_skill(TOGGLE_SKILL);
    toggle.toggle_buff_id = TOGGLE_BUFF;
    data.systems.skills.add_skill(toggle);
    data.systems.skills.add_buff(BuffTemplate {
        id: TOGGLE_BUFF,
        kind: 1,
        duration: 0,
        stack_rule: 1,
        ..Default::default()
    });
    data
}

fn fixture(
    active: bool,
    learned: bool,
    client_sourced: bool,
) -> (Arc<crate::GameServer>, TestClient, PlayerRef) {
    let mut data = data();
    if client_sourced {
        // The loader marks every id named by the player-facing source tables.
        data.skills.add_client_sourced_skill(TREE_SKILL);
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut ch = test_character(1, "Caster", 100.0, 100.0);
    ch.ability1 = if active { ABILITY } else { 1 };
    if learned {
        ch.skills.skills.push(Skill {
            id: TREE_SKILL,
            level: 1,
        });
    }
    let player = spawn_player(&server, &client, ch, 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    (server, client, player)
}

fn packet(skill_id: u32, caster: SkillCaster, target: u32) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id,
        caster,
        target: SkillCastTarget::Unit(target),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

async fn cast(server: &crate::GameServer, client: &mut TestClient, p: &PlayerRef, skill: u32) {
    packet(skill, SkillCaster::Unit(p.obj_id), p.obj_id)
        .handle(server, &mut client.conn)
        .await
        .unwrap();
}

fn started_result(client: &mut TestClient, skill: u32) -> Option<u8> {
    client
        .drain()
        .into_iter()
        .find_map(|(op, body)| (op == sc::SCSkillStartedPacket).then_some(body))
        .and_then(|body| {
            let mut s = aaemu_commons::PacketStream::from_bytes(body);
            if s.read_u32().ok() != Some(skill) {
                return None;
            }
            let _ = s.read_u16();
            // Unit caster / unit target / no skill object: the shapes these tests send.
            let _ = s.read_u8();
            let _ = s.read_bc();
            let _ = s.read_u8();
            let _ = s.read_bc();
            let _ = s.read_u8();
            let _ = s.read_u16();
            let _ = s.read_u16();
            let _ = s.read_u8();
            let flags = s.read_u8().ok()?;
            (flags & 1 != 0).then(|| s.read_u8().ok()).flatten()
        })
}

#[tokio::test]
async fn unlearned_tree_skill_of_an_active_ability_is_refused_with_invalid_skill() {
    let (server, mut client, p) = fixture(true, false, false);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert_eq!(p.data().unit.skill_last_used_ms, 0);
    assert_eq!(
        started_result(&mut client, TREE_SKILL),
        Some(skill_result::INVALID_SKILL)
    );
}

#[tokio::test]
async fn unlearned_sourced_tree_skill_is_refused_with_invalid_skill() {
    // Static tables name this class skill (a quest / doodad condition), but the character never
    // learned it: the 1.2 client refuses it in the action bar.
    let (server, mut client, p) = fixture(true, false, true);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert_eq!(p.data().unit.skill_last_used_ms, 0);
    assert_eq!(
        started_result(&mut client, TREE_SKILL),
        Some(skill_result::INVALID_SKILL)
    );
}

#[tokio::test]
async fn learned_tree_skill_of_an_inactive_ability_is_refused_with_inactive_ability() {
    let (server, mut client, p) = fixture(false, true, false);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert_eq!(p.data().unit.skill_last_used_ms, 0);
    assert_eq!(
        started_result(&mut client, TREE_SKILL),
        Some(skill_result::INACTIVE_ABILITY)
    );
}

#[tokio::test]
async fn learned_tree_skill_of_an_active_ability_casts() {
    let (server, mut client, p) = fixture(true, true, false);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert!(p.data().unit.skill_last_used_ms > 0);
    assert!(!client
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCSkillStartedPacket));
}

#[tokio::test]
async fn variant_of_a_learned_tree_skill_casts() {
    let (server, mut client, p) = fixture(true, true, false);
    cast(&server, &mut client, &p, VARIANT_SKILL).await;
    assert!(p.data().unit.skill_last_used_ms > 0);
}

#[tokio::test]
async fn ability_less_buff_quest_and_doodad_skills_stay_castable() {
    for skill in [BUFF_SKILL, QUEST_SKILL, DOODAD_SKILL] {
        let (server, mut client, p) = fixture(true, false, true);
        cast(&server, &mut client, &p, skill).await;
        assert!(p.data().unit.skill_last_used_ms > 0, "skill {skill}");
    }
}

#[tokio::test]
async fn toggle_skill_of_an_active_ability_casts() {
    let (server, mut client, p) = fixture(true, true, false);
    cast(&server, &mut client, &p, TOGGLE_SKILL).await;
    assert!(p.data().unit.skill_last_used_ms > 0);
}

#[tokio::test]
async fn item_granted_tree_skill_casts_without_learning() {
    let mut data = crate::systems::economy::tests::test_data();
    data.systems.skills.add_skill(tree_skill(ITEM_SKILL));
    data.items
        .templates
        .get_mut(&ITEM_TEMPLATE)
        .expect("item template")
        .use_skill_id = ITEM_SKILL;
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut ch = test_character(1, "ItemCaster", 100.0, 100.0);
    ch.ability1 = 1;
    let p = spawn_player(&server, &client, ch, 0x100);
    client.conn.active_char = Some(p.clone());
    server
        .items
        .add_container(crate::systems::economy::inventory::Inv::of(&p).bag());
    let item = server
        .items
        .create(
            ITEM_TEMPLATE,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("item");
    let item_id = item.id;
    crate::systems::economy::inventory::Inv::of(&p)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, 0, &server.game_data.items)
        .expect("bag add");
    client.drain();
    packet(
        ITEM_SKILL,
        SkillCaster::Item {
            obj_id: p.obj_id,
            item_id,
            item_template_id: ITEM_TEMPLATE,
            type1: 0,
            type2: 0,
        },
        p.obj_id,
    )
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert!(p.data().unit.skill_last_used_ms > 0);
}

#[tokio::test]
async fn denial_codes_are_the_1_2_result_codes() {
    let (server, _client, p) = fixture(true, false, false);
    assert_eq!(
        class_skill_denial(&server, &p, TREE_SKILL),
        Some(skill_result::INVALID_SKILL)
    );
    assert_eq!(
        class_skill_denial(&server, &p, VARIANT_SKILL),
        Some(skill_result::INVALID_SKILL)
    );
    assert_eq!(class_skill_denial(&server, &p, BUFF_SKILL), None);
    let (server, _client, p) = fixture(false, true, false);
    assert_eq!(
        class_skill_denial(&server, &p, TREE_SKILL),
        Some(skill_result::INACTIVE_ABILITY)
    );
    let (server, _client, p) = fixture(true, true, false);
    assert_eq!(class_skill_denial(&server, &p, TREE_SKILL), None);
    assert_eq!(class_skill_denial(&server, &p, VARIANT_SKILL), None);
}

#[tokio::test]
async fn mount_caster_tree_skill_casts_without_learning() {
    // `may_cast_as` accepts the player itself as a `Mount` caster; the mount branch runs before
    // the class authority check, exactly as real mate / slave casts do.
    let (server, mut client, p) = fixture(true, false, false);
    packet(
        TREE_SKILL,
        SkillCaster::Mount {
            obj_id: p.obj_id,
            mount_skill_template_id: TREE_SKILL,
        },
        p.obj_id,
    )
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert!(p.data().unit.skill_last_used_ms > 0);
}

/// Sources whose skills must stay castable without an ability tree: item use, mount-attached
/// rider skills, buff grants, crafts, NPC interactions, slash commands, quests, housing /
/// shipyard steps. Every id they name is ability-less in the 1.2 data, so the class check
/// returns `None` before the learned / fallback branches.
const ABILITY_LESS_SOURCES: &[(&str, &str)] = &[
    ("items", "use_skill_id"),
    ("item_grade_skills", "skill_id"),
    ("item_procs", "skill_id"),
    ("item_cap_scales", "skill_id"),
    ("buff_skills", "skill_id"),
    ("mount_attached_skills", "skill_id"),
    ("crafts", "skill_id"),
    ("npc_interactions", "skill_id"),
    ("slash_commands", "skill_id"),
    ("quest_components", "skill_id"),
    ("quest_act_supply_skills", "skill_id"),
    ("quest_act_check_timers", "skill_id"),
    ("housing_build_steps", "skill_id"),
    ("shipyard_steps", "skill_id"),
    ("default_skills", "skill_id"),
    ("gimmicks", "skill_id"),
    ("gimmicks", "collision_skill_id"),
    ("sphere_skills", "skill_id"),
    ("ai_events", "skill_id"),
];

/// The only ability-tree skills named by a client-facing doodad / quest / sphere / mount table:
/// 13572 is a login-stage mount skill (the mount branch handles it), the others are quest or
/// doodad conditions for class skills the player must still learn and have active.
const CLASS_TREE_SOURCE_EXCEPTIONS: &[u32] = &[13572, 10664, 10134, 12075, 11380];

const CLASS_TREE_SOURCES: &[(&str, &str)] = &[
    ("mount_skills", "skill_id"),
    ("doodad_funcs", "func_skill_id"),
    ("doodad_func_uses", "skill_id"),
    ("doodad_func_fake_uses", "skill_id"),
    ("doodad_func_fake_uses", "fake_skill_id"),
    ("doodad_func_conditional_uses", "fake_skill_id"),
    ("doodad_func_renew_items", "skill_id"),
    ("doodad_func_evidence_item_loots", "skill_id"),
    ("doodad_func_skill_hits", "skill_id"),
    ("sphere_doodad_interacts", "skill_id"),
    ("quest_act_con_accept_skills", "skill_id"),
];

/// Pins the data invariant the authority check relies on. Future compact patches that hand a
/// class-tree skill to one of these sources fail here so the cast policy is reviewed.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_data_skill_sources_do_not_smuggle_class_tree_skills() {
    use rusqlite::OptionalExtension;

    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open compact.sqlite3");
    let ability = |id: u32| -> Option<i64> {
        conn.query_row("SELECT ability_id FROM skills WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .expect("skills lookup")
    };
    let ids = |table: &str, column: &str| -> Vec<u32> {
        let sql = format!("SELECT DISTINCT {column} FROM {table} WHERE {column} > 0");
        let mut stmt = conn
            .prepare(&sql)
            .unwrap_or_else(|e| panic!("{table}.{column}: {e}"));
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap_or_else(|e| panic!("{table}.{column}: {e}"));
        rows.filter_map(|r| u32::try_from(r.expect("id")).ok())
            .collect()
    };

    let mut known = 0;
    let mut class_tree = 0;
    for &(table, column) in ABILITY_LESS_SOURCES {
        let mut count = 0;
        for id in ids(table, column) {
            if let Some(a) = ability(id) {
                count += 1;
                assert_eq!(
                    a, 0,
                    "{table}.{column} names class-tree skill {id} (ability {a}); its cast would \
                     now need an active ability and learning"
                );
            }
        }
        known += count;
        eprintln!("{table}.{column}: {count} known, all ability-less");
    }
    for &(table, column) in CLASS_TREE_SOURCES {
        for id in ids(table, column) {
            if let Some(a) = ability(id) {
                if a != 0 {
                    class_tree += 1;
                    assert!(
                        CLASS_TREE_SOURCE_EXCEPTIONS.contains(&id),
                        "{table}.{column} names new class-tree skill {id} (ability {a}); decide \
                         whether its cast path is a grant or a learned class skill"
                    );
                }
            }
        }
    }
    for id in CLASS_TREE_SOURCE_EXCEPTIONS {
        assert!(
            matches!(ability(*id), Some(a) if a != 0),
            "exception {id} is no longer a class-tree skill"
        );
    }

    let (total, ability_tree): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), SUM(ability_id <> 0) FROM skills",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("skills count");
    let need_learn: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM skills WHERE ability_id <> 0 AND need_learn = 'f'",
            [],
            |r| r.get(0),
        )
        .expect("need_learn count");
    eprintln!(
        "skills {total}, class-tree {ability_tree}, ability-less source ids {known}, \
         class-tree source ids {class_tree}"
    );
    assert_eq!(ability_tree, 170, "unexpected class-tree skill count");
    // `need_learn` is stored as 't' / 'f'. These tree skills only need their ability active.
    assert_eq!(
        need_learn, 27,
        "the number of ability-tree skills castable without learning changed"
    );
    assert!(class_tree > 0, "no class-tree source was checked");
}
