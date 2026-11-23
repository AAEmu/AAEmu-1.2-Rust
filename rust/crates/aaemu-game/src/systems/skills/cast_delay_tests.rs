//! WP-1: a `DisturbCasting` (special effect 5) that carries a delay extends a running direct
//! cast and tells the client with `SCCastingDelayed` (`u16 tl, u16 plot_tl, u32 delay`)
//! instead of interrupting it. A delay of 0 keeps the immediate interrupt.

use std::time::Duration;

use super::*;

use crate::net::registry::ClientPacket;
use crate::world::now_ms;

use super::super::effects::special;
use super::super::effects::{EffectArgs, EffectSource};
use super::super::geometry::Target;
use super::super::handlers::CSStopCastingPacket;
use super::super::state::SkillRef;
use super::super::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};

/// The 1.2 skill that reaches a delayed `DisturbCasting`: Low Kick (12390, effect row 5134,
/// 10 %, 1000 ms).
const CAST_SKILL: u32 = 12_390;
const CAST_MS: i32 = 300;
const DELAY_MS: i32 = 400;

fn cast_data(cast_ms: i32) -> GameData {
    let mut d = data();
    let mut t = (**d.systems.skills.skill(DAMAGE_SKILL).unwrap()).clone();
    t.id = CAST_SKILL;
    t.casting_time = cast_ms;
    t.casting_delayable = true;
    t.casting_cancelable = true;
    d.systems.skills.add_skill(t);
    d
}

fn channel_data(channel_ms: i32) -> GameData {
    let mut d = data();
    let mut t = (**d.systems.skills.skill(DAMAGE_SKILL).unwrap()).clone();
    t.id = CAST_SKILL;
    t.casting_time = 0;
    t.channeling_time = channel_ms;
    d.systems.skills.add_skill(t);
    d
}

fn disturb_args<'a>(
    caster: &WorldObject,
    target: &WorldObject,
    skill_object: &'a SkillObject,
) -> EffectArgs<'a> {
    EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(caster.obj_id()),
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: CastAction::Skill {
            skill_id: 1,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object,
    }
}

fn start_skill_task(f: &mut Fixture) -> super::super::state::SkillTaskSlot {
    f.client.conn.active_char = Some(f.player.clone());
    start_skill(
        &f.server,
        &mut f.client.conn,
        cast_packet(CAST_SKILL, SkillCastTarget::Unit(NPC_OBJ)),
    );
    let task = f
        .player
        .data()
        .unit
        .skill_state
        .skill_task
        .clone()
        .expect("skill task");
    f.client.drain();
    task
}

fn start_timed_cast(f: &mut Fixture) -> SkillRef {
    let task = start_skill_task(f);
    assert_eq!(task.kind, super::super::state::SkillTaskKind::Cast);
    task.skill
}

fn start_channel(f: &mut Fixture) -> SkillRef {
    let task = start_skill_task(f);
    assert!(
        matches!(
            task.kind,
            super::super::state::SkillTaskKind::EndChanneling { channel_doodad: 0 }
        ),
        "expected a channel task, got {:?}",
        task.kind
    );
    task.skill
}

