use std::sync::Arc;

use crate::models::buff::{effect_state, Buff};
use crate::models::unit::BuffAttribute;
use crate::server::GameServer;
use crate::systems::instances::encounters::{hooks as encounter_hooks, registry as encounters};
use crate::world::{now_ms, WorldObject};

use super::super::data::BuffTemplate;
use super::super::state::SkillRun;
use super::super::types::SkillCaster;
use super::{buff_skill_caster, check_buff_immune, exit, find_obj, full_template, new_buff};

// ------------------------------------------------------------------ AddBuff

/// Longest transform chain followed in one `AddBuff` call. The data chains are two links
/// (4152 -> 4752 -> 4101 and 5378 -> 5379 -> 5380); a longer chain is treated as bad data.
const MAX_TRANSFORM_CHAIN: usize = 8;

/// `Buffs.AddBuff(buff, index, forcedDuration)`: tolerance, duration modifiers, stack rules,
/// then the start behaviour (through the buff events) and a final tolerance buff.
pub fn add_buff(
    server: &GameServer,
    owner: &WorldObject,
    buff: Buff,
    index: u32,
    forced_duration: i32,
) {
    add_buff_inner(server, owner, buff, index, forced_duration, &mut Vec::new());
}

fn add_buff_inner(
    server: &GameServer,
    owner: &WorldObject,
    mut buff: Buff,
    index: u32,
    forced_duration: i32,
    transform_chain: &mut Vec<u32>,
) {
    if encounter_hooks::all()
        .iter()
        .any(|encounter| encounter.blocks_buff(server, owner, &buff))
    {
        return;
    }
    let data = &server.game_data.systems.skills;
    let now = now_ms();
    let template = full_template(server, &buff.template);
    buff.state = effect_state::CREATED;

    // Tolerance (`BuffGameData.GetBuffToleranceForBuffTag` of the first tag that has one).
    let tolerance = data
        .buff_tags(template.id)
        .iter()
        .find_map(|t| data.tolerance_for_tag(*t))
        .cloned();
    let mut final_tolerance_buff = 0u32;
    let mut current_reduction: Option<u32> = None;
    if let Some(tol) = &tolerance {
        if owner.is_unit() {
            let has_final = owner.with_base_unit(|b| b.buffs().check_buff(tol.final_step_buff_id));
            let counter = owner
                .with_unit(|u| {
                    u.unit()
                        .skill_state
                        .tolerance_counters
                        .get(&tol.id)
                        .copied()
                })
                .flatten();
            match counter {
                Some(mut c) if !has_final => {
                    if now > c.last_step_ms + i64::from(tol.step_duration) * 1000 {
                        if let Some(first) = tol.first_step() {
                            c.current_step = first;
                        }
                    } else {
                        let Some(next) = tol.step_after(c.current_step.0) else {
                            tracing::error!(
                                "AddBuff {}: tolerance {} has no step after {} (Sequence contains no matching element)",
                                template.id,
                                tol.id,
                                c.current_step.0
                            );
                            return;
                        };
                        if next.2 <= c.current_step.2 {
                            final_tolerance_buff = tol.final_step_buff_id;
                            if let Some(first) = tol.first_step() {
                                c.current_step = first;
                            }
                        } else {
                            c.current_step = next;
                        }
                    }
                    c.last_step_ms = now;
                    current_reduction = Some(c.current_step.2);
                    owner.with_unit(|u| {
                        u.unit_mut()
                            .skill_state
                            .tolerance_counters
                            .insert(tol.id, c);
                    });
                }
                Some(_) => {
                    tracing::error!(
                        "AddBuff {}: tolerance counter {} already exists (Dictionary.Add)",
                        template.id,
                        tol.id
                    );
                    return;
                }
                None => {
                    let Some(first) = tol.first_step() else {
                        tracing::error!(
                            "AddBuff {}: tolerance {} has no steps",
                            template.id,
                            tol.id
                        );
                        return;
                    };
                    owner.with_unit(|u| {
                        u.unit_mut().skill_state.tolerance_counters.insert(
                            tol.id,
                            super::state::ToleranceCounter {
                                tolerance_id: tol.id,
                                current_step: first,
                                last_step_ms: now,
                            },
                        );
                    });
                    current_reduction = Some(first.2);
                }
            }
        }
    }

    let tags = data.buff_tags(template.id).to_vec();
    let mut duration = template.get_duration(buff.ab_level);
    if let Some(caster) = find_obj(server, buff.caster_obj_id) {
        if let Some(d) = caster.with_unit(|u| {
            u.unit().buff_modifiers.apply(
                template.id,
                &tags,
                BuffAttribute::Duration,
                f64::from(duration),
            )
        }) {
            duration = d as i32;
        }
    }
    if let Some(d) = owner.with_unit(|u| {
        u.unit().buff_modifiers.apply(
            template.id,
            &tags,
            BuffAttribute::InDuration,
            f64::from(duration),
        )
    }) {
        duration = d as i32;
    }
    if let (Some(tol), Some(reduction)) = (&tolerance, current_reduction) {
        duration = (f64::from(duration) * ((100.0 - f64::from(reduction)) / 100.0)) as i32;
        let caster_is_char =
            find_obj(server, buff.caster_obj_id).is_some_and(|c| c.as_player().is_some());
        if caster_is_char && owner.as_player().is_some() {
            duration = (f64::from(duration)
                * ((100.0 - f64::from(tol.character_time_reduction)) / 100.0))
                as i32;
        }
    }
    if forced_duration != 0 {
        duration = forced_duration;
    }
    buff.duration = duration;

    let (skill_caster, caster_obj, skill_id, skill_level, ab_level) = (
        buff_skill_caster(&buff),
        buff.caster_obj_id,
        buff.skill_id,
        buff.skill_level,
        buff.ab_level,
    );
    owner.with_base_unit(|b| {
        let buffs = b.buffs_mut();
        // Thundering Strike's 24-second stack window is the length of four authored
        // cooldowns. Renew the window on a hit so the fifth one can petrify the target.
        if encounters::refreshes_stack_window(template.id)
            && buffs.buff_count_by_id(template.id) + 1 < template.max_stack
        {
            buffs.refresh_stack_timers(template.id, duration, now);
        }
        buffs.add_buff_with_duration(buff, index, now);
    });
    crate::systems::dispatch_buff_events(server, owner);
    transform_at_max_stack(
        server,
        owner,
        &template,
        &BuffApplication {
            caster_obj,
            skill_caster: &skill_caster,
            skill_id,
            skill_level,
            ab_level,
        },
        transform_chain,
    );

    if final_tolerance_buff > 0 {
        if let Some(t) = data.buff(final_tolerance_buff).cloned() {
            let caster = find_obj(server, caster_obj);
            let skill = (skill_id != 0)
                .then(|| data.skill(skill_id).cloned())
                .flatten()
                .map(|t| SkillRun::new(t, skill_level));
            let b = new_buff(
                server,
                &t,
                caster.as_ref(),
                &skill_caster,
                skill.as_deref(),
                now_ms(),
            );
            add_buff(server, owner, b, 0, 0);
        }
    }
}

