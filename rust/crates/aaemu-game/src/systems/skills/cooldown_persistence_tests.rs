//! Cooldowns must reach the 1.2 client and survive a relog.
//!
//! Native evidence: `397b64d0` -> `397b2350` reads `i32 skillCount` followed by
//! `(id, total, remaining)` u32 triples, then `i32 tagCount` and the same triples; `OnCooldowns`
//! (`391d43c0`) -> `39899d40` appends them to the local character's cooldown manager, which is
//! what the action bar asks through `GetActionCooldown` (`3943f310` -> `3989a3e0`/`3989a290`).
//! The client starts the cooldown itself for an ordinary skill (`3939b010` when
//! `toggle_buff_id == 0 && timing_id != 0` and the fired packet's two flag bits are 0), for a
//! toggle skill when its buff is removed (`39389b70`), and applies the `Cooldown` special effect
//! itself. The server therefore sends `SCCooldowns` per cast only for a cast that never sends
//! `SCSkillFired` (`plot_only` with a plot) or a `timing_id == 0` skill. The end time is
//! persisted as an absolute Unix-ms stamp and the select sync restores it after a relog.

use std::sync::Arc;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::GameData;
use crate::db_character::{
    apply_cooldown_rows, cooldown_rows, COOLDOWN_TYPE_SKILL, COOLDOWN_TYPE_TAG,
};
use crate::models::unit::UnitCooldowns;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::skills;
use crate::systems::skills::effects::{EffectArgs, EffectSource};
use crate::systems::skills::geometry::Target;
use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{open_client_skills, test_character, TestClient};
use crate::world::session::send_running_cooldowns;
use crate::world::WorldObject;
use crate::world::{now_ms, NpcRef, PlayerRef};

use skills::data::SkillTemplate;
use skills::handlers::{start_skill, CSStartSkillPacket};
use skills::packets::SCCooldownsPacket;
use skills::types::skill_result;

/// The 1.2 `Recall` (`귀환`, `special_effects` 3132 -> `return_`) and `Escape` (`탈출`) skills.
const RECALL: u32 = 14849;
const ESCAPE: u32 = 20297;
/// A long-cooldown consumable and a member of a shared cooldown group (144).
const POTION: u32 = 11717;
const TAGGED_POTION: u32 = 11720;
const TAG: u32 = 144;
/// An ordinary untagged skill: `timing_id` 1, no toggle, started by the client itself.
const ORDINARY: u32 = 21001;
/// A toggle skill: the client starts its cooldown in `OnBuffRemoved` (`39389b70`).
const TOGGLE: u32 = 21002;
/// A `plot_only` skill with a plot: it never sends `SCSkillFired`, so the server must send the
/// timer.
const PLOT_ONLY: u32 = 21004;
/// The rare `timing_id == 0` skill: the client does not start its timer on `SCSkillFired`.
const TIMING_ZERO: u32 = 21005;
/// A skill in the battlefield reset group (`playerSkillsTag` 378), with its own shared
/// cooldown tag.
const BATTLEFIELD_SKILL: u32 = 21006;
const BATTLEFIELD_TAG: u32 = 145;

fn data() -> GameData {
    let mut data = GameData::empty();
    for (id, tag, duration, timing_id, toggle_buff_id, plot_only, plot_id) in [
        (RECALL, 0, 2_000, 1, 0, false, None),
        (ESCAPE, 0, 1_800_000, 1, 0, false, None),
        (POTION, 0, 43_200_000, 1, 0, false, None),
        (TAGGED_POTION, TAG as i32, 60_000, 1, 0, false, None),
        (ORDINARY, 0, 10_000, 1, 0, false, None),
        (TOGGLE, 0, 10_000, 1, 1458, false, None),
        (PLOT_ONLY, 0, 10_000, 1, 0, true, Some(1)),
        (TIMING_ZERO, 0, 500, 0, 0, false, None),
        (
            BATTLEFIELD_SKILL,
            BATTLEFIELD_TAG as i32,
            10_000,
            1,
            0,
            false,
            None,
        ),
    ] {
        data.systems.skills.add_skill(SkillTemplate {
            cooldown_tag_id: tag,
            cooldown_time: duration,
            timing_id,
            toggle_buff_id,
            plot_only,
            plot_id,
            ..skills::tests::skill_template(id)
        });
    }
    data.systems
        .skills
        .tagged_skills
        .insert(378, vec![BATTLEFIELD_SKILL]);
    data
}

