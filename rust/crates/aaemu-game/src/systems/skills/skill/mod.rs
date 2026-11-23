//! `Skill`: `Use`, `GetInitialTarget` / `SetInitialTarget`, `Cast`, `StartChanneling`,
//! `EndChanneling`, `ScheduleEffects`, `ApplyEffects`, `EndSkill`, `Stop`, `StopSkill`,
//! `RollCombatDice`, `ManaCost` / `ConsumeMana`, plus `Unit.InterruptSkills`, `Unit.UseSkill`,
//! `GetAbLevel`, auto attack (`StartAutoSkill` / `UseAutoAttackSkillTask`,
//! `SkillManager.GetAttackDelay`) and the cast / channel / apply tasks.

use crate::models::transform::PositionAndRotation;
use crate::server::GameServer;
use crate::systems::instances::encounters::hooks as encounter_hooks;
use crate::world::{now_ms, WorldObject};

use self::casting::schedule_cast_completion;
use self::cooldowns::{on_skill_cooldown, start_global_cooldown, start_skill_cooldown};
use self::targeting::initial_target;
use super::data::{EffectTemplate, SkillTemplate};
use super::geometry::{world_pos, PosUnit, Target};
use super::packets::{wire_duration_div10, SCSkillStartedPacket};
use super::state::{
    CancelToken, DirectCastEnd, PlotCooldownSnapshot, SkillRef, SkillRun, SkillTaskKind,
    SkillTaskSlot,
};
use super::types::{
    skill_result, BuffRemoveOn, SkillCastTarget, SkillCaster, SkillObject, SkillTargetType,
};
use super::{
    broadcast, buffs, combat, controllers, data, geometry, plots, progress, relation, tasks, types,
    unit_reqs, validation, Data,
};

mod apply;
mod auto_attack;
mod casting;
mod channeling;
mod completion;
mod cooldowns;
mod scaling;
mod targeting;

pub use self::apply::{apply_effects, npc_tag_targets, roll_combat_dice};
pub use self::auto_attack::{attack_delay, start_auto_skill};
pub(super) use self::casting::is_player_music_skill;
pub use self::casting::{cast, delay_cast, schedule_effects};
pub use self::channeling::{end_channeling, start_channeling};
pub(crate) use self::completion::interrupt_ai_skills;
pub use self::completion::{end_skill, interrupt_skills, stop, stop_skill};
pub(super) use self::completion::{refund_cancelled_plot_cooldown, reset_cooldown_if_ignored};
pub(crate) use self::cooldowns::send_recall_cooldown;
pub use self::scaling::{
    ab_level, apply_skill_modifiers, cast_skill_level, cast_time_ms, consume_mana, effective_level,
    level_and_inc, mana_cost, mana_level_scale, new_skill_for, rank_for_level, rank_inc,
    skips_level_mana,
};

#[cfg(test)]
use self::apply::nearest_kill_npc;
#[cfg(test)]
use self::auto_attack::auto_attack_tick;
#[cfg(test)]
use self::auto_attack::schedule_auto_attack;
#[cfg(test)]
use self::casting::fire_anim_for;
#[cfg(test)]
use super::data::EffectKind;
#[cfg(test)]
use super::state::AutoAttackSlot;
#[cfg(test)]
use super::types::ApplicationMethod;
#[cfg(test)]
use super::types::SkillTargetSelection;
#[cfg(test)]
use std::sync::Arc;

/// `SkillsEnum.DigUpTreasureChestMarkedOnMap`.
pub const DIG_UP_TREASURE_SKILL: u32 = 18_349;
pub(crate) const RECALL_SKILL: u32 = 10_594;
pub(super) const PET_DUAL_RECOVERY: u32 = 15_220;
pub(super) const PET_DUAL_RECOVERY_ALT: u32 = 21_628;

/// The arguments a cast carries through its tasks.
#[derive(Debug, Clone)]
pub struct CastCtx {
    pub caster: WorldObject,
    pub caster_caster: SkillCaster,
    pub target: Target,
    pub target_caster: SkillCastTarget,
    pub skill_object: SkillObject,
}