/// The application of an accepted buff, carried into its transform replacement.
struct BuffApplication<'a> {
    caster_obj: u32,
    skill_caster: &'a SkillCaster,
    skill_id: u32,
    skill_level: u8,
    ab_level: u32,
}

/// Replace `template`'s stacks with `transform_buff_id` once the application that reaches
/// `max_stack` has landed.
///
/// Evidence (1.2 tooltips): buff 1757 (`max_stack` 2) "Two stacks turn it into Nymph's Flame",
/// buff 2214 (`max_stack` 4) "Stacks up to 3 times. A 4th stack Stuns", buff 2407
/// (`max_stack` 5) "Petrifies upon reaching 5 stacks": the transform happens on the
/// `max_stack`-th application, not the one after it. All 66 rows are `stack_rule_id` 4 with
/// `max_stack >= 2` (compact `buffs`). The stacks are removed through the normal removal path
/// (Dispel hooks and `SCBuffRemovedPacket`) and the replacement keeps the caster, skill and
/// ability level of the triggering application, then goes through the normal add path so a
/// transform that reaches its own cap transforms again (4152 -> 4752 -> 4101).
fn transform_at_max_stack(
    server: &GameServer,
    owner: &WorldObject,
    template: &Arc<BuffTemplate>,
    application: &BuffApplication<'_>,
    transform_chain: &mut Vec<u32>,
) {
    if template.transform_buff_id == 0 || template.max_stack <= 0 {
        return;
    }
    let stacks = owner.with_base_unit(|b| b.buffs().buff_count_by_id(template.id));
    if stacks < template.max_stack {
        return;
    }
    let data = &server.game_data.systems.skills;
    let Some(transform) = data.buff(template.transform_buff_id).cloned() else {
        // `Data::load_from` warns once and clears such ids; a synthetic template keeps today's
        // refresh-at-cap behaviour.
        return;
    };
    if transform_chain.len() >= MAX_TRANSFORM_CHAIN || transform_chain.contains(&transform.id) {
        tracing::warn!(
            buff = template.id,
            transform = transform.id,
            depth = transform_chain.len(),
            "AddBuff: transform chain does not terminate; keeping the stacks"
        );
        return;
    }
    let indices: Vec<u32> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| e.template.id == template.id)
            .map(|e| e.index)
            .collect()
    });
    for index in indices {
        exit(server, owner, index, false);
    }
    // Same caster / skill as the application that reached the cap (inferred; the 1.2 data and
    // tooltips only establish the replacement itself).
    let caster = find_obj(server, application.caster_obj);
    let skill = (application.skill_id != 0)
        .then(|| data.skill(application.skill_id).cloned())
        .flatten()
        .map(|t| SkillRun::new(t, application.skill_level));
    let mut b = new_buff(
        server,
        &transform,
        caster.as_ref(),
        application.skill_caster,
        skill.as_deref(),
        now_ms(),
    );
    b.ab_level = application.ab_level;
    // Keep the source application's skill even when its template is not loaded (a synthetic
    // template or a removed skill row); `new_buff` only fills these from a `SkillRun`.
    b.skill_id = application.skill_id;
    b.skill_level = application.skill_level;
    transform_chain.push(template.id);
    add_buff_inner(server, owner, b, 0, 0, transform_chain);
    transform_chain.pop();
}