fn fixture(mut data: GameData) -> (Arc<GameServer>, TestClient, PlayerRef, NpcRef) {
    open_client_skills(&mut data);
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    client.conn.active_char = Some(player.clone());
    client.drain();
    (server, client, player, npc)
}

fn cast(server: &GameServer, client: &mut TestClient, id: u32, target: u32) {
    let player = client.conn.active_char.clone().unwrap();
    {
        let mut p = player.data();
        // Isolate skill cooldown admission from the per-press / GCD throttle.
        p.unit.skill_last_used_ms = 0;
        p.unit.global_cooldown_ms = 0;
    }
    let mut request = Vec::from(id.to_le_bytes());
    request.push(0);
    request.extend_from_slice(&player.obj_id.to_le_bytes()[..3]);
    request.push(0);
    request.extend_from_slice(&target.to_le_bytes()[..3]);
    request.push(0);
    let packet = CSStartSkillPacket::read(&mut PacketStream::from_bytes(request)).unwrap();
    start_skill(server, &mut client.conn, packet);
}

type UiCooldownEntry = (u32, u32, u32);

/// Minimal native `397b2350` reader (`i32` count + `(id, total, remaining)` triples).
#[derive(Debug, Default)]
struct UiCooldowns {
    skills: Vec<UiCooldownEntry>,
    tags: Vec<UiCooldownEntry>,
}

