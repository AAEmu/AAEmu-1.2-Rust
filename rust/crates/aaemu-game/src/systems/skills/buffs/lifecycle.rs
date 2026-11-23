use std::sync::Arc;

use rand::Rng;

use crate::models::buff::{effect_state, Buff};
use crate::models::unit::{Bonus, DynamicBonus};
use crate::server::GameServer;
use crate::systems::instances::encounters::registry as encounters;
use crate::world::{now_ms, WorldObject};

use super::super::data::BuffTemplate;
use super::super::effects::{EffectArgs, EffectSource};
use super::super::geometry::Target;
use super::super::packets::{SCBuffCreatedPacket, SCBuffRemovedPacket, SCBuffUpdatedPacket};
use super::super::types::{BuffEventTriggerKind, CastAction, SkillCastTarget, SkillObject};
use super::super::{broadcast, combat, effects};
use super::triggers::{fire_buff_trigger, unsubscribe_triggers};
use super::{
    buff_skill, buff_skill_caster, check_buffs_by_tag, current_buff, do_area_tick, find_obj,
    full_template, get_buff, set_buff,
};

// ------------------------------------------------------------------ Buff lifecycle

/// `Buff.ScheduleEffect(replace)`.
pub fn schedule_effect(server: &GameServer, owner: &WorldObject, index: u32, replace: bool) {
    let Some(b) = get_buff(owner, index) else {
        return;
    };
    // ACTING arrives from the timer. Explicit exit enters FINISHING, including a
    // CREATED buff whose Started event has not yet completed its lifecycle.
    let timed_out = b.state == effect_state::ACTING;
    let template = full_template(server, &b.template);
    let now = now_ms();
    match b.state {
        effect_state::CREATED => {
            set_buff(owner, index, |e| {
                e.state = effect_state::ACTING;
                e.in_use = true;
            });
            start(server, owner, index);
            let Some((gen, delay)) = set_buff(owner, index, |e| {
                if e.duration == 0 && !e.permanent {
                    e.duration = template.get_duration(e.ab_level);
                }
                e.tick = template.tick;
                e.task_gen = super::state::next_task_gen();
                if e.tick > 0.0 {
                    let time = e.time_left(now);
                    e.count = if time > 0.0 {
                        (time / e.tick + 0.5 + 1.0) as i32
                    } else {
                        -1
                    };
                    (e.task_gen, e.tick)
                } else {
                    (e.task_gen, e.time_left(now))
                }
            }) else {
                return;
            };
            schedule_dispel_task(server, owner, index, gen, delay);
            if template.faction_id > 0 && owner.is_unit() {
                combat::set_faction(server, owner, template.faction_id);
            }
            return;
        }
        effect_state::ACTING => {
            let on_action_time = template.on_action_time();
            // The state is read again under the lock: the end of a completed channel
            // ([`finish_effect_by_skill`]) can take the same ACTING buff at this very moment,
            // and exactly one of the two may run its Timeout.
            let step = set_buff(owner, index, |e| {
                if e.state != effect_state::ACTING {
                    return TimerStep::Taken;
                }
                if e.count == -1 {
                    if on_action_time {
                        return TimerStep::Tick;
                    }
                } else if e.count > 0 {
                    e.count -= 1;
                    if on_action_time && e.count > 0 {
                        return TimerStep::Tick;
                    }
                }
                e.state = effect_state::FINISHING;
                TimerStep::Finish
            });
            match step {
                Some(TimerStep::Tick) => {
                    let caster = find_obj(server, b.caster_obj_id);
                    time_to_time_apply(server, &template, caster.as_ref(), owner, &b);
                    return;
                }
                Some(TimerStep::Finish) => {}
                // The buff is gone, or the other party owns its end.
                Some(TimerStep::Taken) | None => return,
            }
        }
        _ => {}
    }
    // FINISHING -> FINISHED is a claim as well: only the caller that makes the step stops the
    // effect.
    let finishing = set_buff(owner, index, |e| {
        if e.state != effect_state::FINISHING {
            return false;
        }
        e.state = effect_state::FINISHED;
        e.in_use = false;
        e.exit_replace = replace;
        true
    })
    .unwrap_or(false);
    if finishing {
        stop_effect_task(server, owner, index, timed_out);
    }
}