/// `Buffs.AddBuff(buffId, caster, forcedDuration)`.
pub fn add_buff_by_id(
    server: &GameServer,
    owner: &WorldObject,
    buff_id: u32,
    caster: &WorldObject,
    forced_duration: i32,
) {
    add_buff_by_id_with_charge(server, owner, buff_id, caster, forced_duration, 0);
}

/// `Buffs.AddBuff(buffId, ...)` with an explicit initial charge (the `battle_field_buffs.value`
/// column). A charge of 0 keeps the template's `init_min_charge` / `init_max_charge` roll.
pub fn add_buff_by_id_with_charge(
    server: &GameServer,
    owner: &WorldObject,
    buff_id: u32,
    caster: &WorldObject,
    forced_duration: i32,
    charge: i32,
) {
    let Some(t) = server.game_data.systems.skills.buff(buff_id).cloned() else {
        tracing::error!("AddBuff: unknown buff {buff_id}");
        return;
    };
    let mut b = new_buff(
        server,
        &t,
        Some(caster),
        &SkillCaster::Unit(caster.obj_id()),
        None,
        now_ms(),
    );
    b.charge = charge;
    add_buff(server, owner, b, 0, forced_duration);
}

/// `BuffTemplate.Apply(caster, casterObj, target, targetObj, castObj, source, skillObject, time)`.
pub fn apply_template(
    server: &GameServer,
    template: &BuffTemplate,
    caster: Option<&WorldObject>,
    caster_caster: &SkillCaster,
    target: &WorldObject,
    source_skill: Option<&SkillRun>,
) {
    if template.id > 0 && target.with_base_unit(|b| b.buffs().check_buff(template.id)) {
        return;
    }
    if template.require_buff_id > 0
        && !target.with_base_unit(|b| b.buffs().check_buff(template.require_buff_id))
    {
        return;
    }
    if check_buff_immune(server, target, template.id) {
        return;
    }
    let ab_level = buff_ab_level(server, caster, source_skill);
    let mut b = new_buff(
        server,
        template,
        caster,
        caster_caster,
        source_skill,
        now_ms(),
    );
    b.ab_level = ab_level;
    if let (Some(caster), Some(skill)) = (caster, source_skill) {
        b.range_damage_multiplier = super::effects::combat::range_multiplier_for_skill(
            server,
            caster,
            target,
            skill.template.id,
        );
    }
    // The height factor is scoped by the damage effect's own flag, not by the skill; capture
    // it from the application positions for every caster (a DoT tick keeps the application
    // height, like it keeps the close-range factor above).
    if let Some(caster) = caster {
        b.height_damage_multiplier =
            super::effects::combat::height_multiplier_at_application(server, caster, target);
    }
    add_buff(server, target, b, 0, 0);
}

/// The `abLevel` of `BuffTemplate.Apply` / `BuffEffect.Apply`.
pub fn buff_ab_level(
    server: &GameServer,
    caster: Option<&WorldObject>,
    skill: Option<&SkillRun>,
) -> u32 {
    let mut ab_level = 1u32;
    match (caster.and_then(|c| c.as_player()), skill) {
        (Some(p), Some(s)) => {
            let t = &s.template;
            let ability_level = super::skill::ab_level(server, &WorldObject::from(p), t.ability_id);
            ab_level = if t.level_step != 0 {
                (ability_level / t.level_step * t.level_step) as u32
            } else {
                t.ability_level as u32
            };
            ab_level = t.ability_level.max(ab_level as i32) as u32;
        }
        (Some(_), None) => {}
        (None, Some(s)) => ab_level = s.template.ability_level as u32,
        (None, None) => {}
    }
    ab_level
}
