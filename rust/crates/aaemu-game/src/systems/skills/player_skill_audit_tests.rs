//! Audit of every player class skill (`skills.ability_id` 1..=10, 170 rows of the 1.2 data):
//! each one is cast through the client path (`CSStartSkillPacket`) against a hostile NPC (or
//! self / a friendly player / a position, as its target type asks) with the prerequisites a real
//! character would have (ability active, skill learned, weapons per `unit_reqs`, mana). The
//! outcome is classified, a table is printed, and the test fails on a skill that fires but
//! deals no damage although its data says it should, and on a refusal nobody explained.
//!
//! Gated by `AAEMU_COMPACT_DB` (path to `compact.sqlite3`):
//! `AAEMU_COMPACT_DB=... cargo test -p aaemu-game player_skill_audit -- --ignored --nocapture`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::PacketStream;

use crate::models::char_data::Skill;
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldObject};

use super::data::{EffectKind, EffectTemplate, SkillTemplate};
use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};

/// Real 1.2 items: a one-handed sword (mainhand, slot 15), a bow (ranged, 17), a shield
/// (offhand, 16) and a string instrument (musical, 18).
const SWORD: u32 = 5639;
const BOW: u32 = 14224;
const SHIELD: u32 = 18420;
const INSTRUMENT: u32 = 20483;
/// A hostile wild animal (the lizard of the quest 44 tests).
const HOSTILE_NPC: u32 = 668;
/// No hit of a level 55 character with starter gear should come near this.
const ABSURD_DAMAGE: i32 = 5_000_000;

/// Class names of `skills.ability_id` 1..=10 (the `Ability` enum of the C# code base).
const CLASSES: [&str; 11] = [
    "General",
    "Battlerage",
    "Witchcraft",
    "Defense",
    "Auramancy",
    "Occultism",
    "Archery",
    "Sorcery",
    "Shadowplay",
    "Songcraft",
    "Vitalism",
];

/// Skills whose failure is understood and accepted, with the reason. Keep this list explicit:
/// every entry is a skill the audit cannot or should not drive end to end.
const KNOWN: &[(u32, &str)] = &[
    (
        12786,
        "login-stage showcase row: Source selection with target_area_radius 0, so the data          only ever hits the caster itself (the C# did the same)",
    ),
    (
        14774,
        "Flame Barrier: its damage comes from the doodad its plot spawns (InteractionEffect          1304), which the audit does not simulate",
    ),
];

/// Skills the audit cannot aim at: no plain unit target exists for them.
const SETUP_LIMITS: &[(u32, &str)] = &[
    (
        10546,
        "Resurrection-type skill: the target must be a dead player",
    ),
    (
        18311,
        "doodad target (target type 8): needs a placed doodad",
    ),
];

/// Skills whose damage is conditional on a buff the audit does not provide.
const COMBO_SKILLS: &[(u32, &str)] = &[(
    10154,
    "Bubble Trap: the DamageEffect only applies to a target carrying buff tag 121 (bubbled)",
)];

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    FiredDamaged,
    FiredDotDamaged,
    FiredNoDamageExpected,
    FiredButNoDamage,
    Refused(u8),
    NeedsBuffCombo,
    NeedsSetup(String),
}

impl Outcome {
    fn label(&self) -> String {
        match self {
            Outcome::FiredDamaged => "FIRED_DAMAGED".into(),
            Outcome::FiredDotDamaged => "FIRED_DOT_DAMAGED".into(),
            Outcome::FiredNoDamageExpected => "FIRED_NO_DAMAGE_EXPECTED".into(),
            Outcome::FiredButNoDamage => "FIRED_BUT_NO_DAMAGE".into(),
            Outcome::Refused(c) => format!("REFUSED({c})"),
            Outcome::NeedsBuffCombo => "NEEDS_BUFF/COMBO".into(),
            Outcome::NeedsSetup(r) => format!("NEEDS_SETUP({r})"),
        }
    }
}

struct Row {
    id: u32,
    name: String,
    ability: u8,
    outcome: Outcome,
    detail: String,
}

