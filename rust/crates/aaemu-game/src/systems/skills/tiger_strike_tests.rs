//! Tiger Strike (skill 13315, plot 17) follows the authored plot timing: the leap event 118
//! (SkillController 2278, 400 ms) reaches the client before the damage event 121, whose
//! `plot_next_events` row 129 puts it 400 ms after 118. The 1.2 client drives the leap from
//! the SCPlotEvent and does not leap onto a unit that is already dead, so the damage and the
//! death must not precede the leap.
//!
//! The plot rows are the real ones (events 1071/118/1675/119/121/4740/11659 with their
//! `plot_next_events` delays and `plot_effects`); they are loaded through the production
//! [`PlotData::load`] from an in-memory database so the test runs without `AAEMU_COMPACT_DB`.
//! The real compact rows can be read with:
//! `sqlite3 -readonly compact.sqlite3 "select * from plot_events where plot_id=17"`.

use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::PacketStream;
use rusqlite::{Connection, OpenFlags};

use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::test_support::TestClient;
use crate::world::{now_ms, Entity, NpcRef, PlayerRef, WorldObject};

use crate::systems::skills::data::effects::{DispelEffect, SpecialEffect};
use crate::systems::skills::data::plots::PlotData;
use crate::systems::skills::data::{
    EffectKind, EffectTemplate, SkillControllerTemplate, SkillTemplate,
};
use crate::systems::skills::skill::use_skill;
use crate::systems::skills::state::{SkillRef, SkillRun};
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};

const SKILL: u32 = 13315;
const PLOT: u32 = 17;

const CASTER_FACTION: u32 = 4;
const MOB_FACTION: u32 = 3;

/// The fixture resolves the two sides as Hostile (`RelationState`, not a faction id); the mob
/// uses the real hostile root faction 3, the caster a fixture-only faction 4.
const HOSTILE: u8 = 1;