/// What one `Buff.ScheduleEffect` timer step of an ACTING buff does.
enum TimerStep {
    /// A periodic tick (`TimeToTimeApply`).
    Tick,
    /// The buff is over: ACTING -> FINISHING was made by this caller.
    Finish,
    /// Someone else took the buff meanwhile (the end of a completed channel).
    Taken,
}

/// `Buff.StopEffectTask(replace)` (the replace flag travels in `Buff.exit_replace`).
pub(super) fn stop_effect_task(
    server: &GameServer,
    owner: &WorldObject,
    index: u32,
    timed_out: bool,
) {
    let Some(b) = get_buff(owner, index) else {
        return;
    };
    if timed_out {
        fire_buff_trigger(server, owner, index, BuffEventTriggerKind::Timeout);
    }
    unsubscribe_triggers(server, owner, &b);
    if current_buff(owner, &b).is_none() {
        return;
    }
    owner.with_base_unit(|u| u.buffs_mut().remove_effect_by_index(index));
    crate::systems::dispatch_buff_events(server, owner);
}

pub(super) fn is_nerta_bomb_drop(owner: &WorldObject, buff_id: u32) -> bool {
    encounters::is_nerta_bomb_drop(buff_id, || {
        owner.as_npc().map(|npc| npc.data().unit.template_id)
    })
}

pub(in crate::systems::skills) fn nerta_bomb_drop_active(owner: &WorldObject) -> bool {
    is_nerta_bomb_drop(owner, encounters::NERTA_BOMB_BUFF)
        && owner.with_base_unit(|unit| {
            unit.buffs().check_buff(encounters::NERTA_BOMB_BUFF)
                || unit.buffs().check_buff(encounters::NERTA_BOMB_BUFF_2)
        })
}

/// `Buff.Exit(replace)`.
pub fn exit(server: &GameServer, owner: &WorldObject, index: u32, replace: bool) {
    let Some(b) = get_buff(owner, index) else {
        return;
    };
    if b.state == effect_state::FINISHED {
        return;
    }
    // DispelEffect and explicit removal must not enter the timer's Timeout path. FISH
    // status removal during catch plot792 otherwise invokes its escape skill21616.
    set_buff(owner, index, |e| e.state = effect_state::FINISHING);
    if b.state != effect_state::CREATED {
        schedule_effect(server, owner, index, replace);
    }
}

/// `Buff.ConsumeCharge(value)`: returns the part of `value` the charge did not absorb.
///
/// A non-passive buff that stays updated in place gets `SCBuffUpdatedPacket` (stack 1, the
/// remaining charge) to the same audience as `SCBuffCreatedPacket` / `SCBuffRemovedPacket`;
/// passive buffs send no buff packets at all. The C# had no sender, so the timing is inferred;
/// no retail 1.2 server capture exists (see `PORTING.md`).
pub fn consume_charge(server: &GameServer, owner: &WorldObject, index: u32, value: i32) -> i32 {
    let Some((rest, exit_now, charge, changed, passive)) = set_buff(owner, index, |e| {
        let old = e.charge;
        let new_charge = (old - value).max(0);
        let rest = (value - old).max(0);
        e.charge = new_charge;
        (
            rest,
            new_charge <= 0,
            new_charge,
            new_charge != old,
            e.passive,
        )
    }) else {
        return value;
    };
    if exit_now {
        exit(server, owner, index, false);
    } else if changed && !passive {
        broadcast(
            server,
            owner,
            &SCBuffUpdatedPacket {
                obj_id: owner.obj_id(),
                index,
                stack: 1,
                charge,
            },
            true,
        );
    }
    rest
}

pub(super) fn schedule_dispel_task(
    server: &GameServer,
    owner: &WorldObject,
    index: u32,
    gen: u64,
    delay: f64,
) {
    let owner = owner.clone();
    super::tasks::schedule(server, delay as i64, move |srv| {
        if super::geometry::is_live_object(&srv, &owner) {
            dispel_task(&srv, owner.obj_id(), index, gen);
        }
    });
}

/// `DispelTask.Execute`.
pub fn dispel_task(server: &GameServer, owner_id: u32, index: u32, gen: u64) {
    let Some(owner) = find_obj(server, owner_id) else {
        return;
    };
    let Some(b) = get_buff(&owner, index) else {
        return;
    };
    if b.task_gen != gen || b.state == effect_state::FINISHED || b.state == effect_state::FINISHING
    {
        return;
    }
    schedule_effect(server, &owner, index, false);
    let Some(b) = get_buff(&owner, index) else {
        return;
    };
    if b.state == effect_state::FINISHED || b.state == effect_state::FINISHING || b.task_gen != gen
    {
        return;
    }
    schedule_dispel_task(server, &owner, index, gen, b.tick);
}