impl CastCtx {
    fn has_live_handles(&self, server: &GameServer) -> bool {
        super::geometry::is_live_object(server, &self.caster)
            && !self
                .target
                .as_obj()
                .is_some_and(|target| !super::geometry::is_live_object(server, target))
    }
}

/// Network grace of the global-cooldown and per-use throttle checks in [`use_skill`]: a request
/// that arrives with at most this much of the cooldown left is still accepted, so client/server
/// clock jitter does not refuse a legitimate press. Same idea as the 250 ms tolerance of
/// `UnitCooldowns::check_cooldown`, but tighter so an over-eager client cannot press early.
const GCD_GRACE_MS: i64 = 150;

/// `Skill.Use(caster, casterCaster, targetCaster, skillObject, bypassGcd, out value)`:
/// (`SkillResult`, extra value).
pub fn use_skill(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    caster_caster: SkillCaster,
    target_caster: &mut SkillCastTarget,
    skill_object: Option<SkillObject>,
    bypass_gcd: bool,
) -> (u8, u32) {
    use_skill_impl(
        server,
        skill,
        caster,
        caster_caster,
        target_caster,
        skill_object,
        bypass_gcd,
        false,
        false,
    )
}

/// [`use_skill`] for a skill that an effect starts on its caster's behalf (the `SkillUse`
/// special effect: a buff trigger, a plot or a doodad chaining a follow-up skill). It bypasses
/// the global cooldown, as that effect always did.
///
/// An instant follow-up (no cast or channel time) also goes through while the caster runs a
/// timed cast of another skill. Such a skill never takes the caster's task slot, so the cast
/// in progress completes as before. Refusing it with `skill_on_casting` silently dropped the
/// chained skill: Lady Velarre's Hellfire Flower (16020, 2 s cast) is her basic attack, and a
/// Bound Soul mark (buff 2586) that timed out during that cast summoned no souls (16442).
pub fn use_triggered_skill(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    caster_caster: SkillCaster,
    target_caster: &mut SkillCastTarget,
    skill_object: Option<SkillObject>,
) -> (u8, u32) {
    use_skill_impl(
        server,
        skill,
        caster,
        caster_caster,
        target_caster,
        skill_object,
        true,
        false,
        true,
    )
}