/// What the data says a skill can deal: `(direct or plot DamageEffect, DamageEffect on a buff
/// tick, any effect gated on a buff tag)`.
fn damage_sources(server: &crate::GameServer, t: &SkillTemplate) -> (bool, bool, bool) {
    let data = &server.game_data.systems.skills;
    let mut effects: Vec<(EffectKind, u32)> = t.effects.iter().filter_map(|e| e.template).collect();
    let mut gated = t.effects.iter().any(|e| {
        e.source_buff_tag_id
            + e.target_buff_tag_id
            + e.source_no_buff_tag_id
            + e.target_no_buff_tag_id
            > 0
    });
    if let Some(plot) = t.plot_id {
        for ev in data.plots.events.values().filter(|e| e.plot_id == plot) {
            gated |= !ev.conditions.is_empty();
            for pe in &ev.effects {
                if let Some(kind) = EffectKind::from_name(&pe.actual_type) {
                    effects.push((kind, pe.actual_id));
                }
            }
        }
    }
    let (mut direct, mut dot) = (false, false);
    for (kind, id) in effects {
        match (kind, data.effect(kind, id)) {
            (EffectKind::DamageEffect, _) => direct = true,
            (_, Some(EffectTemplate::Buff(b))) => {
                let ticks = b
                    .buff_id
                    .and_then(|id| data.buff(id))
                    .map(|buff| buff.tick_effects.clone())
                    .unwrap_or_default();
                dot |= ticks.iter().any(|te| {
                    matches!(
                        data.effect_by_type_id(te.effect_id),
                        Some((EffectKind::DamageEffect, _))
                    )
                });
            }
            _ => {}
        }
    }
    (direct, dot, gated)
}

/// Result byte of a `SCSkillStartedPacket` for `skill` (`None` = no error byte).
fn started_result(body: &[u8], skill: u32) -> Option<u8> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    if s.read_u32().ok()? != skill {
        return None;
    }
    s.read_u16().ok()?;
    let kind = s.read_u8().ok()?;
    SkillCaster::read(kind, &mut s).ok()?;
    let kind = s.read_u8().ok()?;
    SkillCastTarget::read(kind, &mut s).ok()?;
    let flag = s.read_u8().ok()?;
    if flag > 0 {
        SkillObject::read(flag, &mut s).ok()?;
    }
    s.read_u16().ok()?;
    s.read_u16().ok()?;
    s.read_u8().ok()?;
    let flags = s.read_u8().ok()?;
    (flags & 1 != 0).then(|| s.read_u8().ok()).flatten()
}

fn anti_miss(attribute: UnitAttribute) -> Bonus {
    let template = BonusTemplate {
        attribute,
        modifier_type: UnitModifierType::Value,
        value: 1_000_000,
        linear_level_bonus: 0,
    };
    Bonus {
        template,
        value: 1_000_000,
    }
}

