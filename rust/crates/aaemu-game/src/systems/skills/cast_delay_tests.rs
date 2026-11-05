//! WP-1: a `DisturbCasting` (special effect 5) that carries a delay extends a running direct
//! cast and tells the client with `SCCastingDelayed` (`u16 skillId, u16 plotId, u32 delay`)
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
    let skill = start_timed_cast(&mut f);
    let accepted = now_ms();

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
    // Independent literals: skill 12390 = 0x3066, plot 0, delay 400 = 0x190.
    assert_eq!(
        delayed.1,
        vec![0x66, 0x30, 0x00, 0x00, 0x90, 0x01, 0x00, 0x00]
    );

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