#[tokio::test]
async fn delayed_disturb_extends_the_cast_and_sends_casting_delayed() {
    let mut f = fixture_with(cast_data(CAST_MS));
    start_skill(
        &f.server,
        &mut f.client.conn,
        cast_packet(CAST_SKILL, SkillCastTarget::Unit(NPC_OBJ)),
    );
    let accepted = now_ms();
    let skill = f
        .player
        .data()
        .unit
        .skill_state
        .skill_task
        .as_ref()
        .expect("skill task")
        .skill
        .clone();
    let started = frames(&mut f.client)
        .into_iter()
        .find(|(op, _)| *op == sc::SCSkillStartedPacket)
        .expect("skill started")
        .1;
    let timeline = u16::from_le_bytes(started[4..6].try_into().unwrap());

    let victim = WorldObject::from(&f.player);
    let disturber = WorldObject::from(&f.npc);
    let none = SkillObject::None;
    special::execute(
        &f.server,
        5,
        &disturb_args(&disturber, &victim, &none),
        [100, DELAY_MS, 0, 0],
    );

    // Still casting, not interrupted, and the client was told to add the delay to the bar.
    assert!(!skill.cancelled());
    assert!(f.player.data().unit.skill_state.skill_task.is_some());
    let got = frames(&mut f.client);
    let delayed = got
        .iter()
        .find(|(op, _)| *op == sc::SCCastingDelayedPacket)
        .expect("SCCastingDelayed");
    assert_eq!(timeline, skill.tl_id());
    assert_ne!(u32::from(timeline), CAST_SKILL);
    let [lo, hi] = timeline.to_le_bytes();
    assert_eq!(delayed.1, [lo, hi, 0, 0, 0x90, 0x01, 0, 0]);

    // The original 300 ms end has passed; the extended (300 + 400 ms) one has not.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        f.player.data().unit.skill_state.skill_task.is_some(),
        "the cast must not complete at its original end"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while f.player.data().unit.skill_state.skill_task.is_some()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        f.player.data().unit.skill_state.skill_task.is_none(),
        "the extended cast did not end"
    );
    assert!(!skill.cancelled());
    assert!(
        now_ms() - accepted >= i64::from(CAST_MS + DELAY_MS) - 50,
        "the cast ended at its original time instead of the delayed one"
    );
    assert_eq!(
        f.npc.data().unit.hp,
        400,
        "the cast effects ran once, at the delayed end"
    );
}

#[tokio::test]
async fn disturb_without_a_delay_still_interrupts() {
    let mut f = fixture_with(cast_data(10_000));
    let skill = start_timed_cast(&mut f);

    let victim = WorldObject::from(&f.player);
    let disturber = WorldObject::from(&f.npc);
    let none = SkillObject::None;
    special::execute(
        &f.server,
        5,
        &disturb_args(&disturber, &victim, &none),
        [100, 0, 0, 0],
    );

    assert!(skill.cancelled());
    assert!(f.player.data().unit.skill_state.skill_task.is_none());
    assert_eq!(skill.tl_id(), 0);
    let got = frames(&mut f.client);
    assert!(
        got.iter().any(|(op, _)| *op == sc::SCCastingStoppedPacket),
        "{got:x?}"
    );
    assert!(
        !got.iter().any(|(op, _)| *op == sc::SCCastingDelayedPacket),
        "{got:x?}"
    );
}

#[tokio::test]
async fn a_cast_cancelled_after_the_extension_does_not_complete() {
    let mut f = fixture_with(cast_data(CAST_MS));
    let skill = start_timed_cast(&mut f);

    let victim = WorldObject::from(&f.player);
    let disturber = WorldObject::from(&f.npc);
    let none = SkillObject::None;
    special::execute(
        &f.server,
        5,
        &disturb_args(&disturber, &victim, &none),
        [100, DELAY_MS, 0, 0],
    );
    let pending = f
        .player
        .data()
        .unit
        .skill_state
        .skill_task
        .as_ref()
        .expect("the cast is still running")
        .extend_ms;
    assert!(
        pending >= i64::from(DELAY_MS),
        "the cast timer must owe the delay"
    );

    // The client cancels through the real packet path after the extension.
    CSStopCastingPacket {
        tl_id: skill.tl_id(),
        plot_tl_id: 0,
        obj_id: PLAYER_OBJ,
    }
    .handle(&f.server, &mut f.client.conn)
    .await
    .unwrap();
    assert!(skill.cancelled());
    assert!(f.player.data().unit.skill_state.skill_task.is_none());

    // Past the extended end: the timer that is still armed must not complete the cast.
    tokio::time::sleep(Duration::from_millis((CAST_MS + DELAY_MS + 300) as u64)).await;
    assert_eq!(
        f.npc.data().unit.hp,
        1000,
        "the cancelled cast applied its effects at the delayed end"
    );
    assert!(skill.cancelled());
}