impl UiCooldowns {
    fn read(bytes: &[u8]) -> Self {
        let mut body = PacketStream::from_bytes(bytes.to_vec());
        let mut read_list = || {
            let count = body.read_i32().unwrap();
            assert!((0..=150).contains(&count));
            (0..count)
                .map(|_| {
                    (
                        body.read_u32().unwrap(),
                        body.read_u32().unwrap(),
                        body.read_u32().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let skills = read_list();
        let tags = read_list();
        assert_eq!(bytes.len(), 8 + (skills.len() + tags.len()) * 12);
        Self { skills, tags }
    }
}

fn cooldown_packet(frames: &[(u16, Vec<u8>)]) -> Option<UiCooldowns> {
    let packets: Vec<_> = frames
        .iter()
        .filter(|(op, _)| *op == sc::SCCooldownsPacket)
        .collect();
    assert!(packets.len() <= 1, "one UI application per accepted cast");
    packets.first().map(|(_, body)| UiCooldowns::read(body))
}

fn assert_denied(frames: &[(u16, Vec<u8>)], result: u8) {
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCCooldownsPacket));
    let errors: Vec<_> = frames
        .iter()
        .filter(|(op, _)| *op == sc::SCSkillStartedPacket)
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(&errors[0].1[errors[0].1.len() - 2..], &[1, result]);
}

/// A relog: capture the persisted rows from the live character, start a fresh store and apply
/// them, exactly like `save_character` -> `load_character_details` does.
fn relog(player: &PlayerRef, now_ms: i64) -> UnitCooldowns {
    let rows = cooldown_rows(&player.data(), now_ms);
    let mut fresh = test_character(1, "Caster", 100.0, 100.0);
    apply_cooldown_rows(&mut fresh, &rows, now_ms);
    fresh.unit.cooldowns
}

/// The client starts the cooldown itself for an ordinary skill (`3939b010`: `toggle_buff_id`
/// 0, `timing_id` 1, fired flag bits 0) and `39899f90` writes either the skill or the tag entry
/// from its own template. No server entry may be appended: `39899d40` does not de-duplicate and
/// drops additions once the list holds 150.
#[tokio::test]
async fn client_started_casts_send_no_cooldown_packet() {
    let (server, mut client, player, npc) = fixture(data());
    for _ in 0..200 {
        // Let the next press pass while keeping the started coordination: admission and
        // persistence still see it, only the client update is suppressed.
        player.data().unit.cooldowns.remove_cooldown(ORDINARY);
        cast(&server, &mut client, ORDINARY, npc.obj_id);
        let frames = client.drain();
        assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
        assert!(
            !frames.iter().any(|(op, _)| *op == sc::SCCooldownsPacket),
            "the client starts an ordinary skill's timer itself"
        );
    }
    assert!(player
        .data()
        .unit
        .cooldowns
        .cooldowns
        .contains_key(&ORDINARY));

    // Recall, the tagged consumable and a toggle skill are client-started too (toggle on
    // buff removal, `OnBuffRemoved` 39389b70 -> 39368f90).
    for id in [RECALL, POTION, TAGGED_POTION, TOGGLE] {
        let mut p = player.data();
        p.unit.cooldowns.remove_cooldown(id);
        p.unit.cooldowns.tag_cooldowns.remove(&TAG);
        drop(p);
        cast(&server, &mut client, id, npc.obj_id);
        let frames = client.drain();
        assert!(
            frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket),
            "skill {id}"
        );
        assert!(
            !frames.iter().any(|(op, _)| *op == sc::SCCooldownsPacket),
            "skill {id}"
        );
    }
}

/// A `plot_only` cast with a plot never sends `SCSkillFired` (the `use_skill` plot branch
/// returns before `cast`), so the client never starts the timer and the server must send it.
#[tokio::test]
async fn plot_only_cast_sends_the_server_cooldown_packet() {
    let (server, mut client, player, npc) = fixture(data());
    cast(&server, &mut client, PLOT_ONLY, npc.obj_id);
    let frames = client.drain();
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    let ui = cooldown_packet(&frames).expect("plot-only cooldown packet");
    assert_eq!(ui.skills, vec![(PLOT_ONLY, 10_000, 10_000)]);
    assert!(ui.tags.is_empty());
    assert!(player
        .data()
        .unit
        .cooldowns
        .cooldowns
        .contains_key(&PLOT_ONLY));
}

/// The 8 `timing_id == 0` skills fail the client's `3939b010` test, so their timer comes from
/// the server.
#[tokio::test]
async fn timing_zero_skill_sends_the_server_cooldown_packet() {
    let (server, mut client, player, npc) = fixture(data());
    cast(&server, &mut client, TIMING_ZERO, npc.obj_id);
    let frames = client.drain();
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    let ui = cooldown_packet(&frames).expect("timing-zero cooldown packet");
    assert_eq!(ui.skills, vec![(TIMING_ZERO, 500, 500)]);
    assert!(player
        .data()
        .unit
        .cooldowns
        .cooldowns
        .contains_key(&TIMING_ZERO));
}

#[tokio::test]
async fn relog_restores_recall_and_login_sync_and_admission_see_it() {
    let (server, mut client, player, npc) = fixture(data());
    cast(&server, &mut client, RECALL, npc.obj_id);
    client.drain();

    let now = now_ms();
    let restored = relog(&player, now);
    assert_eq!(restored.cooldowns.len(), 1);
    assert!(restored.cooldowns[&RECALL] > now);

    // The select sync sends the restored timer (OnCooldowns appends it to the client manager).
    let mut snapshot = player.data().clone();
    snapshot.unit.cooldowns = restored.clone();
    send_running_cooldowns(&mut client.conn, &snapshot, now);
    let ui = cooldown_packet(&client.drain()).expect("login cooldown sync");
    assert_eq!(ui.skills.len(), 1);
    assert_eq!(ui.skills[0].0, RECALL);
    assert!(ui.skills[0].2 > 0);
    assert!(ui.tags.is_empty());

    // The live character now has the loaded store (as `load_character_details` leaves it): a
    // cast before the expiry is refused on the server, not merely hidden by the client.
    player.data().unit.cooldowns = restored;
    cast(&server, &mut client, RECALL, npc.obj_id);
    assert_denied(&client.drain(), skill_result::COOLDOWN_TIME);
}

#[tokio::test]
async fn consumable_and_tagged_cooldowns_survive_relog() {
    let (server, mut client, player, npc) = fixture(data());
    cast(&server, &mut client, POTION, npc.obj_id);
    let frames = client.drain();
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCCooldownsPacket));
    cast(&server, &mut client, TAGGED_POTION, npc.obj_id);
    let frames = client.drain();
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCCooldownsPacket));

    let now = now_ms();
    let rows = cooldown_rows(&player.data(), now);
    assert_eq!(rows.len(), 3, "two individual timers and the shared tag");
    assert!(rows
        .iter()
        .any(|r| r.kind == COOLDOWN_TYPE_SKILL && r.id == POTION));
    assert!(rows
        .iter()
        .any(|r| r.kind == COOLDOWN_TYPE_SKILL && r.id == TAGGED_POTION));
    assert!(rows
        .iter()
        .any(|r| r.kind == COOLDOWN_TYPE_TAG && r.id == TAG));

    let restored = relog(&player, now);
    assert!(restored.cooldowns[&POTION] > now);
    assert!(restored.cooldowns[&TAGGED_POTION] > now);
    assert!(restored.tag_cooldowns[&TAG] > now);

    let mut snapshot = player.data().clone();
    snapshot.unit.cooldowns = restored.clone();
    send_running_cooldowns(&mut client.conn, &snapshot, now);
    let ui = cooldown_packet(&client.drain()).expect("login cooldown sync");
    assert_eq!(ui.skills.len(), 2);
    assert_eq!(ui.skills[0].0, POTION, "entries are sorted by id");
    assert_eq!(ui.skills[1].0, TAGGED_POTION);
    assert_eq!(ui.tags.len(), 1);
    assert_eq!(ui.tags[0].0, TAG);

    player.data().unit.cooldowns = restored;
    cast(&server, &mut client, POTION, npc.obj_id);
    assert_denied(&client.drain(), skill_result::COOLDOWN_TIME);
    cast(&server, &mut client, TAGGED_POTION, npc.obj_id);
    assert_denied(&client.drain(), skill_result::COOLDOWN_TIME);
}

