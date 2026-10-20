//! Timeline-id lifecycle flow tests: every cast path (instant, interrupted, cancelled,
//! failed validation, item reagent rejection, plot that cannot run, replaced skill reuse)
//! must return its `SkillTlIdManager` id so the pool cannot run out after days of uptime
//! (upstream issue #753). The exhaustion fallback is also exercised: it must not block,
//! panic or leave the skill task slot set.

use std::sync::Arc;
use std::time::Duration;

use crate::data::GameData;
use crate::world::WorldObject;

use super::cast_validation_tests::fixture;
use super::data::{EffectKind, EffectTemplate, SkillTemplate};
use super::skill::use_skill;
use super::state::{SkillRef, SkillRun};
use super::tasks::TlIdManager;
use super::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};

fn cast_skill(
    server: &crate::GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    target: u32,
) -> (u8, u32) {
    use_skill(
        server,
        skill,
        caster,
        SkillCaster::Unit(caster.obj_id()),
        &mut SkillCastTarget::Unit(target),
        Some(SkillObject::None),
        true,
    )
}

/// Waits (bounded) for every pending task of the skill to release its timeline id.
async fn wait_released(skill: &SkillRef) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while skill.tl_id() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(skill.tl_id(), 0, "timeline id not released in time");
}

/// Waits (bounded) for the manager's in-use count to drop back to zero.
async fn wait_pool_empty(mgr: &TlIdManager) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while mgr.in_use() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(mgr.in_use(), 0, "timeline ids still assigned");
}

#[tokio::test]
async fn every_cast_path_returns_its_timeline() {
    let mut data = GameData::empty();
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(super::tests::damage_effect(1, 10))),
        1,
    );
    let (server, _client, player, npc) = fixture(data);
    npc.data().unit.hp = 1_000_000;
    npc.data().unit.max_hp = 1_000_000;
    let me = WorldObject::from(&player);
    let mgr = &server.systems.skills.tl_ids;
    for i in 0..250 {
        // (casting_time, channeling_time) per path: instant, cast interrupted, cast
        // completed, channel interrupted, channel completed.
        let (casting, channeling) = match i % 5 {
            0 => (0, 0),
            1 => (20, 0),
            2 => (10, 0),
            3 => (0, 20),
            _ => (0, 10),
        };
        let skill = SkillRun::new(
            Arc::new(SkillTemplate {
                casting_time: casting,
                channeling_time: channeling,
                effects: vec![super::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
                ..super::tests::skill_template(1000 + i as u32)
            }),
            1,
        );
        match i % 5 {
            // Instant: applied and ended synchronously.
            0 => {
                assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
                assert_eq!(skill.tl_id(), 0);
            }
            // Casting interrupted by the client.
            1 => {
                assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
                assert_ne!(skill.tl_id(), 0);
                super::skill::stop(&server, &skill, &me, 0);
                assert_eq!(skill.tl_id(), 0);
            }
            // Casting that completes on its own.
            2 => {
                assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
                wait_released(&skill).await;
            }
            // Channel interrupted by the client.
            3 => {
                assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
                assert_ne!(skill.tl_id(), 0);
                super::skill::stop(&server, &skill, &me, 0);
                assert_eq!(skill.tl_id(), 0);
            }
            // Channel that runs to its end.
            _ => {
                assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
                wait_released(&skill).await;
            }
        }
        assert_eq!(mgr.in_use(), 0, "iteration {i} left a timeline id assigned");
    }
    assert_eq!(mgr.stats().exhaustions, 0);
    assert_eq!(mgr.stats().unused_releases, 0);
    assert_eq!(mgr.leaked_over(super::tasks::LEAK_AGE_MS), 0);
}

#[tokio::test]
async fn invalid_targets_and_missing_skill_items_do_not_hold_timelines() {
    struct Items;
    impl super::ext::ItemSkillHooks for Items {
        fn use_skill_as_reagent(&self, _server: &crate::GameServer, _template_id: u32) -> bool {
            true
        }
    }
    let mut data = GameData::empty();
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(super::tests::damage_effect(1, 10))),
        1,
    );
    let (server, _client, player, npc) = fixture(data);
    let me = WorldObject::from(&player);
    let mgr = &server.systems.skills.tl_ids;

    // Rejected before the id is handed out (too far).
    npc.data().unit.transform.set_xyz(1000.0, 1000.0, 100.0);
    let skill = SkillRun::new(Arc::new(super::tests::skill_template(1100)), 1);
    assert_eq!(
        cast_skill(&server, &skill, &me, npc.obj_id).0,
        skill_result::TOO_FAR_RANGE
    );
    assert_eq!(skill.tl_id(), 0);
    npc.data().unit.transform.set_xyz(100.0, 105.0, 100.0);

    // Accepted, then rejected by the item-reagent check inside `cast()` (the C# leaked here).
    server.systems.skills.ext.items.install(Arc::new(Items));
    let skill = SkillRun::new(Arc::new(super::tests::skill_template(1101)), 1);
    let mut target = SkillCastTarget::Unit(npc.obj_id);
    let (result, _) = use_skill(
        &server,
        &skill,
        &me,
        SkillCaster::Item {
            obj_id: 1,
            item_id: 4242,
            item_template_id: 55,
            type1: 0,
            type2: 0,
        },
        &mut target,
        Some(SkillObject::None),
        true,
    );
    assert_eq!(result, 0);
    assert!(skill.cancelled());
    assert_eq!(skill.tl_id(), 0);
    assert!(!mgr.is_assigned(0x0A), "the timeline id must be back");
    assert_eq!(mgr.in_use(), 0);
}