/// [`use_skill`] with a check-only mode: every refusal is returned exactly as a real use would
/// return it, but a passing check stops before the first cast-state mutation. `triggered`:
/// see [`use_triggered_skill`].
#[allow(clippy::too_many_arguments)]
fn use_skill_impl(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    caster_caster: SkillCaster,
    target_caster: &mut SkillCastTarget,
    skill_object: Option<SkillObject>,
    bypass_gcd: bool,
    dry_run: bool,
    triggered: bool,
) -> (u8, u32) {
    if !caster.is_unit() {
        tracing::debug!(
            skill_id = skill.id,
            "Skill use refused: the caster is not a unit"
        );
        return (skill_result::INVALID_SOURCE, 0);
    }
    if let Some(result) = encounter_hooks::all()
        .iter()
        .find_map(|encounter| encounter.cast_gate(server, skill, caster))
    {
        return result;
    }
    if let Err(result) = super::validation::check_item_zone(server, caster, &caster_caster) {
        tracing::debug!(
            skill_id = skill.id,
            result = ?result,
            "Skill use refused: the item cannot be used in this zone"
        );
        return result;
    }
    let template = skill.template.clone();
    // The client refuses a cap-scale cast on an `item_cap_scale_forbids` target before it is
    // sent (`FUN_3939dc20` -> SkillResult INVALID_TARGET 0x22); a forged cast is refused here
    // before the source item is taken, and answers `SCSkillStarted` with result 0x22.
    if let Err(result) = super::validation::check_cap_scale_target(
        server,
        caster,
        &template,
        &caster_caster,
        target_caster,
    ) {
        return result;
    }
    let resolved_target = initial_target(server, skill, caster, &caster_caster, target_caster);
    // The `once_one_*` interaction target, kept as a handle: `resolved_target` is moved into
    // `owned_target` below and the gate runs after that.
    let interaction_doodad = resolved_target
        .as_ref()
        .and_then(Target::as_obj)
        .and_then(WorldObject::as_doodad)
        .cloned();
    let Some(target) = &resolved_target else {
        if let Some(npc) = caster.as_npc() {
            server
                .systems
                .skills
                .ext
                .npcs()
                .npc_no_aggro_target(server, npc);
        }
        return (skill_result::NO_TARGET, 0);
    };
    if let Err(result) = super::validation::check(server, skill, caster, target)
        .and_then(|()| super::validation::check_build_materials(server, skill, caster, target))
    {
        tracing::debug!(
            skill_id = skill.id,
            result = ?result,
            target = target.obj_id(),
            "Skill use refused: the source / target validation failed"
        );
        return result;
    }
    // The 1.2 fishing reactions require the caster's own spawned NPC. OwnerId is
    // the character id; using an object id here would accept unrelated NPCs or
    // reject the fisherman. Resolve this before cast state, GCD or buff mutations.
    let owned_target = if template.target_my_npc {
        let Some(player) = caster.as_player() else {
            return (unit_reqs::skill_result_for_key("skill_not_my_npc"), 0);
        };
        let target = resolved_target.clone();
        let owner = target
            .as_ref()
            .and_then(Target::as_obj)
            .and_then(WorldObject::as_npc)
            .map(|npc| npc.data().unit.owner_id);
        if player.character_id == 0 || owner != Some(player.character_id) {
            return (unit_reqs::skill_result_for_key("skill_not_my_npc"), 0);
        }
        target
    } else {
        resolved_target
    };
    // r208022 `skills` 20866 (guard-tower engraving) is target type 5 (AnyUnit), not Building:
    // it still takes the dominion check before any cast state is touched (`begin_plot` repeats it).
    let housing_target = (template.target_type == SkillTargetType::Building as i32
        || template.id == crate::systems::housing::dominion::runtime::ENGRAVING_SKILL)
        .then(|| initial_target(server, skill, caster, &caster_caster, target_caster))
        .flatten();
    if owned_target
        .as_ref()
        .or(housing_target.as_ref())
        .is_some_and(|target| {
            !crate::systems::housing::dominion::runtime::check_skill(server, skill, caster, target)
        })
    {
        return (skill_result::INVALID_TARGET, 0);
    }
    let character = caster.as_player().cloned();
    let req = unit_reqs::can_use_skill_on_target(
        server,
        &template,
        caster,
        &caster_caster,
        owned_target.as_ref().and_then(Target::as_obj),
    );
    if !req.is_ok() {
        if let Some(c) = &character {
            let c = c.data();
            tracing::warn!(
                "{} ({}) failed requirements to use skill {} - {}",
                c.unit.name,
                c.id,
                template.id,
                req.key
            );
        }
        skill.state().cancelled = true;
        return (unit_reqs::skill_result_for_key(req.key), req.value);
    }
    // Resolve buff restrictions before cooldown writes or StartSkill buff removal:
    // removing a restrictive buff first would let the forbidden skill bypass it.
    let checked_target = if !server
        .game_data
        .systems
        .skills
        .buff_skill_requirements
        .is_empty()
    {
        let target = owned_target
            .or_else(|| initial_target(server, skill, caster, &caster_caster, target_caster));
        if let Some(req) = super::buff_reqs::failed_requirement(
            server,
            &template,
            caster,
            target.as_ref().and_then(Target::as_obj),
        ) {
            tracing::debug!(
                skill_id = template.id,
                requirement = req,
                "Skill use refused: the buff skill requirement failed"
            );
            skill.state().cancelled = true;
            return (skill_result::SKILL_REQ_FAIL, req);
        }
        target
    } else {
        owned_target
    };
    let active_task = caster
        .with_unit(|u| u.unit().skill_state.skill_task.clone())
        .flatten();
    if let Some(task) = &active_task {
        match task.kind {
            // An instant triggered skill leaves the task slot to the cast in progress.
            SkillTaskKind::Cast
                if !(triggered && template.casting_time <= 0 && template.channeling_time <= 0) =>
            {
                tracing::debug!(
                    skill_id = skill.id,
                    active_skill = task.skill.template.id,
                    "Skill use refused: the caster is already casting"
                );
                return (unit_reqs::skill_result_for_key("skill_on_casting"), 0);
            }
            SkillTaskKind::EndChanneling { .. }
                if !task.skill.template.stop_channeling_on_start_skill =>
            {
                tracing::debug!(
                    skill_id = skill.id,
                    active_skill = task.skill.template.id,
                    "Skill use refused: the caster is channeling"
                );
                return (
                    unit_reqs::skill_result_for_key("skill_cannot_cast_in_channeling"),
                    0,
                );
            }
            _ => {}
        }
    }
    if let Err(result) = super::validation::check_resources(server, skill, caster, &caster_caster) {
        tracing::debug!(
            skill_id = skill.id,
            result = ?result,
            "Skill use refused: the caster lacks the resources"
        );
        return result;
    }
    if let Some(c) = &character {
        if let Some(result) = portal_zone_check(server, &template, c, skill_object.as_ref()) {
            return (result, 0);
        }
    }
    let now = now_ms();
    if on_skill_cooldown(server, skill, caster, now) {
        tracing::debug!(
            skill_id = skill.id,
            "Skill use refused: the skill is on cooldown"
        );
        return (skill_result::COOLDOWN_TIME, 0);
    }
    skill.state().bypass_gcd = bypass_gcd;
    if !bypass_gcd {
        let delay: i64 = if matches!(skill.id, 2..=4) {
            if character.is_some() {
                500
            } else {
                1500
            }
        } else {
            150
        };
        let (last_used, gcd) = caster
            .with_unit(|u| (u.unit().skill_last_used_ms, u.unit().global_cooldown_ms))
            .unwrap_or((0, 0));
        // Deviation from C# (`Skill.Use` + `Helper.CheckInterval`): the C# only refused a second
        // attempt inside the 150 ms interval, so the first press during any GCD got through.
        // Refuse outright while more than the network grace of the cooldown is left.
        if last_used + delay - now > GCD_GRACE_MS {
            tracing::debug!(
                skill_id = skill.id,
                last_used,
                now,
                "Skill use refused: the previous skill was used too recently"
            );
            return (skill_result::COOLDOWN_TIME, 0);
        }
        if !template.ignore_global_cooldown && gcd - now > GCD_GRACE_MS {
            tracing::debug!(
                skill_id = skill.id,
                global_cooldown_ms = gcd,
                now,
                "Skill use refused: the global cooldown is still running"
            );
            return (skill_result::COOLDOWN_TIME, 0);
        }
    }
    if dry_run {
        return (skill_result::SUCCESS, 0);
    }
    // 1.2 `doodad_almighties.once_one_man`: another unit's timed interaction with a held
    // doodad is refused before any of this cast's side effects (stealth / StartSkill buff
    // removal, unmount, plot run, GCD, mana, reagent, labor). A held timed cast is acquired
    // here too, before those side effects; a seat use only checks the seat. Instant non-seat
    // interactions are never blocked and take no occupancy. The client-facing caller sends the
    // single in-use message; this path stays silent for the other caster kinds.
    if let Some(doodad) = interaction_doodad.as_ref() {
        if !crate::systems::npcs::doodad::occupancy::begin_skill(server, doodad, caster, skill) {
            return (skill_result::INVALID_TARGET, 0);
        }
    }
    if let Some(task) = active_task {
        if let SkillTaskKind::EndChanneling { channel_doodad } = task.kind {
            stop(server, &task.skill, caster, channel_doodad);
        }
    }
    if !bypass_gcd {
        caster.with_unit(|u| u.unit_mut().skill_last_used_ms = now);
    }
    if let Some(c) = &character {
        c.data().skill_char.skill_cancelled = false;
    }
    {
        let mut state = skill.state();
        state.generation = state.generation.wrapping_add(1);
        state.direct_cast_end = DirectCastEnd::Pending;
        state.cancelled = false;
        state.effect_cancelled = false;
        state.music_started_pending = false;
        state.gcd_started = false;
        state.plot_cooldown_snapshot = None;
        state.hit_types.clear();
    }
    caster.with_unit(|u| u.unit_mut().skill_state.condition_chance = true);
    if template.cancel_ongoing_buffs {
        // `skills.keep_stealth`: a stealth skill keeps the caster's stealth buffs through its
        // own start/use cancel loop (see `buffs::trigger_remove_on_keep_stealth`).
        let trigger = |on| {
            if template.keep_stealth {
                buffs::trigger_remove_on_keep_stealth(
                    server,
                    caster,
                    on,
                    template.cancel_ongoing_buff_exception_tag_id,
                );
            } else {
                buffs::trigger_remove_on(
                    server,
                    caster,
                    on,
                    template.cancel_ongoing_buff_exception_tag_id,
                );
            }
        };
        if caster.as_mate().is_some() {
            trigger(BuffRemoveOn::UseSkill);
        }
        trigger(BuffRemoveOn::StartSkill);
    }
    let skill_object = skill_object.unwrap_or_default();
    let Some(target) = checked_target
        .or_else(|| initial_target(server, skill, caster, &caster_caster, target_caster))
    else {
        if let Some(n) = caster.as_npc() {
            server
                .systems
                .skills
                .ext
                .npcs()
                .npc_no_aggro_target(server, n);
        }
        tracing::trace!(
            "Skill: SkillResult.NoTarget! - Skill {}, Caster {}",
            template.id,
            caster.obj_id()
        );
        skill.state().initial_target = 0;
        return (skill_result::NO_TARGET, 0);
    };
    skill.state().initial_target = target.obj_id();
    // Unmount character if skill asks for it (vehicles subsystem)
    if template.unmount {
        if let Some(p) = caster.as_player() {
            crate::systems::vehicles::mates::unmount_riding_character(server, p, true);
        }
    }
    let tl = server
        .systems
        .skills
        .tl_ids
        .next_id(caster.obj_id(), template.id);
    // A reused `SkillRun` (auto attack) can still hold an id when its previous use never ended
    // (a leak path). Recover it so the pool does not run out; the counter makes it visible.
    let previous = std::mem::replace(&mut skill.state().tl_id, tl);
    if previous != 0 {
        server.systems.skills.tl_ids.release_replaced(previous);
    }
    tracing::debug!(
        "Created SkillTlId {tl} for Skill {}, Caster {} with target {}",
        template.id,
        caster.obj_id(),
        target.obj_id()
    );
    let ctx = CastCtx {
        caster: caster.clone(),
        caster_caster,
        target: target.clone(),
        target_caster: *target_caster,
        skill_object: skill_object.clone(),
    };
    if template.plot_id.is_some() {
        // Plot-only skills never reach `apply_effects`: verify the cast item is still the same
        // owned item before the plot runs (the plot itself consumes plot reagents).
        if let (Some(p), SkillCaster::Item { item_id, .. }) = (caster.as_player(), caster_caster) {
            let items = server.systems.skills.ext.items();
            let still_owned =
                items
                    .owned_item(server, p, item_id)
                    .is_some_and(|(template_id, _)| {
                        items.consume_owned_item(server, p, item_id, template_id, 0) != Some(false)
                    });
            if !still_owned {
                if let Some(doodad) = interaction_doodad.as_ref() {
                    crate::systems::npcs::doodad::occupancy::end_cast(
                        server,
                        doodad.obj_id,
                        caster,
                    );
                }
                skill.state().cancelled = true;
                return (skill_result::INVALID_SOURCE, 0);
            }
        }
    }
    if let Some(plot_id) = template.plot_id {
        if template.plot_only {
            // Plot-only skills never enter `cast`; commit cooldowns once at acceptance,
            // before a plot can finish synchronously. A plot cancelled while still casting
            // gives them back (`refund_cancelled_plot_cooldown`).
            let tag = template.cooldown_tag_id.max(0) as u32;
            let snapshot = caster.with_unit(|u| {
                let u = u.unit_mut();
                PlotCooldownSnapshot {
                    skill_cooldown_end: u.cooldowns.cooldowns.get(&template.id).copied(),
                    tag_id: tag,
                    tag_cooldown_end: u.cooldowns.tag_cooldowns.get(&tag).copied(),
                    global_cooldown_ms: u.global_cooldown_ms,
                }
            });
            skill.state().plot_cooldown_snapshot = snapshot;
            start_global_cooldown(server, skill, caster, now);
            start_skill_cooldown(server, skill, caster, now);
            super::debug_casts::report(server, skill, &ctx, tl, 0);
        }
        super::plots::run_plot(server, plot_id, ctx.clone(), skill.clone());
        if template.plot_only {
            // No cast follows: a plot-only skill never holds a doodad.
            if let Some(doodad) = interaction_doodad.as_ref() {
                crate::systems::npcs::doodad::occupancy::end_cast(server, doodad.obj_id, caster);
            }
            return (skill_result::SUCCESS, 0);
        }
    }
    // Casting time
    let mut cast_time = cast_time_ms(server, skill, caster);
    let ctm = skill.state().cast_time_multiplier;
    cast_time = (f64::from(cast_time) * f64::from(ctm)).round_ties_even() as i32;
    super::debug_casts::report(server, skill, &ctx, tl, cast_time.max(0));
    if cast_time > 0 {
        // The 1.2 client measures a player's global cooldown from the cast start, so start it
        // now; `cast` would only start it at the cast end, refusing a skill the client already
        // shows as ready. NPC casts keep the cast-end start (their AI pacing relies on it).
        if caster.as_npc().is_none() {
            start_global_cooldown(server, skill, caster, now);
            skill.state().gcd_started = true;
        }
        let mut p = SCSkillStartedPacket::new(
            skill.id,
            tl,
            ctx.caster_caster,
            ctx.target_caster,
            ctx.skill_object.clone(),
        );
        p.base_cast_time_div10 = wire_duration_div10(cast_time);
        p.real_cast_time_div10 = p.base_cast_time_div10;
        broadcast(server, caster, &p, true);
        let cancel = CancelToken::new();
        caster.with_unit(|u| {
            u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
                kind: SkillTaskKind::Cast,
                skill: skill.clone(),
                cancel: cancel.clone(),
                extend_ms: 0,
            })
        });
        schedule_cast_completion(
            server,
            skill.clone(),
            ctx.clone(),
            cancel,
            i64::from(cast_time),
        );
    } else {
        if is_player_music_skill(server, &template, caster) {
            skill.state().music_started_pending = true;
        }
        cast(server, skill, &ctx);
    }
    (skill_result::SUCCESS, 0)
}