/// The 1.2 client applies the `Cooldown` special effect (40) itself (`OnPlotEvent` special
/// 0x28 -> `39368440`), so the server only updates its own store and sends no packet.
#[tokio::test]
async fn cooldown_effect_updates_the_server_store_without_a_packet() {
    let (server, mut client, player, _) = fixture(data());
    let caster = WorldObject::from(&player);
    let skill = skills::skill::new_skill_for(
        &server,
        server
            .game_data
            .systems
            .skills
            .skill(POTION)
            .unwrap()
            .clone(),
        Some(&caster),
    );
    let so = SkillObject::None;
    let args = EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(player.obj_id),
        target: Target::Obj(caster.clone()),
        target_caster: SkillCastTarget::Unit(player.obj_id),
        cast: CastAction::Skill {
            skill_id: 0,
            tl_id: 0,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &so,
    };
    client.drain();
    skills::effects::special::execute(&server, 40, &args, [5_000, 0, 0, 0]);
    assert!(!client.drain_ops().contains(&sc::SCCooldownsPacket));
    let remaining = player.data().unit.cooldowns.cooldowns[&POTION] - now_ms();
    assert!(
        (4_900..=5_000).contains(&remaining),
        "remaining = {remaining}"
    );

    // A non-positive effect duration starts nothing and sends nothing (never wraps to u32::MAX).
    let escape = skills::skill::new_skill_for(
        &server,
        server
            .game_data
            .systems
            .skills
            .skill(ESCAPE)
            .unwrap()
            .clone(),
        Some(&caster),
    );
    let mut args = args.clone();
    args.source = EffectSource::from_skill(escape);
    skills::effects::special::execute(&server, 40, &args, [-5_000, 0, 0, 0]);
    assert!(!player.data().unit.cooldowns.cooldowns.contains_key(&ESCAPE));
    assert!(!client.drain_ops().contains(&sc::SCCooldownsPacket));
}

