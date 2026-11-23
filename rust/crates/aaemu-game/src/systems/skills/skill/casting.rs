use std::sync::Arc;

use crate::server::GameServer;
use crate::systems::instances::encounters::{hooks as encounter_hooks, registry as encounters};
use crate::world::{now_ms, WorldObject};

use super::super::data::{EffectTemplate, SkillTemplate};
use super::super::geometry::{distance_to, world_pos, Target};
use super::super::packets::{SCSkillFiredPacket, SCSkillStartedPacket};
use super::super::state::{CancelToken, SkillRef, SkillRun, SkillTaskKind};
use super::super::stats::{equip_slot, stat, StatKind};
use super::super::types::{skill_result, SkillCaster};
use super::super::{broadcast, buffs};
use super::cooldowns::{on_skill_cooldown, start_global_cooldown, start_skill_cooldown};
use super::{
    apply_effects, consume_mana, end_skill, portal_zone_check, start_channeling, stop, CastCtx,
};

/// `Skill.Cast(caster, casterCaster, target, targetCaster, skillObject)`.
pub fn cast(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    let caster = &ctx.caster;
    if !caster.is_unit() {
        return;
    }
    if skill.cancelled() {
        // A cancellation path that did not run `stop` (the id would otherwise be lost). The
        // task slot of a timed cast is cleared here too (`stop` clears it on its own path).
        caster.with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            if slot
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
            {
                *slot = None;
            }
        });
        end_skill(server, skill, caster);
        return;
    }
    // Source, target and resources may change while the cast timer is running.
    let ready = caster
        .as_player()
        .and_then(|p| portal_zone_check(server, &skill.template, p, Some(&ctx.skill_object)))
        .map_or(Ok(()), |result| Err((result, 0)))
        .and_then(|()| super::validation::check_item_zone(server, caster, &ctx.caster_caster))
        .and_then(|()| super::validation::check(server, skill, caster, &ctx.target))
        .and_then(|()| {
            if on_skill_cooldown(server, skill, caster, now_ms()) {
                Err((skill_result::COOLDOWN_TIME, 0))
            } else {
                Ok(())
            }
        })
        .and_then(|()| {
            super::validation::check_resources(server, skill, caster, &ctx.caster_caster)
        })
        .and_then(|()| {
            super::validation::check_build_materials(server, skill, caster, &ctx.target)
        });
    if let Err((result, value)) = ready {
        if let Some(player) = caster.as_player() {
            let mut packet = SCSkillStartedPacket::new(
                skill.id,
                skill.tl_id(),
                ctx.caster_caster,
                ctx.target_caster,
                ctx.skill_object.clone(),
            );
            packet.set_skill_result(result).set_result_uint(value);
            player.send(packet);
            // A craft cast that fails here never reaches `CraftEffect`: end the craft batch.
            server
                .systems
                .skills
                .ext
                .items()
                .craft_cast_failed(server, player, skill.id, result);
        }
        stop(server, skill, caster, 0);
        return;
    }
    let t = &skill.template;
    if caster.as_npc().is_some() && t.skill_controller_id != 0 {
        if let Some(sc) = server
            .game_data
            .systems
            .skills
            .skill_controller(t.skill_controller_id)
        {
            let dist = super::geometry::raw_distance(
                &world_pos(server, &Target::Obj(caster.clone())),
                &world_pos(server, &ctx.target),
                true,
            );
            if dist >= t.min_range as f32 && dist <= t.max_range as f32 {
                if let Some(sc) =
                    super::controllers::create(server, sc, caster, &ctx.target, 0.0, 0.0, 0.0, 0.0)
                {
                    super::controllers::activate(server, &sc);
                }
            }
        }
    }
    // Validate the cast item before committing mana or cooldowns.
    if let (
        Some(p),
        SkillCaster::Item {
            item_id,
            item_template_id,
            ..
        },
    ) = (caster.as_player(), ctx.caster_caster)
    {
        let items = server.systems.skills.ext.items();
        if items.use_skill_as_reagent(server, item_template_id) {
            match items.owned_item(server, p, item_id) {
                None => {
                    tracing::warn!(
                        "SkillItem does not exists {item_id} (templateId: {item_template_id})"
                    );
                    // The C# returned here and lost the timeline id (issue #753); stop instead.
                    stop(server, skill, caster, 0);
                    return;
                }
                Some((tid, _)) => {
                    if items.item_count(server, p, tid) < 1 {
                        tracing::warn!("SkillItem, player does not own enough of {item_id}");
                        stop(server, skill, caster, 0);
                        return;
                    }
                }
            }
        }
    }
    let now = now_ms();
    // A player's timed cast started the global cooldown at acceptance (see `use_skill`).
    if !std::mem::take(&mut skill.state().gcd_started) {
        start_global_cooldown(server, skill, caster, now);
    }
    // Clear only this skill's task: the timer claims the slot before calling `cast`, and a new
    // cast of another skill (or a reused auto-attack skill) must keep its own slot.
    caster.with_unit(|u| {
        let slot = &mut u.unit_mut().skill_state.skill_task;
        if slot
            .as_ref()
            .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
        {
            *slot = None;
        }
    });
    consume_mana(server, skill, caster, &ctx.caster_caster);
    start_skill_cooldown(server, skill, caster, now);
    if t.channeling_time > 0 {
        start_channeling(server, skill, ctx);
    } else {
        schedule_effects(server, skill, ctx);
    }
}