#[tokio::test]
async fn a_delayed_disturb_interrupts_a_channel_after_the_delay() {
    // A channel has no `SCCastingDelayed` equivalent, so a delayed disturb keeps today's
    // behaviour: the old interrupt after the delay (capture-pending in PORTING.md), not an
    // immediate one and not an extension.
    let mut f = fixture_with(channel_data(10_000));
    let skill = start_channel(&mut f);

    let victim = WorldObject::from(&f.player);
    let disturber = WorldObject::from(&f.npc);
    let none = SkillObject::None;
    special::execute(
        &f.server,
        5,
        &disturb_args(&disturber, &victim, &none),
        [100, DELAY_MS, 0, 0],
    );

    // Still channeling immediately after the effect: the interrupt is armed, not immediate.
    assert!(!skill.cancelled());
    assert!(f.player.data().unit.skill_state.skill_task.is_some());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while f.player.data().unit.skill_state.skill_task.is_some()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(skill.cancelled(), "the delayed interrupt did not run");
    assert!(f.player.data().unit.skill_state.skill_task.is_none());
    // The channel was not extended: no delay packet for it.
    let got = frames(&mut f.client);
    assert!(
        !got.iter().any(|(op, _)| *op == sc::SCCastingDelayedPacket),
        "{got:x?}"
    );
}

#[tokio::test(start_paused = true)]
async fn long_fire_delay_uses_full_milliseconds_on_the_wire_and_in_the_scheduler() {
    // ms / 10 + 10: 4010 = 0x0FAA and 9010 = 0x2332 (the 90 s value wrapped as i16 before).
    for (delay, expected) in [(40_000, [0xAA, 0x0F]), (90_000, [0x32, 0x23])] {
        // 90,000 ms is the effect_delay authored for skill 16385.
        let mut d = cast_data(0);
        let mut t = (**d.systems.skills.skill(CAST_SKILL).unwrap()).clone();
        t.effect_delay = delay;
        d.systems.skills.add_skill(t);
        let mut f = fixture_with(d);
        start_skill(
            &f.server,
            &mut f.client.conn,
            cast_packet(CAST_SKILL, SkillCastTarget::Unit(NPC_OBJ)),
        );

        let fired = frames(&mut f.client)
            .into_iter()
            .find(|(op, _)| *op == sc::SCSkillFiredPacket)
            .expect("skill fired")
            .1;
        // Skill id, timeline, caster, target and skill object precede the two u16 times.
        // No channel: 0 ms -> 10 with the bias.
        assert_eq!(fired[15..19], [expected[0], expected[1], 10, 0]);

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(delay as u64 - 10)).await;
        tokio::task::yield_now().await;
        assert_eq!(f.npc.data().unit.hp, 1000);
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(f.npc.data().unit.hp, 400);
    }
}

#[tokio::test]
async fn long_cast_start_times_saturate_at_the_unsigned_wire_limit() {
    // Skill 11747 authors a 1,000,000 ms direct cast.
    let mut f = fixture_with(cast_data(1_000_000));
    start_skill(
        &f.server,
        &mut f.client.conn,
        cast_packet(CAST_SKILL, SkillCastTarget::Unit(NPC_OBJ)),
    );

    let started = frames(&mut f.client)
        .into_iter()
        .find(|(op, _)| *op == sc::SCSkillStartedPacket)
        .expect("skill started")
        .1;
    assert_eq!(started[15..19], [0xFF; 4]);
}

#[tokio::test(start_paused = true)]
async fn long_channel_keeps_its_server_duration_when_the_wire_value_saturates() {
    let mut f = fixture_with(channel_data(700_000));
    start_skill(
        &f.server,
        &mut f.client.conn,
        cast_packet(CAST_SKILL, SkillCastTarget::Unit(NPC_OBJ)),
    );

    let fired = frames(&mut f.client)
        .into_iter()
        .find(|(op, _)| *op == sc::SCSkillFiredPacket)
        .expect("channel fired")
        .1;
    // Fire delay 0 -> 10 (the 100 ms bias); the channel saturates at u16::MAX.
    assert_eq!(fired[15..19], [0x0A, 0, 0xFF, 0xFF]);

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(655_350)).await;
    tokio::task::yield_now().await;
    assert!(f.player.data().unit.skill_state.skill_task.is_some());
    tokio::time::advance(Duration::from_millis(44_650)).await;
    tokio::task::yield_now().await;
    assert!(f.player.data().unit.skill_state.skill_task.is_none());
}

