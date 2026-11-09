//! Regression: a mate's dash bar skill fires although its rider skill carries
//! `source_cannot_use_while_walk` and the rider moved before mounting.
//!
//! The mount branch of `CSStartSkill` pre-checks the linked rider skill (`mount_attached_skills`)
//! before the mount cast. `validation::check` used to refuse a skill flagged
//! `source_cannot_use_while_walk` whenever the caster's last own move had a velocity
//! (the unit skill state's `moving` flag). A rider that walked up to its mate and mounted kept that flag for
//! good (the mate's moves go to the mate), so Stormdarter Run! 17092 (rider 18228) and Keep
//! Running! 17526 (rider 12815) were refused with result 20 `skill_cannot_cast_while_walking`
//! and no visible error. The C# never read the flag; the server no longer enforces it
//! (`PORTING.md`).
//!
//! * `a_mate_bar_skill_with_a_walk_flagged_rider_skill_fires_after_the_rider_moved`: synthetic
//!   rows, runs in CI.
//! * `stormdarter_dash_bar_skills_fire_after_the_rider_moved`: the real 1.2 rows (gated by
//!   `AAEMU_COMPACT_DB`).

use super::super::data::{MountAttachedSkills, MountSkills};
use super::*;
use crate::data::world_data::{FactionData, Localization};
use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::world::MateRef;

// Synthetic rows: bar entry `DASH_ROW` of `MATE_NPC` casts `DASH_SKILL`, whose driver rider
// skill is `DASH_RIDER`; both are flagged `source_cannot_use_while_walk` like the real ones.
const DASH_ROW: u32 = 11;
const DASH_SKILL: u32 = 300_001;
const DASH_RIDER: u32 = 300_002;

const STORMDARTER: u32 = 7043;
const RUN_SKILL: u32 = 17_092;
const RUN_ROW: u32 = 83;
const RUN_RIDER: u32 = 18_228;
/// The buff Run! gives the mate.
const RUN_BUFF: u32 = 1_933;
const KEEP_RUNNING_SKILL: u32 = 17_526;
const KEEP_RUNNING_ROW: u32 = 108;
const KEEP_RUNNING_RIDER: u32 = 12_815;

struct Rider {
    client: TestClient,
    player: PlayerRef,
    mate: MateRef,
}

