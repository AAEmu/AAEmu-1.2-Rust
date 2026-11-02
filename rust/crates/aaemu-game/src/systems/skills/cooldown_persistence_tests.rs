//! Cooldowns must reach the 1.2 client and survive a relog.
//!
//! Native evidence: `397b64d0` -> `397b2350` reads `i32 skillCount` followed by
//! `(id, total, remaining)` u32 triples, then `i32 tagCount` and the same triples; `OnCooldowns`
//! (`391d43c0`) -> `39899d40` appends them to the local character's cooldown manager, which is
//! what the action bar asks through `GetActionCooldown` (`3943f310` -> `3989a3e0`/`3989a290`).
//! An untagged skill (Recall 14849, Escape 20297, most consumables) therefore still needs an
//! `SCCooldownsPacket` with its individual entry: sending nothing leaves the client with no
//! countdown. The end time is persisted as an absolute Unix-ms stamp, so a relog restores it.

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

fn data() -> GameData {
    let mut data = GameData::empty();
    for (id, tag, duration) in [
        (RECALL, 0, 2_000),
        (ESCAPE, 0, 1_800_000),
        (POTION, 0, 43_200_000),
        (TAGGED_POTION, TAG as i32, 60_000),
    ] {
        data.systems.skills.add_skill(SkillTemplate {
            cooldown_tag_id: tag,
            cooldown_time: duration,
            ..skills::tests::skill_template(id)
        });
    }
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

#[tokio::test]
async fn recall_untagged_cast_sends_the_native_cooldown_packet() {
    let (server, mut client, player, npc) = fixture(data());
    cast(&server, &mut client, RECALL, npc.obj_id);
    let frames = client.drain();
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
    let ui = cooldown_packet(&frames).expect("untagged skill cooldown packet");
    // 397b2350: count 1, (14849, 2000, 2000), then an empty tag list. The remaining time is
    // the authored 2000 ms because the packet is built with the cast's own `now`.
    assert_eq!(
        ui.skills,
        vec![(RECALL, 2_000, 2_000)],
        "individual entry without a tag"
    );
    assert!(ui.tags.is_empty());
    let remaining = player.data().unit.cooldowns.cooldowns[&RECALL] - now_ms();
    assert!(
        (1_900..=2_000).contains(&remaining),
        "remaining = {remaining}"
    );
    cast(&server, &mut client, RECALL, npc.obj_id);
    assert_denied(&client.drain(), skill_result::COOLDOWN_TIME);
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
    assert_eq!(
        cooldown_packet(&client.drain())
            .expect("consumable packet")
            .skills[0]
            .0,
        POTION
    );
    cast(&server, &mut client, TAGGED_POTION, npc.obj_id);
    let ui = cooldown_packet(&client.drain()).expect("tagged packet");
    assert_eq!(ui.skills[0].0, TAGGED_POTION);
    assert_eq!(ui.tags[0].0, TAG);

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

#[tokio::test]
async fn expired_cooldowns_are_neither_saved_nor_restored() {
    let (server, mut client, player, npc) = fixture(data());
    let now = now_ms();
    {
        let mut p = player.data();
        p.unit.cooldowns.cooldowns.insert(ESCAPE, now - 1);
        p.unit.cooldowns.tag_cooldowns.insert(TAG, now - 1);
        p.unit.cooldowns.add_cooldown(RECALL, 2_000, now);
    }
    let rows = cooldown_rows(&player.data(), now);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].kind, rows[0].id), (COOLDOWN_TYPE_SKILL, RECALL));

    let mut fresh = test_character(1, "Caster", 100.0, 100.0);
    apply_cooldown_rows(&mut fresh, &rows, now);
    assert_eq!(fresh.unit.cooldowns.cooldowns.len(), 1);
    assert!(fresh.unit.cooldowns.cooldowns.contains_key(&RECALL));
    assert!(fresh.unit.cooldowns.tag_cooldowns.is_empty());

    // An expired timer does not lock the skill: the cast path removes it and fires.
    player
        .data()
        .unit
        .cooldowns
        .cooldowns
        .insert(ESCAPE, now - 1);
    cast(&server, &mut client, ESCAPE, npc.obj_id);
    assert!(client
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCSkillFiredPacket));
}

#[test]
fn native_reader_bytes_for_an_untagged_cooldown() {
    let mut c = test_character(1, "Caster", 100.0, 100.0);
    c.unit.cooldowns.add_cooldown(RECALL, 2_000, 1_000);
    let packet = SCCooldownsPacket::from_cooldowns(&c.unit.cooldowns, 1_000);
    let mut body = PacketStream::new();
    Marshal::write(&packet, &mut body);
    // 397b2350: skillCount 1, (14849, 2000, 2000), tagCount 0.
    assert_eq!(
        body.into_bytes(),
        [1, 0, 0, 0, 0x01, 0x3a, 0, 0, 0xd0, 0x07, 0, 0, 0xd0, 0x07, 0, 0, 0, 0, 0, 0]
    );
}

/// The `Cooldown` special effect (`40`) starts a cooldown without a cast of its own; it must
/// report it like `start_skill_cooldown` so the client counts it down. No 1.2 skill references
/// the effect, so this drives it directly.
#[tokio::test]
async fn cooldown_effect_reports_the_source_skills_timer() {
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
    let ui = cooldown_packet(&client.drain()).expect("effect cooldown packet");
    assert_eq!(ui.skills.len(), 1);
    assert_eq!(ui.skills[0].0, POTION);
    assert_eq!(ui.skills[0].2, 5_000);
    assert!(ui.tags.is_empty());
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

/// The real 1.2 data: Recall / Escape and the long-cooldown consumable are untagged, so the
/// per-skill entry is the only thing that can make the client show their countdown.
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
}