#[tokio::test(start_paused = true)]
async fn delayed_disturb_leaves_a_non_delayable_cast_untouched() {
    let mut d = cast_data(CAST_MS);
    let mut t = (**d.systems.skills.skill(CAST_SKILL).unwrap()).clone();
    t.casting_delayable = false;
    d.systems.skills.add_skill(t);
    let mut f = fixture_with(d);
    let skill = start_timed_cast(&mut f);

    let victim = WorldObject::from(&f.player);
    let disturber = WorldObject::from(&f.npc);
    special::execute(
        &f.server,
        5,
        &disturb_args(&disturber, &victim, &SkillObject::None),
        [100, 100, 0, 0], // compact special_effects 11220
    );

    assert!(
        !f.client.drain_ops().contains(&sc::SCCastingDelayedPacket),
        "a non-delayable cast must not announce a delay"
    );
    assert!(!skill.cancelled());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(CAST_MS as u64)).await;
    tokio::task::yield_now().await;
    assert!(f.player.data().unit.skill_state.skill_task.is_none());
    assert_eq!(f.npc.data().unit.hp, 400);
}

#[tokio::test(start_paused = true)]
async fn a_delayed_channel_disturb_cannot_interrupt_the_successor() {
    let mut f = fixture_with(channel_data(10_000));
    let original = start_channel(&mut f);
    let victim = WorldObject::from(&f.player);
    let disturber = WorldObject::from(&f.npc);
    special::execute(
        &f.server,
        5,
        &disturb_args(&disturber, &victim, &SkillObject::None),
        [100, DELAY_MS, 0, 0],
    );
    tokio::task::yield_now().await;

    crate::systems::skills::skill::interrupt_skills(&f.server, &victim);
    f.player.data().unit.skill_last_used_ms = 0;
    let successor = start_channel(&mut f);
    assert!(original.cancelled());
    tokio::time::advance(Duration::from_millis(DELAY_MS as u64)).await;
    tokio::task::yield_now().await;
    assert!(!successor.cancelled());
    assert!(f.player.data().unit.skill_state.skill_task.is_some());
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
async fn real_compact_cancel_permissions_apply_to_the_requested_stage() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let catalog = crate::systems::skills::data::Data::load_from(
        &db,
        &crate::data::client_files::ClientFileManager::new(),
    )
    .unwrap();

    for (id, cancelable) in [(10644, false), (13661, true), (11230, false), (10201, true)] {
        let authored = catalog.skill(id).unwrap();
        let mut d = cast_data(authored.casting_time);
        let mut t = (**d.systems.skills.skill(CAST_SKILL).unwrap()).clone();
        t.channeling_time = authored.channeling_time;
        t.casting_cancelable = authored.casting_cancelable;
        t.channeling_cancelable = authored.channeling_cancelable;
        d.systems.skills.add_skill(t);
        let mut f = fixture_with(d);
        let task = start_skill_task(&mut f);
        let skill = task.skill;

        let stop = CSStopCastingPacket {
            tl_id: skill.tl_id(),
            plot_tl_id: 0,
            obj_id: PLAYER_OBJ,
        };
        if cancelable {
            // A plot request cannot select the direct slot even when the number matches.
            CSStopCastingPacket {
                tl_id: 0,
                plot_tl_id: stop.tl_id,
                ..stop
            }
            .handle(&f.server, &mut f.client.conn)
            .await
            .unwrap();
            assert!(
                !skill.cancelled(),
                "plot timeline request stopped direct skill {id}"
            );
        }
        stop.handle(&f.server, &mut f.client.conn).await.unwrap();
        assert_eq!(skill.cancelled(), cancelable, "skill {id}");
        assert_eq!(
            f.client.drain_ops().contains(&sc::SCCastingStoppedPacket),
            cancelable,
            "skill {id}"
        );
        if !cancelable {
            let victim = WorldObject::from(&f.player);
            crate::systems::skills::skill::interrupt_skills(&f.server, &victim);
            assert!(skill.cancelled(), "forced interruption of skill {id}");
        }
    }
}