#[tokio::test]
async fn plots_that_cannot_run_do_not_hold_timelines() {
    use crate::systems::housing::dominion::runtime::{ENGRAVING_PLOT, ENGRAVING_SKILL};
    let (server, _client, player, npc) = fixture(GameData::empty());
    let me = WorldObject::from(&player);
    let mgr = &server.systems.skills.tl_ids;

    // The engraving plot is refused by the dominion runtime (`begin_plot` false). `use_skill`
    // already rejects the engraving skill (no canonical template), so drive the plot entry
    // point directly: the id must still come back through `do_plot_end`.
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            plot_id: Some(ENGRAVING_PLOT),
            plot_only: true,
            ..super::tests::skill_template(ENGRAVING_SKILL)
        }),
        1,
    );
    skill.state().tl_id = mgr.next_id(me.obj_id(), ENGRAVING_SKILL);
    assert_ne!(skill.tl_id(), 0);
    let ctx = super::skill::CastCtx {
        caster: me.clone(),
        caster_caster: SkillCaster::Unit(me.obj_id()),
        target: super::geometry::Target::Obj(WorldObject::from(&npc)),
        target_caster: SkillCastTarget::Unit(npc.obj_id),
        skill_object: SkillObject::None,
    };
    super::plots::run_plot(&server, ENGRAVING_PLOT, ctx, skill.clone());
    assert_eq!(skill.tl_id(), 0);
    assert_eq!(mgr.in_use(), 0);

    // A plot id with no tree ends through `do_plot_end` too.
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            plot_id: Some(9999),
            plot_only: true,
            ..super::tests::skill_template(1200)
        }),
        1,
    );
    assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
    wait_pool_empty(mgr).await;
    assert_eq!(skill.tl_id(), 0);
}

#[tokio::test]
async fn replaced_skill_reuse_recovers_the_old_timeline() {
    let mut data = GameData::empty();
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(super::tests::damage_effect(1, 10))),
        1,
    );
    let (server, _client, player, npc) = fixture(data);
    let me = WorldObject::from(&player);
    let mgr = &server.systems.skills.tl_ids;
    // Auto attacks reuse one SkillRun; a delayed apply can overlap the next use.
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            effect_delay: 50,
            effects: vec![super::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
            ..super::tests::skill_template(1300)
        }),
        1,
    );
    assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
    let first = skill.tl_id();
    assert_ne!(first, 0);
    assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
    let second = skill.tl_id();
    assert_ne!(second, 0);
    assert_ne!(first, second);
    assert!(
        !mgr.is_assigned(first),
        "the overwritten id must be recovered"
    );
    assert_eq!(mgr.stats().replaced, 1);
    wait_released(&skill).await;
    assert_eq!(mgr.in_use(), 0);
}

#[tokio::test]
async fn an_exhausted_pool_does_not_block_or_panic() {
    let (server, _client, player, npc) = fixture(GameData::empty());
    let me = WorldObject::from(&player);
    let mgr = &server.systems.skills.tl_ids;
    let mut held = Vec::with_capacity(TlIdManager::capacity() as usize);
    while mgr.in_use() < TlIdManager::capacity() {
        let id = mgr.next_id(1, 1);
        assert_ne!(id, 0, "the pool filled early");
        held.push(id);
    }
    assert_eq!(mgr.next_id(1, 1), 0);
    assert_eq!(mgr.stats().exhaustions, 1);
    // With no id available, the cast falls back to "no timeline" and still finishes.
    let skill = SkillRun::new(Arc::new(super::tests::skill_template(1400)), 1);
    assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
    assert_eq!(skill.tl_id(), 0);
    assert!(player.data().unit.skill_state.skill_task.is_none());
    for id in held {
        mgr.release(id);
    }
    assert_eq!(mgr.in_use(), 0);
    assert_eq!(mgr.stats().unused_releases, 0);
}

#[tokio::test]
async fn a_timed_cast_flagged_cancelled_without_stop_releases_its_timeline_once() {
    // Something other than `stop` (a cancel effect, `cancelled = true`) flags the skill while
    // its CastTask is pending: the task must still end the skill and return the id - once.
    let mut data = GameData::empty();
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(super::tests::damage_effect(1, 10))),
        1,
    );
    let (server, _client, player, npc) = fixture(data);
    let me = WorldObject::from(&player);
    let mgr = &server.systems.skills.tl_ids;
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            casting_time: 30,
            effects: vec![super::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
            ..super::tests::skill_template(2000)
        }),
        1,
    );
    assert_eq!(cast_skill(&server, &skill, &me, npc.obj_id).0, 0);
    assert_ne!(skill.tl_id(), 0);
    skill.state().cancelled = true;
    wait_released(&skill).await;
    wait_pool_empty(mgr).await;
    assert_eq!(mgr.stats().unused_releases, 0, "released more than once");
    assert!(
        player.data().unit.skill_state.skill_task.is_none(),
        "the task slot was left set"
    );
}