/// Player `n` summons its level-`level` mate and mounts it as DRIVER. `moved_before_mount`
/// replays what the client does while walking up to the mount: a character move with a
/// velocity (`CSMoveUnit` -> `skills::on_move_request`).
fn mounted_rider(server: &Arc<GameServer>, n: u32, level: u16, moved_before_mount: bool) -> Rider {
    let mut client = TestClient::new(server, n, n);
    let (player, item_id) = player_with_item(
        server,
        &client,
        n,
        PA + n,
        (100.0 + 20.0 * n as f32, 100.0),
        MATE_ITEM,
    );
    {
        let mut c = player.data();
        c.unit.level = 30;
        c.unit.hp = 5_000;
        c.unit.max_hp = 5_000;
        c.unit.mp = 5_000;
        c.unit.max_mp = 5_000;
    }
    client.conn.active_char = Some(player.clone());
    // A new mate starts at its npc row's level (5 for the Stormdarter); seed the saved row.
    let now = chrono::Utc::now().naive_utc();
    server.systems.vehicles.mates.character(n, |cm| {
        cm.mates.push(mates::MateDb {
            id: 700 + n,
            item_id,
            name: "Mate".into(),
            xp: 0,
            level,
            mileage: 0,
            hp: 10_000,
            mp: 10_000,
            owner: n,
            updated_at: now,
            created_at: now,
        });
    });
    mates::spawn_mount(server, &player, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(n)
        .pop()
        .expect("the mate spawned");
    {
        let mut m = mate.data();
        assert_eq!(u16::from(m.unit.level), level);
        m.unit.max_hp = m.unit.max_hp.max(10_000);
        m.unit.hp = m.unit.max_hp;
        m.unit.max_mp = m.unit.max_mp.max(10_000);
        m.unit.mp = m.unit.max_mp;
    }
    if moved_before_mount {
        crate::systems::skills::on_move_request(server, &player, 10, 0, 0);
    }
    let tl = u32::from(mate.data().unit.tl_id);
    mates::mount_mate(server, &player, tl, attach_point::DRIVER, 1);
    assert!(player.data().is_riding, "the rider sits on the mate");
    assert_eq!(player.data().attached_point, attach_point::DRIVER);
    client.drain();
    Rider {
        client,
        player,
        mate,
    }
}

fn bar_cast(mate_obj_id: u32, skill_id: u32, row: u32) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id,
        caster: SkillCaster::Mount {
            obj_id: mate_obj_id,
            mount_skill_template_id: row,
        },
        target: SkillCastTarget::Unit(mate_obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

fn buff_ids(obj: &WorldObject) -> Vec<u32> {
    obj.with_unit(|u| {
        u.unit()
            .buffs
            .effects
            .iter()
            .filter(|b| !b.is_ended())
            .map(|b| b.template.id)
            .collect()
    })
    .unwrap_or_default()
}

/// Presses bar entry `row` as the client does and asserts that the bar skill and its rider skill
/// both fired and that no refusal (`SCSkillStarted` with a result) came back.
async fn press_and_expect_fired(
    server: &Arc<GameServer>,
    rider: &mut Rider,
    skill: u32,
    row: u32,
    rider_skill: u32,
    target_is_mate: bool,
    what: &str,
) {
    rider.player.data().unit.current_target = if target_is_mate { rider.mate.obj_id } else { 0 };
    start_skill(
        server,
        &mut rider.client.conn,
        bar_cast(rider.mate.obj_id, skill, row),
    );
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(1_500)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let frames = rider.client.drain();
    assert_eq!(
        started_result(&frames, skill),
        None,
        "{what}: the press was refused: {:x?}",
        ops(&frames)
    );
    let fired = skill_ids_of(&frames, sc::SCSkillFiredPacket);
    assert!(
        fired.contains(&skill) && fired.contains(&rider_skill),
        "{what}: the bar skill {skill} and its rider skill {rider_skill} must both fire, fired {fired:?}: {:x?}",
        ops(&frames)
    );
}

fn dash_data() -> GameData {
    let mut d = data();
    for id in [DASH_SKILL, DASH_RIDER] {
        let mut skill = crate::systems::skills::tests::skill_template(id);
        skill.target_type = 0; // Self
        skill.damage_type_id = 0;
        skill.source_cannot_use_while_walk = true;
        d.systems.skills.add_skill(skill);
    }
    let vehicles = &mut d.systems.vehicles;
    vehicles.mount_skills.push(MountSkills {
        id: DASH_ROW,
        name: "dash".into(),
        skill_id: DASH_SKILL,
    });
    vehicles.mount_attached_skills.push(MountAttachedSkills {
        id: 1,
        mount_skill_id: DASH_ROW,
        attach_point_id: attach_point::DRIVER,
        skill_id: DASH_RIDER,
    });
    d
}

#[tokio::test(start_paused = true)]
async fn a_mate_bar_skill_with_a_walk_flagged_rider_skill_fires_after_the_rider_moved() {
    let server = server_with(dash_data());
    crate::systems::skills::bind(&server);
    let d = &server.game_data.systems.vehicles;
    assert_eq!(d.mate_skills(MATE_NPC), vec![DASH_ROW]);
    assert_eq!(
        d.mount_attached_skill(DASH_ROW, attach_point::DRIVER),
        DASH_RIDER
    );
    for (n, target_is_mate) in [(1, true), (2, false)] {
        let mut rider = mounted_rider(&server, n, 5, true);
        press_and_expect_fired(
            &server,
            &mut rider,
            DASH_SKILL,
            DASH_ROW,
            DASH_RIDER,
            target_is_mate,
            &format!("rider moved before mounting, target_is_mate {target_is_mate}"),
        )
        .await;
    }
}

/// The real skill, vehicle, npc, item and formula rows a Stormdarter mate cast needs.
fn dash_server(db: &str) -> Arc<GameServer> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = app_path();
    let files = ClientFileManager::from_sources(&[], &app);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    let mut stmt = conn
        .prepare("SELECT id, total_exp, total_mate_exp, skill_points FROM levels ORDER BY id ASC")
        .expect("levels query");
    let levels: Vec<LevelTemplate> = stmt
        .query_map([], |r| {
            Ok(LevelTemplate {
                level: r.get::<_, i64>(0)? as u8,
                total_exp: r.get::<_, i64>(1)? as i32,
                total_mate_exp: r.get::<_, i64>(2)? as i32,
                skill_points: r.get::<_, i64>(3)? as i32,
            })
        })
        .expect("levels")
        .collect::<Result<_, _>>()
        .expect("levels rows");
    drop(stmt);
    data.experience = ExperienceTable::from_templates(levels, 55).expect("experience");
    data.features = crate::data::FeatureSet::initialized(
        data.experience.max_player_level(),
        data.experience.max_mate_level(),
    );
    data.items = crate::data::ItemData::load(&conn).expect("item data");
    data.skills = crate::data::SkillData::load(&conn).expect("skill data");
    data.formulas = crate::data::FormulaManager::load(&conn).expect("formulas");
    let localization = Localization::load(&conn, "en_us").expect("localization");
    data.factions = FactionData::load(&conn, &localization).expect("factions");
    data.systems.vehicles = super::super::Data::load(&ctx).expect("vehicles data");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    data.systems.npcs = crate::systems::npcs::Data::load(&ctx).expect("npcs data");
    data.systems.economy = crate::systems::economy::Data::load(&ctx).expect("economy data");
    // The summon item that spawns the Stormdarter (the real item id does not matter).
    data.items.templates.insert(
        MATE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonMate {
                npc_id: STORMDARTER,
            },
            ..ItemTemplate::plain(MATE_ITEM, 1)
        },
    );
    let server = real_game_server(data);
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    server
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn stormdarter_dash_bar_skills_fire_after_the_rider_moved() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let server = dash_server(&db);
    let d = &server.game_data.systems.vehicles;
    let mut bar = d.mate_skills(STORMDARTER);
    bar.sort_unstable();
    assert_eq!(bar, vec![14, 83, 101, 102, 108]);
    assert_eq!(
        d.mount_attached_skill(RUN_ROW, attach_point::DRIVER),
        RUN_RIDER
    );
    assert_eq!(
        d.mount_attached_skill(KEEP_RUNNING_ROW, attach_point::DRIVER),
        KEEP_RUNNING_RIDER
    );
    for skill in [RUN_RIDER, KEEP_RUNNING_RIDER] {
        assert!(
            server
                .game_data
                .systems
                .skills
                .skill(skill)
                .expect("rider template")
                .source_cannot_use_while_walk,
            "rider skill {skill} carries the flag"
        );
    }

    // Run! on the level-23 Stormdarter: the rider never moved / moved before mounting, with the
    // mate and with nothing as the current target.
    for (n, moved, target_is_mate) in [
        (1, false, true),
        (2, false, false),
        (3, true, true),
        (4, true, false),
    ] {
        let mut rider = mounted_rider(&server, n, 23, moved);
        let what = format!("Run!, moved {moved}, target_is_mate {target_is_mate}");
        press_and_expect_fired(
            &server,
            &mut rider,
            RUN_SKILL,
            RUN_ROW,
            RUN_RIDER,
            target_is_mate,
            &what,
        )
        .await;
        assert!(
            buff_ids(&WorldObject::from(&rider.mate)).contains(&RUN_BUFF),
            "{what}: the mate gets buff {RUN_BUFF}"
        );
    }

    // Keep Running! carries the same flag on its rider skill 12815.
    let mut rider = mounted_rider(&server, 5, 25, true);
    press_and_expect_fired(
        &server,
        &mut rider,
        KEEP_RUNNING_SKILL,
        KEEP_RUNNING_ROW,
        KEEP_RUNNING_RIDER,
        true,
        "Keep Running!, moved, target_is_mate true",
    )
    .await;
}