/// What the cast timer found under the caster's task lock.
enum CastTimerAction {
    /// The task still owns the slot and owes no further wait: this call completes the cast.
    Complete,
    /// The task owns the slot and a delayed `DisturbCasting` added a wait: wake again then.
    Rearm(i64),
    /// The slot is gone or belongs to another task: a stop, a completion or a newer cast won.
    Stale,
}

/// Arms the completion timer of a direct cast. The timer reads the cast's pending `extend_ms`
/// from the caster's task slot under the slot lock; [`delay_cast`] adds to it, so a timer that
/// wakes while a delay is pending re-arms itself for that wait instead of completing. Both
/// paths act on the same slot under one lock, so a cast completes exactly once and a cancelled
/// cast never does. (A wall-clock deadline would not follow a paused test clock.)
pub(super) fn schedule_cast_completion(
    server: &GameServer,
    skill: SkillRef,
    ctx: CastCtx,
    cancel: CancelToken,
    wait_ms: i64,
) {
    let skill2 = skill.clone();
    let ctx2 = ctx.clone();
    let cancel2 = cancel.clone();
    let scheduled = super::tasks::schedule(server, wait_ms, move |srv| {
        cast_completion_tick(&srv, &skill2, &ctx2, &cancel2);
    });
    if !scheduled {
        // No scheduler (server shutting down): finish the cast right away instead of leaving
        // the task slot - and the timeline id - set forever.
        cast(server, &skill, &ctx);
    }
}

/// `CastTask.Execute`: completes the cast once its (possibly delayed) end time is reached.
fn cast_completion_tick(
    server: &GameServer,
    skill: &SkillRef,
    ctx: &CastCtx,
    cancel: &CancelToken,
) {
    // A cancelled token means `stop` ran: it took the task slot and released the timeline id.
    // A skill flagged `cancelled` by something else (a cancel effect, a failed requirement)
    // still holds its id: `cast` ends it, exactly once (`end_skill` takes the id).
    if cancel.is_cancelled() {
        return;
    }
    let action = ctx
        .caster
        .with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            let Some(task) = slot.as_mut() else {
                return CastTimerAction::Stale;
            };
            if task.kind != SkillTaskKind::Cast
                || !Arc::ptr_eq(&task.skill, skill)
                || task.cancel != *cancel
            {
                return CastTimerAction::Stale;
            }
            if task.extend_ms > 0 {
                return CastTimerAction::Rearm(std::mem::take(&mut task.extend_ms));
            }
            // Claim the completion under the same lock `delay_cast` extends under: an extension
            // that runs after this sees no task and does not announce a delay for a cast that
            // is already ending.
            *slot = None;
            CastTimerAction::Complete
        })
        .unwrap_or(CastTimerAction::Stale);
    match action {
        CastTimerAction::Complete => cast(server, skill, ctx),
        CastTimerAction::Rearm(wait_ms) => {
            schedule_cast_completion(server, skill.clone(), ctx.clone(), cancel.clone(), wait_ms);
        }
        CastTimerAction::Stale => {}
    }
}