/// The `AccessLevel < 100` checks of `Skill.Use` for `Return` / `OpenPortalEffect` skills.
fn portal_zone_check(
    server: &GameServer,
    template: &SkillTemplate,
    c: &crate::world::PlayerRef,
    skill_object: Option<&SkillObject>,
) -> Option<u8> {
    let (access, district, faction, instance) = {
        let d = c.data();
        (
            d.access_level,
            d.return_district_id,
            d.unit.faction_id,
            d.unit.transform.instance_id,
        )
    };
    if access >= 100 {
        return None;
    }
    let data = &server.game_data.systems.skills;
    for effect in &template.effects {
        let Some((kind, id)) = effect.template else {
            continue;
        };
        let mut destination_instance = instance;
        let destination = match data.effect(kind, id) {
            Some(EffectTemplate::Special(s)) if s.special_effect_type_id == 25 => {
                if s.value1 > 0 {
                    crate::systems::npcs::portal::world_gates_by_id(server, s.value1 as u32)
                        .inspect(|gate| {
                            destination_instance =
                                super::effects::special::movement::return_gate_instance(
                                    server,
                                    instance,
                                    gate.zone_id,
                                )
                                .unwrap_or(instance);
                        })
                        .cloned()
                        .or_else(|| {
                            if instance != 0 {
                                return None;
                            }
                            let t = c.data().main_world_position?;
                            destination_instance = t.instance_id;
                            Some(crate::data::world_data::Portal {
                                zone_id: t.zone_id,
                                x: t.x,
                                y: t.y,
                                z: t.z,
                                ..Default::default()
                            })
                        })
                } else {
                    destination_instance = crate::models::character::DEFAULT_INSTANCE_ID;
                    let rp = server
                        .game_data
                        .portals
                        .district_return_point(district, faction);
                    server
                        .game_data
                        .portals
                        .recalls
                        .values()
                        .flatten()
                        .find(|p| p.id == rp)
                        .cloned()
                        .or_else(|| {
                            if rp == 0 || instance != 0 {
                                return None;
                            }
                            let t = c.data().main_world_position?;
                            destination_instance = t.instance_id;
                            Some(crate::data::world_data::Portal {
                                zone_id: t.zone_id,
                                x: t.x,
                                y: t.y,
                                z: t.z,
                                ..Default::default()
                            })
                        })
                }
            }
            Some(EffectTemplate::OpenPortal { .. }) => {
                if instance != 0 {
                    return Some(skill_result::INVALID_LOCATION);
                }
                match skill_object {
                    Some(SkillObject::PortalInfo { id, .. }) => {
                        crate::systems::npcs::portal::portal_info(&c.data().portals, *id as u32)
                            .cloned()
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(p) = destination {
            if !crate::world::session::zone_access_allowed(
                server,
                c,
                destination_instance,
                p.zone_id,
                p.x,
                p.y,
            ) {
                return Some(skill_result::NO_PERM);
            }
        }
    }
    None
}

/// `Unit.UseSkill(skillId, target)`.
pub fn unit_use_skill(
    server: &GameServer,
    unit: &WorldObject,
    skill_id: u32,
    target: &WorldObject,
) -> u8 {
    unit_use_skill_impl(server, unit, skill_id, target, false).0
}

/// The checks [`unit_use_skill`] performs for `skill_id`, without casting or changing the unit.
/// Returns the first refusal exactly as the real use would return it.
pub fn unit_use_skill_check(
    server: &GameServer,
    unit: &WorldObject,
    skill_id: u32,
    target: &WorldObject,
) -> (u8, u32) {
    unit_use_skill_impl(server, unit, skill_id, target, true)
}

fn unit_use_skill_impl(
    server: &GameServer,
    unit: &WorldObject,
    skill_id: u32,
    target: &WorldObject,
    dry_run: bool,
) -> (u8, u32) {
    let Some(t) = server.game_data.systems.skills.skill(skill_id).cloned() else {
        // Retail data has quest components pointing at skills without a `skills` row (see
        // PORTING.md); warn once per id, repeats go to debug.
        let first = server
            .systems
            .skills
            .missing_skill_warned
            .lock()
            .expect("missing skill set poisoned")
            .insert(skill_id);
        let caster = unit.obj_id();
        let target = target.obj_id();
        if first {
            tracing::warn!("UseSkill: skill {skill_id} has no template (dangling retail reference), skipped; caster {caster} target {target}");
        } else {
            tracing::debug!("UseSkill: skill {skill_id} has no template (dangling retail reference), skipped; caster {caster} target {target}");
        }
        return (skill_result::INVALID_SKILL, 0);
    };
    let skill = SkillRun::new(t, 1);
    let mut sct = if target.as_doodad().is_some() {
        SkillCastTarget::Doodad(target.obj_id())
    } else {
        SkillCastTarget::Unit(target.obj_id())
    };
    use_skill_impl(
        server,
        &skill,
        unit,
        SkillCaster::Unit(unit.obj_id()),
        &mut sct,
        None,
        true,
        dry_run,
        false,
    )
}

/// The world position as a `PositionAndRotation` (re-export for callers).
pub fn pos(server: &GameServer, t: &Target) -> PositionAndRotation {
    world_pos(server, t)
}

/// A position unit target (for callers outside the module).
pub fn pos_target(p: PosUnit) -> Target {
    Target::Pos(p)
}

#[cfg(test)]
mod owned_npc_target_tests;

#[cfg(test)]
mod modifier_flow_tests;

#[cfg(test)]
mod cooldown_tag_flow_tests;

#[cfg(test)]
mod auto_attack_lifecycle_tests;

#[cfg(test)]
mod fire_anim_tests;

#[cfg(test)]
mod self_despawn_tests;