fn plot_rows(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE aoe_diminishings (id, rate);
         CREATE TABLE plots (id, name, target_type_id);
         CREATE TABLE plot_events (id, plot_id, position, name, source_update_method_id,
           target_update_method_id, target_update_method_param1, target_update_method_param2,
           target_update_method_param3, tickets, target_update_method_param4,
           target_update_method_param5, target_update_method_param6,
           target_update_method_param7, aoe_diminishing, target_update_method_param8,
           target_update_method_param9);
         CREATE TABLE plot_conditions (id, not_condition, kind_id, param1, param2, param3);
         CREATE TABLE plot_event_conditions (id, event_id, condition_id, position, source_id, target_id, notify_failure);
         CREATE TABLE plot_aoe_conditions (id, event_id, condition_id, position);
         CREATE TABLE plot_effects (id, event_id, position, source_id, target_id, actual_id, actual_type);
         CREATE TABLE plot_next_events (id, event_id, position, next_event_id, per_target, casting,
           delay, speed, channeling, casting_inc, add_anim_cs_time, casting_delayable,
           casting_cancelable, cancel_on_big_hit, use_exe_time, fail);

         INSERT INTO plots VALUES (17, 'Tiger Strike', 4);
         -- id, plot, pos, name, src, tgt, p1, p2, p3, tickets, p4, p5, p6, p7, dim, p8, p9
         INSERT INTO plot_events VALUES (1071, 17, 1, 'start', 3, 4, 0, 0, 0, 1, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES (118, 17, 2, 'first hit', 3, 4, 5000, 1, 2000, 1, 0, 0, 2000, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES (1675, 17, 3, 'source target', 1, 2, 0, 0, 0, 4, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES (119, 17, 4, 'second hit', 3, 6, 1137, 1, 4, 4, 39, 0, 2000, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES (121, 17, 5, 'damage', 3, 4, 0, 0, 0, 5, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES (4740, 17, 6, 'auto attack', 3, 4, 0, 0, 0, 1, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES (11659, 17, 7, 'charge', 3, 4, 0, 0, 0, 1, 0, 0, 0, 0, 'f', 0, 0);

         -- id, event, pos, next, per_target, casting, delay, speed, channeling, casting_inc,
         -- add_anim_cs_time, casting_delayable, casting_cancelable, cancel_on_big_hit,
         -- use_exe_time, fail
         INSERT INTO plot_next_events VALUES (1241, 1071, 1, 118, 'f', 'f', 1, 0, 'f', 0, 'f', 'f', 'f', 'f', 't', 'f');
         INSERT INTO plot_next_events VALUES (123, 118, 1, 1675, 'f', 'f', 400, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (129, 118, 2, 121, 'f', 'f', 400, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (13629, 118, 3, 11659, 'f', 'f', 0, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (1918, 1675, 1, 119, 'f', 'f', 0, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (1919, 1675, 2, 1675, 'f', 'f', 200, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (130, 119, 1, 121, 'f', 'f', 200, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (5720, 121, 1, 4740, 'f', 'f', 0, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');

         -- id, event, pos, source, target, actual_id, actual_type
         INSERT INTO plot_effects VALUES (328, 118, 1, 3, 4, 1634, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (329, 118, 2, 3, 4, 2278, 'SkillController');
         INSERT INTO plot_effects VALUES (643, 118, 3, 3, 3, 1876, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (5171, 118, 4, 1, 3, 6377, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (1327, 118, 5, 3, 3, 2486, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (1700, 118, 6, 3, 3, 208, 'DispelEffect');
         INSERT INTO plot_effects VALUES (332, 119, 1, 3, 4, 1636, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (333, 119, 2, 3, 4, 2279, 'SkillController');
         INSERT INTO plot_effects VALUES (5082, 121, 1, 3, 4, 6286, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (338, 121, 2, 1, 4, 945, 'DamageEffect');
         INSERT INTO plot_effects VALUES (8216, 1071, 1, 1, 3, 9164, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (1889, 1071, 2, 1, 3, 3179, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (4726, 1071, 3, 1, 3, 5892, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (7706, 4740, 1, 1, 3, 8711, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (17359, 11659, 1, 1, 1, 12052, 'BuffEffect');",
    )
    .expect("plot fixture");
}

fn special(id: u32, kind: i32, values: [i32; 4]) -> EffectTemplate {
    EffectTemplate::Special(SpecialEffect {
        id,
        special_effect_type_id: kind,
        value1: values[0],
        value2: values[1],
        value3: values[2],
        value4: values[3],
    })
}

fn leap(id: u32, duration_ms: i32) -> EffectTemplate {
    // compact `skill_controllers` 2278 / 2279: kind 2 (Leap), Value2 = 20 m, Value3 = duration,
    // Value4 = 1000 mm offset, Value6 = 1, Value9/Value10 = 1.
    let mut value = [0i32; 15];
    value[1] = 20;
    value[2] = duration_ms;
    value[3] = 1000;
    value[5] = 1;
    value[7] = 1;
    value[8] = 1;
    value[9] = 1;
    value[11] = 1;
    EffectTemplate::SkillController(SkillControllerTemplate {
        id,
        kind_id: 2,
        active_weapon_id: 1,
        value,
    })
}

fn add_hostile_factions(data: &mut GameData) {
    data.factions.add(SystemFaction {
        id: CASTER_FACTION,
        ..Default::default()
    });
    data.factions.add(SystemFaction {
        id: MOB_FACTION,
        ..Default::default()
    });
    data.factions.relations.push(FactionRelation {
        id: CASTER_FACTION,
        id2: MOB_FACTION,
        state: HOSTILE,
        exp_time: 0,
    });
}

fn data() -> GameData {
    let conn = Connection::open_in_memory().expect("in-memory db");
    plot_rows(&conn);
    data_with(PlotData::load(&conn).expect("plot 17 fixture"))
}

/// The plot-17 fixture plus a dedicated synthetic plot for the stalled-flush test (its own
/// ids so the test hook cannot reach the other plot tests running in parallel).
const STALLED_PLOT: u32 = 9_000_017;
const STALLED_ROOT_EVENT: u32 = 9_001;
const STALLED_LEAP_EVENT: u32 = 9_002;
const STALLED_DAMAGE_EVENT: u32 = 9_003;

fn stalled_plot_rows(conn: &Connection) {
    conn.execute_batch(&format!(
        "INSERT INTO plots VALUES ({STALLED_PLOT}, 'stalled', 4);
         INSERT INTO plot_events VALUES ({STALLED_ROOT_EVENT}, {STALLED_PLOT}, 1, 'start', 1, 2, 0, 0, 0, 1, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES ({STALLED_LEAP_EVENT}, {STALLED_PLOT}, 2, 'leap', 1, 2, 0, 0, 0, 1, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_events VALUES ({STALLED_DAMAGE_EVENT}, {STALLED_PLOT}, 3, 'damage', 1, 2, 0, 0, 0, 1, 0, 0, 0, 0, 'f', 0, 0);
         INSERT INTO plot_next_events VALUES (91001, {STALLED_ROOT_EVENT}, 1, {STALLED_LEAP_EVENT}, 'f', 'f', 0, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_next_events VALUES (91002, {STALLED_LEAP_EVENT}, 1, {STALLED_DAMAGE_EVENT}, 'f', 'f', 400, 0, 'f', 0, 'f', 'f', 'f', 'f', 'f', 'f');
         INSERT INTO plot_effects VALUES (92001, {STALLED_ROOT_EVENT}, 1, 1, 2, 9164, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (92002, {STALLED_LEAP_EVENT}, 1, 1, 2, 1634, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (92003, {STALLED_LEAP_EVENT}, 2, 1, 2, 2278, 'SkillController');
         INSERT INTO plot_effects VALUES (92004, {STALLED_DAMAGE_EVENT}, 1, 1, 2, 6286, 'SpecialEffect');
         INSERT INTO plot_effects VALUES (92005, {STALLED_DAMAGE_EVENT}, 2, 1, 2, 945, 'DamageEffect');"
    ))
    .expect("stalled plot fixture");
}

fn stalled_plot_data() -> GameData {
    let conn = Connection::open_in_memory().expect("in-memory db");
    plot_rows(&conn);
    stalled_plot_rows(&conn);
    data_with(PlotData::load(&conn).expect("plot + stalled fixture"))
}

fn data_with(plots: PlotData) -> GameData {
    let mut data = GameData::empty();
    add_hostile_factions(&mut data);
    data.systems.skills.plots = plots;
    let s = &mut data.systems.skills;
    // compact `aoe_shapes` 1137: kind 1, 10 m (the event 119 bounce radius).
    s.area_shapes.insert(
        1137,
        crate::systems::skills::data::misc::AreaShape {
            id: 1137,
            kind: 1,
            value1: 10.0,
            value2: 0.0,
            value3: 0.0,
        },
    );
    // The effects of the real plot rows. Only the leap, the damage and the special effects
    // that carry a plot packet are needed here; `BuffEffect` 12052 has no template and is
    // skipped by the effect dispatcher exactly like a missing client effect.
    s.add_effect(EffectKind::SkillController, leap(2278, 400), 2278);
    s.add_effect(EffectKind::SkillController, leap(2279, 200), 2279);
    s.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(crate::systems::skills::tests::damage_effect(
            945, 1_000,
        ))),
        945,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(1634, 36, [440, 0, 0, 0]),
        1634,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(1636, 36, [440, 0, 0, 0]),
        1636,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(1876, 40, [18_000, 0, 0, 0]),
        1876,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(2486, 52, [0, 0, 0, 0]),
        2486,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(3179, 55, [0, 0, 0, 0]),
        3179,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(5892, 61, [0, 0, 0, 0]),
        5892,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(6286, 67, [5, 0, 0, 0]),
        6286,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(6377, 39, [0, 383, 0, 0]),
        6377,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(8711, 66, [1, 0, 0, 0]),
        8711,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        special(9164, 41, [0, 0, 0, 0]),
        9164,
    );
    s.add_effect(
        EffectKind::DispelEffect,
        EffectTemplate::Dispel(DispelEffect {
            id: 208,
            dispel_count: 0,
            cure_count: 1,
            buff_tag_id: 10,
        }),
        208,
    );
    data
}

/// The real 1.2 catalog for plot 17 and skill 13315 (gated by `AAEMU_COMPACT_DB`).
fn real_game_data(conn: &Connection) -> GameData {
    let mut data = GameData::empty();
    add_hostile_factions(&mut data);
    data.formulas = crate::data::FormulaManager::load(conn).expect("formula catalog");
    data.systems.skills = crate::systems::skills::Data::load_from(
        conn,
        &crate::data::client_files::ClientFileManager::new(),
    )
    .expect("skill catalog");
    // The real skill row requires an equipped mainhand (`unit_reqs` kind 8, slot 15). This
    // test drives the plot timing and does not equip one; every other requirement stays.
    if let Some(reqs) = data.systems.skills.unit_reqs.get_mut("Skill") {
        reqs.retain(|r| r.owner_id != SKILL);
    }
    data
}

fn skill(plot_id: u32) -> SkillRef {
    // The real skill row shape: plot-only, 20 m, target angle 180, no cast time.
    SkillRun::new(
        Arc::new(SkillTemplate {
            id: SKILL,
            plot_id: Some(plot_id),
            plot_only: true,
            target_alive: true,
            target_relation: 0,
            max_range: 20,
            target_angle: 180,
            ..crate::systems::skills::tests::skill_template(SKILL)
        }),
        1,
    )
}

fn fixture(
    data: GameData,
    mobs: usize,
) -> (Arc<crate::GameServer>, TestClient, PlayerRef, Vec<NpcRef>) {
    let (server, client, player, _) = crate::systems::skills::cast_validation_tests::fixture(data);
    player.data().unit.faction_id = CASTER_FACTION;
    crate::systems::skills::stats::refresh_cached_stats(&server, &WorldObject::from(&player));
    let mobs = (0..mobs)
        .map(|i| {
            let mut n = Npc::new(1);
            n.unit.hp = 100;
            n.unit.max_hp = 100;
            n.unit.mp = 100;
            n.unit.max_mp = 100;
            n.unit.level = 10;
            n.unit.faction_id = MOB_FACTION;
            // 5 m in front of the caster; a second mob 1 m beside the first stays inside the
            // 10 m `RandomUnit` bounce shape of event 119.
            n.unit.transform.set_xyz(100.0 + i as f32, 105.0, 100.0);
            let npc = Entity::new(0x200 + i as u32, n);
            server
                .world
                .world(0)
                .unwrap()
                .spawn(&npc, &server.view_ctx());
            npc
        })
        .collect();
    (server, client, player, mobs)
}

/// One observed client packet relevant to the leap timing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    t_ms: i64,
    what: &'static str,
    event_id: Option<u32>,
    target: Option<u32>,
    /// The clicked mob's server HP when the packet was drained.
    mob_hp: i32,
}

fn read_plot_object(s: &mut PacketStream) -> Option<u32> {
    match s.read_u8().ok()? {
        1 => Some(s.read_bc().ok()?),
        2 => {
            s.read_position().ok()?;
            s.read_i8().ok()?;
            s.read_i8().ok()?;
            s.read_i8().ok()?;
            Some(0)
        }
        _ => None,
    }
}

/// `(event id, target unit id, body length)` of one `SCPlotEventPacket` body.
fn plot_event(body: &[u8]) -> Option<(u32, Option<u32>, usize)> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    s.read_u16().ok()?; // tl
    let event_id = s.read_u32().ok()?;
    s.read_u32().ok()?; // skill id
    read_plot_object(&mut s)?; // caster
    let target = read_plot_object(&mut s)?;
    s.read_u64().ok()?; // item id
    s.read_bc().ok()?; // obj id
    s.read_u16().ok()?; // casting time
    s.read_bc().ok()?; // unknown
    s.read_i16().ok()?; // unknown
    let count = s.read_u8().ok()?;
    for _ in 0..count {
        s.read_bc().ok()?;
    }
    let flag = s.read_u8().ok()?;
    if (flag >> 3) & 1 == 1 {
        for _ in 0..13 {
            s.read_i32().ok()?;
        }
    }
    Some((event_id, (target != 0).then_some(target), s.pos()))
}

/// Drains both plain packets and level-4 `CompressedGamePackets`, recording every
/// `SCPlotEventPacket`, `SCUnitDamagedPacket` and `SCUnitDeathPacket`.
fn drain_records(client: &mut TestClient, started: i64, mob: &NpcRef, out: &mut Vec<Record>) {
    let mob_hp = mob.data().unit.hp;
    for frame in client.drain_frames() {
        let bytes = &frame.bytes[..];
        let now = now_ms() - started;
        match bytes.get(1) {
            Some(1) => {
                let op = u16::from_le_bytes([bytes[4], bytes[5]]);
                let body = &bytes[6..];
                match op {
                    sc::SCPlotEventPacket => {
                        if let Some((event_id, target, _)) = plot_event(body) {
                            out.push(Record {
                                t_ms: now,
                                what: "plot",
                                event_id: Some(event_id),
                                target,
                                mob_hp,
                            });
                        }
                    }
                    sc::SCUnitDamagedPacket => out.push(Record {
                        t_ms: now,
                        what: "damaged",
                        event_id: None,
                        target: None,
                        mob_hp,
                    }),
                    sc::SCUnitDeathPacket => out.push(Record {
                        t_ms: now,
                        what: "death",
                        event_id: None,
                        target: None,
                        mob_hp,
                    }),
                    _ => {}
                }
            }
            Some(4) => {
                let Some(payload) = crate::systems::skills::packets::inflate_stored(&bytes[4..])
                else {
                    continue;
                };
                let mut pos = 0usize;
                while pos + 4 <= payload.len() {
                    let s = PacketStream::from_bytes(payload[pos + 4..].to_vec());
                    let op = u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]);
                    if op == sc::SCPlotEventPacket {
                        let Some((event_id, target, len)) = plot_event(s.as_bytes()) else {
                            break;
                        };
                        out.push(Record {
                            t_ms: now,
                            what: "plot",
                            event_id: Some(event_id),
                            target,
                            mob_hp,
                        });
                        pos += 4 + len;
                    } else {
                        // Unknown length: stop this batch rather than mis-parse it.
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

async fn cast_and_record(
    server: &crate::GameServer,
    client: &mut TestClient,
    player: &PlayerRef,
    mobs: &[NpcRef],
    skill: SkillRef,
) -> Vec<Record> {
    let (result, _) = use_skill(
        server,
        &skill,
        &WorldObject::from(player),
        SkillCaster::Unit(player.obj_id),
        &mut SkillCastTarget::Unit(mobs[0].obj_id),
        Some(SkillObject::None),
        false,
    );
    assert_eq!(result, skill_result::SUCCESS, "the cast must be accepted");
    let started = now_ms();
    let mut records = Vec::new();
    while now_ms() - started < 1_500 {
        drain_records(client, started, &mobs[0], &mut records);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    drain_records(client, started, &mobs[0], &mut records);
    records
}

fn first_plot(records: &[Record], event_id: u32) -> Option<&Record> {
    records
        .iter()
        .find(|r| r.what == "plot" && r.event_id == Some(event_id))
}

/// The first leap event (118, 400 ms) must reach the client at least the authored 400 ms
/// before the damage event (121) is applied, and the clicked mob must still be alive then.
#[tokio::test]
async fn leap_event_reaches_the_client_before_the_damage_kills_the_mob() {
    let (server, mut client, player, mobs) = fixture(data(), 1);
    let records = cast_and_record(&server, &mut client, &player, &mobs, skill(PLOT)).await;
    eprintln!("{records:#?}");

    let leap = first_plot(&records, 118).expect("leap plot event 118");
    let damage = first_plot(&records, 121).expect("damage plot event 121");
    assert_eq!(
        leap.mob_hp, 100,
        "the leap event must be sent while the target is still alive"
    );
    assert!(
        damage.t_ms - leap.t_ms >= 350,
        "damage at {} ms is only {} ms after the leap event; plot_next_events row 129 says 400 ms",
        damage.t_ms,
        damage.t_ms - leap.t_ms
    );
    // Nothing damaged or killed the mob before the leap event went out.
    assert!(
        !records
            .iter()
            .any(|r| r.what != "plot" && r.t_ms < leap.t_ms),
        "damage/death before the leap event: {records:#?}"
    );
    // The leap target is the clicked mob.
    assert_eq!(leap.target, Some(mobs[0].obj_id));
    // The mob does die from event 121, after the leap.
    assert!(
        records
            .iter()
            .any(|r| r.what == "death" && r.t_ms >= damage.t_ms),
        "the damage must kill the 100 HP mob: {records:#?}"
    );
}

/// One hostile mob: one leap (118), no second leap (119), because the `RandomUnit` bounce
/// excludes the clicked target and has no other hostile in 10 m.
#[tokio::test]
async fn a_lone_mob_gets_no_bounce_leap() {
    let (server, mut client, player, mobs) = fixture(data(), 1);
    let records = cast_and_record(&server, &mut client, &player, &mobs, skill(PLOT)).await;
    assert!(first_plot(&records, 118).is_some(), "{records:#?}");
    assert!(
        first_plot(&records, 119).is_none(),
        "no bounce target, but event 119 went out: {records:#?}"
    );
}

/// Two hostile mobs: event 118 leaps onto the clicked one, event 119 bounces onto the other.
#[tokio::test]
async fn a_second_mob_gets_the_bounce_leap() {
    let (server, mut client, player, mobs) = fixture(data(), 2);
    let records = cast_and_record(&server, &mut client, &player, &mobs, skill(PLOT)).await;
    let leap = first_plot(&records, 118).expect("first leap");
    assert_eq!(leap.target, Some(mobs[0].obj_id));
    let bounce = first_plot(&records, 119).expect("second leap onto the other mob");
    assert_eq!(
        bounce.target,
        Some(mobs[1].obj_id),
        "the bounce must land on the other hostile mob, not the clicked one: {records:#?}"
    );
}

/// The shipped `compact.sqlite3` rows drive the same order: the real plot 17, the real
/// `plot_next_events` delays (118 -> 121: 400 ms, 119 -> 121: 200 ms) and the real skill 13315.
/// `AAEMU_COMPACT_DB=rust/crates/aaemu-game/app/Data/compact.sqlite3 cargo test -p aaemu-game
/// real_compact_db_tiger_strike -- --ignored`
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
async fn real_compact_db_tiger_strike_leaps_before_the_damage() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let gd = real_game_data(&conn);
    let template = gd
        .systems
        .skills
        .skill(SKILL)
        .expect("the real catalog carries 13315")
        .clone();
    assert_eq!(template.plot_id, Some(PLOT));
    let (server, mut client, player, mobs) = fixture(gd, 1);
    mobs[0].data().unit.hp = 1;
    mobs[0].data().unit.max_hp = 1;
    {
        // The real row scales its mana with `mana_level_md`; the test character only needs
        // enough mana for the cast to be accepted.
        let mut c = player.data();
        c.unit.mp = 1_000_000;
        c.unit.max_mp = 1_000_000;
    }
    let real_skill = SkillRun::new(template, 1);
    let records = cast_and_record(&server, &mut client, &player, &mobs, real_skill).await;
    eprintln!("{records:#?}");

    let leap = first_plot(&records, 118).expect("leap plot event 118");
    let damage = first_plot(&records, 121).expect("damage plot event 121");
    assert_eq!(
        leap.mob_hp, 1,
        "the leap event must be sent while the target is still alive"
    );
    assert!(
        damage.t_ms - leap.t_ms >= 350,
        "real plot 17: damage at {} ms is only {} ms after the leap",
        damage.t_ms,
        damage.t_ms - leap.t_ms
    );
    assert!(!records
        .iter()
        .any(|r| r.what != "plot" && r.t_ms < leap.t_ms));
}

/// A stalled runtime (the flush of the leap event is late) must not shorten the authored delay:
/// the damage event still follows the leap by the full 400 ms of `plot_next_events` row 91002.
#[tokio::test]
async fn a_stalled_flush_does_not_shorten_the_leap_to_damage_delay() {
    let (server, mut client, player, mobs) = fixture(stalled_plot_data(), 1);
    super::stall_next_flush_with(STALLED_PLOT, STALLED_LEAP_EVENT, 250);
    let records = cast_and_record(&server, &mut client, &player, &mobs, skill(STALLED_PLOT)).await;
    eprintln!("{records:#?}");

    let leap = first_plot(&records, STALLED_LEAP_EVENT).expect("leap event");
    let damage = first_plot(&records, STALLED_DAMAGE_EVENT).expect("damage event");
    assert!(
        damage.t_ms - leap.t_ms >= 350,
        "damage at {} ms is only {} ms after the stalled leap flush; the authored delay is 400 ms",
        damage.t_ms,
        damage.t_ms - leap.t_ms
    );
}