/// Special effect types of `PlayUserMusic` / `PauseUserMusic`.
const SPECIAL_PLAY_USER_MUSIC: i32 = 93;
const SPECIAL_PAUSE_USER_MUSIC: i32 = 94;

/// Whether a player's skill carries a play / pause score special effect in the static catalog
/// (`skill_effects` -> `SpecialEffect` -> `special_effects.special_effect_type_id` 93 / 94).
pub(in crate::systems::skills) fn is_player_music_skill(
    server: &GameServer,
    template: &SkillTemplate,
    caster: &WorldObject,
) -> bool {
    if caster.as_player().is_none() {
        return false;
    }
    let data = &server.game_data.systems.skills;
    template.effects.iter().any(|e| {
        e.template.is_some_and(|(kind, id)| {
            matches!(
                data.effect(kind, id),
                Some(EffectTemplate::Special(s))
                    if matches!(
                        s.special_effect_type_id,
                        SPECIAL_PLAY_USER_MUSIC | SPECIAL_PAUSE_USER_MUSIC
                    )
            )
        })
    })
}

/// `Skill.ScheduleEffects`.
pub fn schedule_effects(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    let caster = &ctx.caster;
    let t = &skill.template;
    let data = &server.game_data.systems.skills;
    if t.toggle_buff_id != 0 {
        if let (Some(b), Some(target)) = (data.buff(t.toggle_buff_id).cloned(), ctx.target.as_obj())
        {
            buffs::apply_template(
                server,
                &b,
                Some(caster),
                &ctx.caster_caster,
                target,
                Some(skill),
            );
        }
    }
    let mut total_delay = 0i32;
    if t.effect_delay > 0 {
        total_delay += t.effect_delay;
    }
    if t.effect_speed > 0.0 {
        let d = distance_to(server, &Target::Obj(caster.clone()), &ctx.target, false);
        total_delay = total_delay.saturating_add((d / t.effect_speed * 1000.0) as i32);
    }
    let fire_anim_id = fire_anim_for(t, super::plots::weapon_wield_kind(server, caster));
    if fire_anim_id != 0 && t.use_anim_time {
        if let Some(anim) = data.anims.get(fire_anim_id) {
            let mul = stat(server, caster, StatKind::GlobalCooldownMul) as f32;
            total_delay =
                total_delay.saturating_add((anim.combat_sync_time as f32 * (mul / 100.0)) as i32);
        }
    }
    // The cast slot is already cleared, and the interaction effect of a skill that holds a
    // `once_one_man` doodad still owes the doodad's funnel (after the whole delay, the
    // animation time included, or at once): the hold must survive until `apply_effects` /
    // `end_skill`, so another unit cannot take the doodad in between.
    if skill.state().holds_doodad {
        if let Some(doodad) = ctx.target.as_obj().and_then(WorldObject::as_doodad) {
            crate::systems::npcs::doodad::occupancy::mark_effects_pending(
                server,
                doodad,
                caster,
                i64::from(total_delay),
            );
        }
    }
    if matches!(t.id, 2..=4) {
        buffs::trigger_remove_on(server, caster, super::types::BuffRemoveOn::AutoAttack, 0);
    }
    let weapon_anim = weapon_attack_anim_id(server, skill, caster);
    let mut fired = SCSkillFiredPacket {
        id: skill.id,
        tl: skill.tl_id(),
        caster: ctx.caster_caster,
        target: ctx.target_caster,
        skill_object: ctx.skill_object.clone(),
        computed_delay: total_delay,
        channeling_time: t.channeling_time,
        fire_anim_id,
    };
    if weapon_anim > 0 {
        fired.fire_anim_id = weapon_anim;
    }
    // The 1.2 client reacts to its own `SCSkillStarted` of a play / pause score skill (special
    // effects 93 / 94) by sending the MIDI cache / the end of the music. An instant cast has no
    // cast-time `SCSkillStarted`, so the validated cast announces itself right before its fire.
    if std::mem::take(&mut skill.state().music_started_pending) {
        broadcast(
            server,
            caster,
            &SCSkillStartedPacket::new(
                skill.id,
                fired.tl,
                ctx.caster_caster,
                ctx.target_caster,
                ctx.skill_object.clone(),
            ),
            true,
        );
    }
    if encounters::is_wind_sealstone_launch_skill(skill.id) {
        for encounter in encounter_hooks::all() {
            encounter.on_skill_fired(server, skill, caster);
        }
    }
    broadcast(server, caster, &fired, true);
    if total_delay > 0 {
        let skill2 = skill.clone();
        let ctx2 = ctx.clone();
        let timeline = fired.tl;
        let scheduled = super::tasks::schedule(server, i64::from(total_delay), move |srv| {
            // ApplySkillTask
            if skill2.tl_id() != timeline {
                // A newer use of the same SkillRun took the timeline; it recovered this id.
                return;
            }
            if skill2.cancelled() {
                end_skill(&srv, &skill2, &ctx2.caster);
                return;
            }
            if !ctx2.has_live_handles(&srv) {
                skill2.state().cancelled = true;
                end_skill(&srv, &skill2, &ctx2.caster);
                return;
            }
            apply_effects(&srv, &skill2, &ctx2);
            end_skill(&srv, &skill2, &ctx2.caster);
        });
        if !scheduled {
            // No scheduler: apply now and close the timeline instead of leaking it.
            if ctx.has_live_handles(server) {
                apply_effects(server, skill, ctx);
            }
            end_skill(server, skill, caster);
        }
    } else {
        apply_effects(server, skill, ctx);
        end_skill(server, skill, caster);
    }
}