/// The battlefield reset sends one `SCSkillCooldownReset` per tag-378 skill and, because the
/// port persists cooldowns, must also clear those server records (the C# reset could rely on
/// the client packet alone; a relog here would restore a cooldown the reset removed).
#[tokio::test]
async fn battlefield_reset_clears_the_server_records_and_sends_resets() {
    let (server, mut client, player, _) = fixture(data());
    let now = now_ms();
    {
        let mut p = player.data();
        p.unit
            .cooldowns
            .add_cooldown(BATTLEFIELD_SKILL, 10_000, now);
        p.unit
            .cooldowns
            .add_tag_cooldown(BATTLEFIELD_TAG, 10_000, now);
        // An unrelated shared group is not part of the reset.
        p.unit.cooldowns.add_tag_cooldown(TAG, 10_000, now);
    }
    client.drain();
    crate::systems::instances::instant_game::reset_all_skill_cooldowns(&server, &player, false);
    assert!(!player
        .data()
        .unit
        .cooldowns
        .cooldowns
        .contains_key(&BATTLEFIELD_SKILL));
    {
        let p = player.data();
        assert!(
            !p.unit
                .cooldowns
                .tag_cooldowns
                .contains_key(&BATTLEFIELD_TAG),
            "the reset skill's shared cooldown tag is cleared as well"
        );
        assert!(p.unit.cooldowns.tag_cooldowns.contains_key(&TAG));
    }
    assert!(
        !client.drain_frames().is_empty(),
        "the compressed reset packet is sent"
    );
}

#[test]
fn native_reader_bytes_for_a_tag_only_update() {
    let mut c = test_character(1, "Caster", 100.0, 100.0);
    c.unit.cooldowns.add_tag_cooldown(TAG, 10_000, 1_000);
    let packet = SCCooldownsPacket::from_cooldowns(&c.unit.cooldowns, 1_000);
    let mut body = PacketStream::new();
    Marshal::write(&packet, &mut body);
    // 397b2350: skillCount 0, tagCount 1, (144, 10000, 10000).
    assert_eq!(
        body.into_bytes(),
        [0, 0, 0, 0, 1, 0, 0, 0, 0x90, 0, 0, 0, 0x10, 0x27, 0, 0, 0x10, 0x27, 0, 0]
    );
}

/// The real 1.2 data behind the rule: Recall / Escape and the consumable are ordinary
/// `toggle_buff_id == 0, timing_id == 1` skills, which the client starts itself; a glider is a
/// toggle skill (`toggle_buff_id` 1458, cooldown on removal); skill 10201 is `plot_only` with a
/// plot, so it never sends `SCSkillFired`; skill 2 is one of the eight `timing_id == 0` skills.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_recall_escape_and_consumables_are_untagged() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    for (id, cooldown) in [(RECALL, 2_000), (21_586, 2_000), (ESCAPE, 1_800_000)] {
        let row: (i64, Option<i64>) = db
            .query_row(
                "SELECT cooldown_time, cooldown_tag_id FROM skills WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (cooldown, None), "skill {id}");
    }
    // Item 11123 (labor potion) uses skill 19266: a 12 hour untagged cooldown.
    let row: (i64, i64, Option<i64>) = db
        .query_row(
            "SELECT s.id, s.cooldown_time, s.cooldown_tag_id \
             FROM items i JOIN skills s ON s.id = i.use_skill_id WHERE i.id = 11123",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, (19_266, 43_200_000, None));
    for (id, timing, toggle) in [(RECALL, 1, None), (21_586, 1, None), (ESCAPE, 1, None)] {
        let row: (i64, Option<i64>) = db
            .query_row(
                "SELECT timing_id, toggle_buff_id FROM skills WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (timing, toggle), "skill {id}");
    }
    let glider: (i64, Option<i64>) = db
        .query_row(
            "SELECT timing_id, toggle_buff_id FROM skills WHERE id = 13411",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(glider, (1, Some(1458)));
    let plot: (String, i64) = db
        .query_row(
            "SELECT plot_only, plot_id FROM skills WHERE id = 10201",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(plot, ("t".to_string(), 320));
    let timing_zero: (i64, i64) = db
        .query_row(
            "SELECT timing_id, cooldown_time FROM skills WHERE id = 2",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(timing_zero, (0, 300));
}