// ------------------------------------------------------------------ BuffTemplate behaviour

/// `BuffTemplate.Start(caster, owner, buff)`.
pub fn start(server: &GameServer, owner: &WorldObject, index: u32) {
    let Some(b) = get_buff(owner, index) else {
        return;
    };
    let template = full_template(server, &b.template);
    let data = &server.game_data.systems.skills;
    remove_bonuses(owner, &template, index);
    let mut bonuses = Vec::new();
    for t in &template.bonuses {
        let value = (f64::from(t.value)
            + f64::from(t.linear_level_bonus) * f64::from(b.ab_level as f32 / 100.0))
        .round_ties_even() as i32;
        bonuses.push(Bonus {
            template: t.clone(),
            value,
        });
    }
    let mut dynamics = Vec::new();
    for t in &template.dynamic_bonuses {
        match t.func_type.as_str() {
            "LinearFunc" => match data.linear_func(t.func_id) {
                Some(f) => dynamics.push(DynamicBonus {
                    template: t.clone(),
                    source_buff_index: index,
                    source_start_ms: b.start_time_ms,
                    source_duration: b.duration,
                    linear_func: Some(f),
                    manual_values: None,
                    source_tick_ms: template.tick,
                }),
                None => tracing::warn!(
                    "Missing linear_func {} for dynamic_unit_modifier on buff {}.",
                    t.func_id,
                    template.id
                ),
            },
            "ManualFunc" => match data.manual_funcs.get(&t.func_id) {
                Some(values) => dynamics.push(DynamicBonus {
                    template: t.clone(),
                    source_buff_index: index,
                    source_start_ms: b.start_time_ms,
                    source_duration: b.duration,
                    linear_func: None,
                    manual_values: Some(values.clone()),
                    source_tick_ms: template.tick,
                }),
                None => tracing::warn!(
                    "Missing manual_func {} for dynamic_unit_modifier on buff {}.",
                    t.func_id,
                    template.id
                ),
            },
            other => tracing::warn!(
                "Unsupported dynamic_unit_modifier FuncType={other}, FuncId={}, buff {}.",
                t.func_id,
                template.id
            ),
        }
    }
    owner.with_unit(|u| {
        let u = u.unit_mut();
        for bonus in bonuses {
            u.bonuses.add_bonus(index, bonus);
        }
        for d in dynamics {
            u.bonuses.add_dynamic_bonus(index, d);
        }
    });
    if b.charge == 0 {
        let charge = if template.init_min_charge < template.init_max_charge {
            rand::thread_rng().gen_range(template.init_min_charge..template.init_max_charge)
        } else if template.init_min_charge == template.init_max_charge {
            template.init_min_charge
        } else {
            tracing::error!(
                "Buff {}: init_min_charge > init_max_charge (ArgumentOutOfRangeException)",
                template.id
            );
            0
        };
        set_buff(owner, index, |e| e.charge = charge);
    }
    let b = get_buff(owner, index).unwrap_or(b);
    if !b.passive {
        let p = buff_created_packet(server, owner, &b);
        broadcast(server, owner, &p, true);
    }
    if is_nerta_bomb_drop(owner, template.id) {
        combat::set_invisible(server, owner, true);
    }
    // The Rotten Flesh Golem's Slam needs the server to finish its lift and
    // fall. Other character controllers run from the client buff payload.
    let caster = (owner.is_unit()
        && (owner.as_player().is_none() || encounters::server_runs_buff_controller(template.id)))
    .then(|| find_obj(server, b.caster_obj_id))
    .flatten();
    if let Some(caster) = caster {
        start_skill_controller(server, &template, owner, &caster);
    }
    if let Some(p) = owner.as_player() {
        // RadarManager registrations (`systems::services::radar`).
        let radar = &server.systems.services.radar;
        if template.find_school_of_fish_range > 0.0 {
            radar.register_for_fish_school(p, template.find_school_of_fish_range);
        }
        if template.transfer_telescope_range > 0.0 {
            radar.register_for_public_transport(p, template.transfer_telescope_range);
        }
        if template.telescope_range > 0.0 {
            radar.register_for_ships(p, template.telescope_range);
        }
        if p.data()
            .unit
            .buffs
            .check_buff(super::types::buff_constants::DASH)
        {
            super::regen::register_mana_regen(
                server,
                p,
                template.tick,
                template.tick_level_mana_cost,
            );
        }
    }
    super::stats::refresh_cached_stats(server, owner);
}