fn equip(server: &crate::GameServer, player: &PlayerRef, template: u32, slot: i32) {
    let item = server
        .items
        .create(
            template,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("item template");
    let equipment = Inv::of(player).equipment();
    equipment
        .lock()
        .unwrap()
        .add_new_item(item, slot, &server.game_data.items)
        .unwrap_or_else(|e| panic!("equip {template} in slot {slot}: {e:?}"));
}

/// A level 55 character with the skill's ability active and the skill learned.
fn make_player(
    server: &Arc<crate::GameServer>,
    n: u32,
    pos: (f32, f32),
    t: &SkillTemplate,
    learn: bool,
) -> (TestClient, PlayerRef) {
    let mut client = TestClient::new(server, n, 5000 + n);
    let mut c = test_character(n, &format!("Audit{n}"), pos.0, pos.1);
    c.unit.level = 55;
    c.unit.hp = 2_000_000;
    c.unit.max_hp = 2_000_000;
    c.unit.mp = 2_000_000;
    c.unit.max_mp = 2_000_000;
    c.skill_char.ignore_skill_cooldowns = true;
    c.ability1 = t.ability_id;
    // Ability level 55 (plot conditions of kind 17 read it): `test_experience` has
    // 1000 exp per level.
    for a in c
        .abilities
        .abilities
        .iter_mut()
        .filter(|a| a.id == t.ability_id)
    {
        a.order = 0;
        a.exp = 55_000;
    }
    if learn {
        c.skills.skills.push(Skill { id: t.id, level: 1 });
    }
    for (i, attribute) in [
        UnitAttribute::MeleeAntiMiss,
        UnitAttribute::RangedAntiMiss,
        UnitAttribute::SpellAntiMiss,
    ]
    .into_iter()
    .enumerate()
    {
        c.unit.bonuses.add_bonus(i as u32 + 1, anti_miss(attribute));
    }
    let player = spawn_player(server, &client, c, 0x1000 + n);
    client.conn.active_char = Some(player.clone());
    server.items.add_container(Inv::of(&player).bag());
    client.drain();
    (client, player)
}

fn spawn_npc(server: &crate::GameServer, obj_id: u32, pos: (f32, f32)) -> Option<NpcRef> {
    let npc = crate::systems::npcs::npc::manager::create(server, obj_id, HOSTILE_NPC)?;
    {
        let mut n = npc.data();
        n.unit.transform.set_xyz(pos.0, pos.1, 100.0);
        // The template's own hit points: a stat refresh recomputes `max_hp` from them.
        n.unit.hp = n.unit.max_hp;
    }
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    Some(npc)
}

/// One skill's scene: a caster with the skill's prerequisites, a hostile NPC in front of it, a
/// friendly player beside it.
struct Scene {
    t: Arc<SkillTemplate>,
    client: TestClient,
    player: PlayerRef,
    npc: NpcRef,
    /// Extra NPCs on a ring around the target for skills that scatter their hits randomly.
    ring: Vec<NpcRef>,
    _friend_client: TestClient,
    target: SkillCastTarget,
    dist: f32,
    hp_before: i32,
    direct: bool,
    dot: bool,
    gated: bool,
    skill_reqs: usize,
    result: u8,
    fired: bool,
    accepted: bool,
    attempts: u32,
    frames: Vec<(u16, Vec<u8>)>,
    early: Option<Outcome>,
}

/// Builds the scene of skill number `index`. Every skill gets its own 80 m cell of the test
/// world so scenes never touch and can run at the same time.
fn setup(server: &Arc<crate::GameServer>, index: u32, t: &Arc<SkillTemplate>) -> Scene {
    let n = index + 1;
    let base = (
        120.0 + (index % 12) as f32 * 80.0,
        120.0 + (index / 12) as f32 * 80.0,
    );
    let skill_reqs = server
        .game_data
        .systems
        .skills
        .buff_skill_requirements
        .iter()
        .filter(|r| r.skill_ids.contains(&t.id))
        .count();
    let (client, player) = make_player(server, n, base, t, t.need_learn);
    for (template, slot) in [(SWORD, 15), (SHIELD, 16), (BOW, 17), (INSTRUMENT, 18)] {
        equip(server, &player, template, slot);
    }
    crate::systems::skills::stats::refresh_cached_stats(server, &WorldObject::from(&player));
    // Target distance: inside [min_range, max_range] (both in metres in the template).
    let min = t.min_range.max(0) as f32;
    let max = t.max_range.max(0) as f32;
    let mut dist = if min > 0.0 { min + 1.0 } else { 3.0 };
    if max > 0.0 && dist > max - 0.5 {
        dist = (max - 0.5).max(1.0);
    }
    let npc_pos = (base.0, base.1 + dist);
    let npc = spawn_npc(server, 0x3000 + n, npc_pos).expect("npc 668 template");
    // A plot that picks a random point on a ring around its target (Missile Rain) only hits
    // a unit that happens to stand there: surround the target with a ring of NPCs.
    let ring_radius = t
        .plot_id
        .map(|plot| {
            server
                .game_data
                .systems
                .skills
                .plots
                .events
                .values()
                .filter(|e| e.plot_id == plot && e.target_update_method_id == 7)
                .map(|e| e.target_update_method_params[2])
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0) as f32
        / 1000.0;
    let ring: Vec<NpcRef> = if ring_radius > 0.0 {
        (0..24u32)
            .filter_map(|k| {
                let a = k as f32 / 24.0 * std::f32::consts::TAU;
                spawn_npc(
                    server,
                    0x6000 + n * 32 + k,
                    (
                        npc_pos.0 + ring_radius * a.cos(),
                        npc_pos.1 + ring_radius * a.sin(),
                    ),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    let (friend_client, friend) = make_player(server, 400 + n, (base.0 + 2.0, base.1), t, false);
    let mut early = None;
    let target = match t.target_type {
        0 => SkillCastTarget::Unit(player.obj_id),
        1..=3 => SkillCastTarget::Unit(friend.obj_id),
        4 | 5 => SkillCastTarget::Unit(npc.obj_id),
        6 => SkillCastTarget::Position {
            obj_id: 0,
            x: npc_pos.0,
            y: npc_pos.1,
            z: 100.0,
            rot: 0.0,
            obj_id1: 0,
            obj_id2: 0,
        },
        other => {
            early = Some(Outcome::NeedsSetup(format!("target type {other}")));
            SkillCastTarget::Unit(player.obj_id)
        }
    };
    if t.target_dead {
        early = Some(Outcome::NeedsSetup("dead target".into()));
    }
    let (direct, dot, gated) = damage_sources(server, t);
    let hp_before = npc.data().unit.hp + ring.iter().map(|n| n.data().unit.hp).sum::<i32>();
    Scene {
        t: t.clone(),
        client,
        player,
        npc,
        ring,
        _friend_client: friend_client,
        target,
        dist,
        hp_before,
        direct,
        dot,
        gated,
        skill_reqs,
        result: 0,
        fired: false,
        accepted: false,
        attempts: 0,
        frames: Vec::new(),
        early,
    }
}

/// Casts the scene's skill through the client path (or, for a row no client may cast, through
/// the server's own cast API, as the plot or trigger that owns it does).
fn cast_scene(server: &Arc<crate::GameServer>, s: &mut Scene) {
    s.attempts += 1;
    {
        let mut p = s.player.data();
        p.unit.skill_last_used_ms = 0;
        p.unit.global_cooldown_ms = 0;
        p.unit.mp = 2_000_000;
    }
    let t = s.t.clone();
    start_skill(
        server,
        &mut s.client.conn,
        CSStartSkillPacket {
            skill_id: t.id,
            caster: SkillCaster::Unit(s.player.obj_id),
            target: s.target,
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    let mut frames = s.client.drain();
    let no_perm = frames.iter().any(|(op, body)| {
        *op == sc::SCSkillStartedPacket && started_result(body, t.id) == Some(skill_result::NO_PERM)
    });
    if no_perm && !t.need_learn {
        let me = WorldObject::from(&s.player);
        let skill = super::skill::new_skill_for(server, t.clone(), Some(&me));
        let mut target = s.target;
        let (r, _) = super::skill::use_skill(
            server,
            &skill,
            &me,
            SkillCaster::Unit(s.player.obj_id),
            &mut target,
            Some(SkillObject::None),
            false,
        );
        frames.retain(|(op, _)| *op != sc::SCSkillStartedPacket);
        if r != 0 {
            s.result = r;
        }
    }
    s.accepted |= s.player.data().unit.skill_last_used_ms > 0;
    s.frames.extend(frames);
}

/// Reads what the cast sent and did since the last call.
fn collect(s: &mut Scene, verbose: bool) {
    s.frames.extend(s.client.drain());
    if verbose {
        let ops: Vec<String> = s
            .frames
            .iter()
            .map(|(op, b)| format!("{op:#x}/{}", b.len()))
            .collect();
        eprintln!(
            "  skill {} attempt {}: result {} packets {ops:?}",
            s.t.id, s.attempts, s.result
        );
    }
    for (op, body) in std::mem::take(&mut s.frames) {
        if op == sc::SCSkillStartedPacket {
            if let Some(r) = started_result(&body, s.t.id) {
                s.result = r;
            }
        } else if op == sc::SCSkillFiredPacket {
            s.fired = true;
        }
    }
}

fn damage_dealt(s: &Scene) -> i32 {
    s.hp_before - s.npc.data().unit.hp - s.ring.iter().map(|n| n.data().unit.hp).sum::<i32>()
}

fn classify(s: &Scene) -> (Outcome, String) {
    let damage = damage_dealt(s);
    let t = &s.t;
    let detail = format!(
        "range {}-{} dist {} sel {} tt {} rel {} dmg {damage} tries {} fired {} accepted {} expect direct={} dot={} gated={} reqs={}",
        t.min_range,
        t.max_range,
        s.dist,
        t.target_selection,
        t.target_type,
        t.target_relation,
        s.attempts,
        s.fired,
        s.accepted,
        s.direct,
        s.dot,
        s.gated,
        s.skill_reqs,
    );
    if let Some(early) = &s.early {
        return (early.clone(), detail);
    }
    if s.result != 0 {
        return (Outcome::Refused(s.result), detail);
    }
    // Plausibility: a level 55 character with starter gear cannot out-damage the NPC's whole
    // pool by orders of magnitude (the NPC dies at most once).
    if damage > ABSURD_DAMAGE {
        return (
            Outcome::NeedsSetup(format!("absurd damage {damage}")),
            detail,
        );
    }
    if damage > 0 {
        let outcome = if s.direct {
            Outcome::FiredDamaged
        } else {
            Outcome::FiredDotDamaged
        };
        return (outcome, detail);
    }
    if !s.fired && !s.accepted {
        return (Outcome::NeedsSetup("never fired".into()), detail);
    }
    if !s.direct && !s.dot {
        return (Outcome::FiredNoDamageExpected, detail);
    }
    if s.gated || s.skill_reqs > 0 {
        return (Outcome::NeedsBuffCombo, detail);
    }
    (Outcome::FiredButNoDamage, detail)
}

/// Plot events are scheduled against the wall clock (`now_ms`), so the audit runs in real time:
/// every scene casts at once and the test waits once per round.
const ROUND: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn player_skill_audit() {
    let Some(data) = super::lizard_stun_flow_tests::real_data() else {
        return;
    };
    // Skill names for the table.
    let conn = rusqlite::Connection::open_with_flags(
        std::env::var("AAEMU_COMPACT_DB").unwrap(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("compact db");
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT idx, en_us FROM localized_texts WHERE tbl_name='skills' AND tbl_column_name='name'",
        )
        .unwrap();
    for row in stmt
        .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
    {
        let (id, name) = row.unwrap();
        names.insert(id, name);
    }
    drop(stmt);
    drop(conn);

    // `AUDIT_LOG=debug` prints what the server logs while the skills run.
    let _log = std::env::var("AUDIT_LOG").ok().map(|level| {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(
                    level
                        .parse::<tracing::Level>()
                        .unwrap_or(tracing::Level::WARN),
                )
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .finish(),
        )
    });
    let verbose = std::env::var("AUDIT_VERBOSE").is_ok();
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);

    let mut skills: Vec<Arc<SkillTemplate>> = server
        .game_data
        .systems
        .skills
        .skills
        .values()
        .filter(|t| (1..=10).contains(&t.ability_id))
        .cloned()
        .collect();
    skills.sort_by_key(|t| (t.ability_id, t.ability_level, t.id));
    assert_eq!(skills.len(), 170, "player class skill count");
    // `AUDIT_SKILLS=10670,11939` narrows the run while debugging.
    let only: Vec<u32> = std::env::var("AUDIT_SKILLS")
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_default();

    let mut scenes: Vec<Scene> = skills
        .iter()
        .enumerate()
        .filter(|(_, t)| only.is_empty() || only.contains(&t.id))
        .map(|(i, t)| setup(&server, i as u32, t))
        .collect();

    // Round 1: everything casts. Round 2: a skill that dealt nothing although its data says it
    // should casts once more (a dodged or blocked hit is not a bug).
    for round in 0..2 {
        let mut cast_any = false;
        for s in scenes.iter_mut().filter(|s| s.early.is_none()) {
            let again = round == 0
                || (s.result == 0 && (s.direct || s.dot) && damage_dealt(s) == 0 && s.accepted);
            if again {
                cast_scene(&server, s);
                cast_any = true;
            }
        }
        if !cast_any {
            break;
        }
        tokio::time::sleep(ROUND).await;
        for s in scenes.iter_mut().filter(|s| s.early.is_none()) {
            collect(s, verbose);
        }
    }

    let mut rows = Vec::new();
    for s in &scenes {
        let (outcome, detail) = classify(s);
        rows.push(Row {
            id: s.t.id,
            name: names.get(&s.t.id).cloned().unwrap_or_default(),
            ability: s.t.ability_id,
            outcome,
            detail,
        });
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    eprintln!("id\tname\tclass\toutcome\tdetail");
    for r in &rows {
        let key = match &r.outcome {
            Outcome::Refused(_) => "REFUSED".to_string(),
            Outcome::NeedsSetup(_) => "NEEDS_SETUP".to_string(),
            o => o.label(),
        };
        *counts.entry(key).or_default() += 1;
        eprintln!(
            "{}\t{}\t{}\t{}\t{}",
            r.id,
            r.name,
            CLASSES[r.ability as usize],
            r.outcome.label(),
            r.detail
        );
    }
    eprintln!("summary: {counts:?}");

    let bad: Vec<String> = rows
        .iter()
        .filter(|r| {
            matches!(r.outcome, Outcome::FiredButNoDamage | Outcome::Refused(_))
                && !KNOWN.iter().any(|(id, _)| *id == r.id)
        })
        .map(|r| format!("{} {} {}", r.id, r.name, r.outcome.label()))
        .collect();
    assert!(bad.is_empty(), "unexplained audit failures: {bad:#?}");
    // The rows the audit cannot drive, by id: a new one is a regression of the harness or the
    // data and must be looked at, not absorbed.
    let unexplained: Vec<String> = rows
        .iter()
        .filter(|r| match &r.outcome {
            Outcome::NeedsSetup(_) => !SETUP_LIMITS.iter().any(|(id, _)| *id == r.id),
            Outcome::NeedsBuffCombo => !COMBO_SKILLS.iter().any(|(id, _)| *id == r.id),
            _ => false,
        })
        .map(|r| format!("{} {} {}", r.id, r.name, r.outcome.label()))
        .collect();
    assert!(
        unexplained.is_empty() || !only.is_empty(),
        "unexplained audit setup gaps: {unexplained:#?}"
    );
}