/// `Skill.GetWeaponAttackAnimId(caster)`.
/// The fire clip for a caster whose weapons are of `wield_kind` (`weapon_wield_kind`: 2 two-handed, 3 two weapons): the skill's
/// two-handed or dual wield variant when it has one, else `fire_anim_id`. The 1.2 client plays the clip it is sent and picks no
/// variant itself: sent only `fire_anim_id`, it swung a greatsword with Precision Strike's `onehand_co_sk_weapon_blunt_2`
/// (skill parity trace, 2026-11-03).
pub(super) fn fire_anim_for(t: &SkillTemplate, wield_kind: i32) -> u32 {
    match wield_kind {
        2 if t.twohand_fire_anim_id != 0 => t.twohand_fire_anim_id,
        3 if t.dual_wield_fire_anim_id != 0 => t.dual_wield_fire_anim_id,
        _ => t.fire_anim_id,
    }
}

fn weapon_attack_anim_id(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> u32 {
    if !matches!(skill.template.id, 2..=4) {
        return 0;
    }
    let idx = {
        let mut s = skill.state();
        let i = s.auto_attack_index;
        s.auto_attack_index += 1;
        i
    };
    if caster.as_npc().is_some() {
        return (idx % 2 + 1) as u32;
    }
    if caster.as_player().is_none() {
        skill.state().auto_attack_index -= 1;
        return 0;
    }
    let slot = match skill.template.id {
        3 => equip_slot::OFFHAND,
        4 => equip_slot::RANGED,
        _ => equip_slot::MAINHAND,
    };
    let items = server.systems.skills.ext.items();
    match items.weapon_holdable(server, caster, slot) {
        Some(h) => items.weapon_attack_anim(server, h.holdable_id, idx, skill.template.id == 3),
        None => {
            if idx % 2 == 0 {
                1
            } else {
                2
            }
        }
    }
}

/// Extends a running direct cast's end by `delay_ms` (`DisturbCasting` with a delay, WP-1:
/// the 1.2 client adds the delay to the cast bar when it receives `SCCastingDelayed`). Returns
/// the active cast timeline, or `None` when there is no extendable direct cast task.
pub fn delay_cast(unit: &WorldObject, delay_ms: i64) -> Option<u16> {
    if delay_ms <= 0 {
        return None;
    }
    unit.with_unit(|u| {
        let task = u.unit_mut().skill_state.skill_task.as_mut()?;
        if task.kind != SkillTaskKind::Cast || !task.skill.template.casting_delayable {
            return None;
        }
        let run = task.skill.state();
        if run.cancelled || run.tl_id == 0 {
            return None;
        }
        task.extend_ms = task.extend_ms.saturating_add(delay_ms);
        Some(run.tl_id)
    })
    .flatten()
}