/// The skill controller part of `BuffTemplate.Start` (owner: a non-character unit; caster: the
/// buff's caster): `SkillControllerId`, else the Gliding lift / Psychokinesis pull fallbacks.
fn start_skill_controller(
    server: &GameServer,
    template: &BuffTemplate,
    owner: &WorldObject,
    caster: &WorldObject,
) {
    use super::controllers;
    let target = Target::Obj(caster.clone());
    let sc = if template.skill_controller_id > 0 {
        let Some(sct) = server
            .game_data
            .systems
            .skills
            .skill_controller(template.skill_controller_id)
        else {
            // The 1.2 data lacks many controllers, the client's own compact.sqlite3 included
            // (see PORTING.md); the startup summary warns, so the per-start skip is debug only.
            tracing::debug!(
                "BuffTemplate.Start: buff {} SC template not found for sc_id={}",
                template.id,
                template.skill_controller_id
            );
            return;
        };
        // Bubbletrap-style buffs set Gliding but leave GlidingLiftHeight = 0: lift 5 m.
        let lift_height = if template.gliding_lift_height > 0.0 {
            template.gliding_lift_height
        } else if template.gliding {
            5.0
        } else {
            0.0
        };
        tracing::debug!(
            "BuffTemplate.Start: buff {} creating SC sc_id={} kind={} for owner={} caster={} psychoSpeed={} liftH={}",
            template.id,
            template.skill_controller_id,
            sct.kind_id,
            owner.obj_id(),
            caster.obj_id(),
            template.psychokinesis_speed,
            lift_height
        );
        match controllers::create(
            server,
            sct,
            owner,
            &target,
            template.psychokinesis_speed,
            lift_height,
            template.gliding_lift_speed,
            template.gliding_lift_duration,
        ) {
            Some(sc) => sc,
            None => {
                // WP-9 (combat-ai analysis, 2026-11-05): Rope (5), Anchor (6) and Flowgraph (8)
                // have no generic controller by design (Rope is handled by the harpoon family,
                // Flowgraph is test skill 23969 only, Anchor two monster skills), so this is an
                // expected debug message. Other unimplemented kinds stay warnings.
                if matches!(sct.kind_id, 5 | 6 | 8) {
                    tracing::debug!(
                        "BuffTemplate.Start: buff {} SC factory returned null for kind={} — controller class not implemented",
                        template.id,
                        sct.kind_id
                    );
                } else {
                    tracing::warn!(
                        "BuffTemplate.Start: buff {} SC factory returned null for kind={} — controller class not implemented",
                        template.id,
                        sct.kind_id
                    );
                }
                return;
            }
        }
    } else if template.gliding {
        let lift_height = if template.gliding_lift_height > 0.0 {
            template.gliding_lift_height
        } else {
            5.0
        };
        let lift_speed = if template.gliding_lift_speed > 0.0 {
            template.gliding_lift_speed
        } else {
            3.0
        };
        controllers::new_floating(
            server,
            None,
            owner,
            &target,
            0.0,
            lift_height,
            lift_speed,
            template.gliding_lift_duration,
        )
    } else if template.psychokinesis && template.psychokinesis_speed > 0.0 {
        controllers::new_floating(
            server,
            None,
            owner,
            &target,
            template.psychokinesis_speed,
            0.0,
            0.0,
            0.0,
        )
    } else {
        return;
    };
    sc.set_source_buff_id(template.id);
    controllers::activate(server, &sc);
}

fn remove_bonuses(owner: &WorldObject, template: &BuffTemplate, index: u32) {
    owner.with_unit(|u| {
        let u = u.unit_mut();
        for t in &template.bonuses {
            u.bonuses.remove_bonus(index, t.attribute);
        }
        for t in &template.dynamic_bonuses {
            u.bonuses.remove_dynamic_bonus(index, t.attribute);
        }
    });
}

/// `SCBuffCreatedPacket(buff)`.
pub fn buff_created_packet(
    server: &GameServer,
    owner: &WorldObject,
    b: &Buff,
) -> SCBuffCreatedPacket {
    let skill_id = buff_skill(server, b)
        .filter(|s| s.template.toggle_buff_id == b.template.id)
        .map_or(0, |s| s.template.id);
    SCBuffCreatedPacket {
        skill_caster: buff_skill_caster(b),
        caster_id: b.caster.map_or(0, |c| c.0),
        owner_obj_id: owner.obj_id(),
        index: b.index,
        buff_id: b.template.id,
        caster_level: b.caster.map_or(1, |c| c.1),
        ab_level: b.ab_level,
        skill_id,
        charge: b.charge,
        duration: b.duration,
        tick: full_template(server, &b.template).tick,
    }
}

/// `BuffTemplate.Dispel(caster, owner, buff, replaced)`.
pub(super) fn dispel(
    server: &GameServer,
    template: &BuffTemplate,
    _caster: Option<&WorldObject>,
    owner: &WorldObject,
    b: &Buff,
    replaced: bool,
) {
    // Stop the controller this buff started. End() may keep it running for the floating
    // controller's fall phase, so the slot is not cleared here (the controller clears itself).
    if let Some(sc) = super::controllers::active(owner) {
        if sc.source_buff_id() == template.id {
            sc.end(server, false);
        }
    }
    remove_bonuses(owner, template, b.index);
    let requiring: Vec<u32> = {
        let data = &server.game_data.systems.skills;
        owner.with_base_unit(|u| {
            u.buffs()
                .effects
                .iter()
                .filter(|e| {
                    data.buff(e.template.id)
                        .is_some_and(|t| t.require_buff_id == template.id)
                })
                .map(|e| e.index)
                .collect()
        })
    };
    for i in requiring {
        exit(server, owner, i, false);
    }
    if !b.passive && !replaced {
        broadcast(
            server,
            owner,
            &SCBuffRemovedPacket {
                obj_id: owner.obj_id(),
                index: b.index,
            },
            true,
        );
    }
    // RadarManager unregistrations (`systems::services::radar`).
    if let Some(p) = owner.as_player() {
        let radar = &server.systems.services.radar;
        if template.find_school_of_fish_range > 0.0 {
            radar.register_for_fish_school(p, 0.0);
        }
        if template.transfer_telescope_range > 0.0 {
            radar.register_for_public_transport(p, 0.0);
        }
        if template.telescope_range > 0.0 {
            radar.register_for_ships(p, 0.0);
        }
    }
}

/// `BuffTemplate.TimeToTimeApply(caster, owner, buff)`.
fn time_to_time_apply(
    server: &GameServer,
    template: &Arc<BuffTemplate>,
    caster: Option<&WorldObject>,
    owner: &WorldObject,
    b: &Buff,
) {
    if template.tick_area_radius > 0.0 {
        do_area_tick(server, template, caster, owner, b);
        return;
    }
    let data = &server.game_data.systems.skills;
    let cast = CastAction::Buff {
        buff_id: template.id,
        owner_obj_id: owner.obj_id(),
        index: b.index,
    };
    for tick in &template.tick_effects {
        if tick.target_buff_tag_id > 0
            && !check_buffs_by_tag(server, owner, tick.target_buff_tag_id)
        {
            continue;
        }
        if tick.target_no_buff_tag_id > 0
            && check_buffs_by_tag(server, owner, tick.target_no_buff_tag_id)
        {
            continue;
        }
        let Some((kind, eff)) = data.effect_by_type_id(tick.effect_id) else {
            continue;
        };
        let skill_object = SkillObject::None;
        let mut source = EffectSource::from_buff(template.clone());
        source.buff_permanent = b.permanent || (b.duration == 0 && b.count == -1);
        source.range_multiplier = Some(b.range_damage_multiplier);
        source.height_multiplier = Some(b.height_damage_multiplier);
        let args = EffectArgs {
            caster: caster.cloned(),
            caster_caster: buff_skill_caster(b),
            target: Target::Obj(owner.clone()),
            target_caster: SkillCastTarget::Unit(owner.obj_id()),
            cast,
            source,
            skill_object: &skill_object,
        };
        effects::apply(server, kind, eff, &args, None);
    }
}
