//! Buff behaviour: the runtime half of `Units/Buffs` (`AddBuff` with tolerance and duration
//! modifiers, `RemoveBuffs`, `TriggerRemoveOn`, `RemoveEffectsOnDeath`, absorption), `Buff`
//! (`ScheduleEffect` / `UpdateEffect` / `Exit` / `StopEffectTask` / `ConsumeCharge`),
//! `BuffTemplate` (`Apply`, `Start`, `TimeToTimeApply`, `DoAreaTick`, `Dispel`), the dispel
//! tasks (`EffectTaskManager` / `DispelTask`), and `BuffTriggersHandler` / `BuffTrigger` /
//! `DamagedBuffTrigger` with the unit events they listen to.
//!
//! The bookkeeping (`models::buff::Buffs`) queues a `BuffEvent` for every effect that enters,
//! is refreshed in or leaves a unit; [`on_buff_event`] (the installed `UnitHooks`) runs the
//! behaviour the C# runs inline at those points.

use std::sync::Arc;

use rand::Rng;

use crate::data::skills::BuffTemplate as NetBuffTemplate;
use crate::models::buff::{effect_state, Buff, BuffEvent, SkillCaster as NetSkillCaster};
use crate::models::unit::{Bonus, BuffAttribute, DynamicBonus, UnitAttribute};
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::data::{BuffTemplate, BuffTriggerTemplate};
use super::effects::{self, EffectArgs, EffectSource};
use super::geometry::Target;
use super::packets::{SCBuffCreatedPacket, SCBuffRemovedPacket, SCBuffUpdatedPacket};
use super::state::SkillRun;
use super::types::{
    tags, BuffEventTriggerKind, BuffRemoveOn, CastAction, DamageType, SkillCastTarget, SkillCaster,
    SkillObject, SkillTargetRelation,
};
use super::{broadcast, combat};

/// The full template of a buff (`SkillManager.GetBuffTemplate`); buffs created from a phase-2
/// subset template that the skills tables lack get a template with the subset's values.
pub fn full_template(server: &GameServer, t: &NetBuffTemplate) -> Arc<BuffTemplate> {
    match server.game_data.systems.skills.buff(t.id) {
        Some(b) => b.clone(),
        None => Arc::new(BuffTemplate {
            id: t.id,
            kind: match t.kind {
                crate::data::skills::BuffKind::Good => 1,
                crate::data::skills::BuffKind::Bad => 2,
                crate::data::skills::BuffKind::Hidden => 3,
                crate::data::skills::BuffKind::Other(k) => k,
            },
            duration: t.duration,
            level_duration: t.level_duration,
            tick: t.tick,
            stack_rule: t.stack_rule,
            max_stack: t.max_stack,
            init_min_charge: t.init_min_charge,
            save_rule_id: t.save_rule_id,
            real_time: t.real_time,
            ..BuffTemplate::default()
        }),
    }
}

/// The phase-2 bookkeeping template for a full template.
pub fn net_template(server: &GameServer, t: &BuffTemplate) -> NetBuffTemplate {
    if let Some(b) = server.game_data.skills.buff(t.id) {
        return b.clone();
    }
    NetBuffTemplate {
        id: t.id,
        kind: crate::data::skills::BuffKind::from(t.kind),
        duration: t.duration,
        level_duration: t.level_duration,
        tick: t.tick,
        stack_rule: t.stack_rule,
        max_stack: t.max_stack,
        init_min_charge: t.init_min_charge,
        save_rule_id: t.save_rule_id,
        real_time: t.real_time,
    }
}

/// `new Buff(owner, caster, skillCaster, template, skill, time)`.
pub fn new_buff(
    server: &GameServer,
    template: &BuffTemplate,
    caster: Option<&WorldObject>,
    skill_caster: &SkillCaster,
    skill: Option<&SkillRun>,
    now: i64,
) -> Buff {
    let caster_info = caster.and_then(|c| {
        // `Caster.Id`: character id, `Npc.Id` = template id, slave / mate db id (read before
        // `with_unit` takes the object lock).
        let db_id = match c {
            WorldObject::Slave(s) => Some(s.data().id),
            WorldObject::Mate(m) => Some(m.data().id),
            _ => None,
        };
        c.with_unit(|u| {
            let id = match c {
                WorldObject::Player(p) => p.character_id,
                WorldObject::Npc(_) => u.unit().template_id,
                _ => db_id.unwrap_or(0),
            };
            (id, u.unit().level)
        })
    });
    let mut b = Buff::new(
        net_template(server, template),
        skill_caster.to_net(),
        caster_info,
        now,
    );
    b.caster_obj_id = caster.map_or(0, WorldObject::obj_id);
    if let Some(s) = skill {
        b.skill_id = s.template.id;
        b.skill_level = s.level;
    }
    if !matches!(skill_caster, SkillCaster::Unit(_)) {
        let mut s = aaemu_commons::PacketStream::new();
        skill_caster.write(&mut s);
        b.skill_caster_bytes = s.into_bytes();
    }
    b
}

/// The full `SkillCaster` of a buff.
pub fn buff_skill_caster(b: &Buff) -> SkillCaster {
    if !b.skill_caster_bytes.is_empty() {
        let mut s = aaemu_commons::PacketStream::from_bytes(b.skill_caster_bytes.clone());
        if let Ok(kind) = s.read_u8() {
            if let Ok(c) = SkillCaster::read(kind, &mut s) {
                return c;
            }
        }
    }
    SkillCaster::from_net(b.skill_caster)
}

/// The `Skill` of a buff (rebuilt from its template id and level).
pub fn buff_skill(server: &GameServer, b: &Buff) -> Option<Arc<SkillRun>> {
    if b.skill_id == 0 {
        return None;
    }
    let t = server.game_data.systems.skills.skill(b.skill_id)?.clone();
    Some(SkillRun::new(t, b.skill_level))
}

pub(crate) fn find_obj(server: &GameServer, obj_id: u32) -> Option<WorldObject> {
    if obj_id == 0 {
        return None;
    }
    server.world.object(obj_id).or_else(|| {
        server
            .world
            .character_by_obj_id(obj_id)
            .map(WorldObject::from)
    })
}

/// The buff with `index` on `owner` (a copy).
pub fn get_buff(owner: &WorldObject, index: u32) -> Option<Buff> {
    owner.with_base_unit(|b| b.buffs().effect_by_index(index).cloned())
}

fn current_buff(owner: &WorldObject, expected: &Buff) -> Option<Buff> {
    get_buff(owner, expected.index).filter(|b| b.same_instance(expected))
}

fn set_buff<R>(owner: &WorldObject, index: u32, f: impl FnOnce(&mut Buff) -> R) -> Option<R> {
    owner.with_base_unit(|b| b.buffs_mut().effect_by_index_mut(index).map(f))
}

/// `Buffs.CheckBuffTag(tagId)`.
pub fn check_buff_tag(server: &GameServer, owner: &WorldObject, tag: u32) -> bool {
    let Some(ids) = server.game_data.systems.skills.buffs_by_tag(tag) else {
        return false;
    };
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| ids.contains(&e.template.id))
    })
}

/// `Buffs.CheckBuffs(SkillManager.GetBuffsByTagId(tag))`.
pub fn check_buffs_by_tag(server: &GameServer, owner: &WorldObject, tag: u32) -> bool {
    let ids = server
        .game_data
        .systems
        .skills
        .buffs_by_tag(tag)
        .unwrap_or(&[]);
    if ids.is_empty() {
        return false;
    }
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| e.template.id > 0 && ids.contains(&e.template.id))
    })
}

/// `Buffs.CheckBuffImmune(buffId)`.
pub fn check_buff_immune(server: &GameServer, owner: &WorldObject, buff_id: u32) -> bool {
    let data = &server.game_data.systems.skills;
    let tags: Vec<u32> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter_map(|e| data.buff(e.template.id).map(|t| t.immune_buff_tag_id))
            .filter(|&t| t != 0)
            .collect()
    });
    tags.into_iter()
        .any(|t| data.buffs_by_tag(t).is_some_and(|l| l.contains(&buff_id)))
}

/// Whether a buff template marks the given damage type as immune.
fn has_immune_flag(t: &BuffTemplate, dt: Option<DamageType>) -> bool {
    match dt {
        Some(DamageType::Melee) => t.melee_immune,
        Some(DamageType::Magic) => t.spell_immune,
        Some(DamageType::Ranged) => t.ranged_immune,
        Some(DamageType::Siege) => t.siege_immune,
        _ => false,
    }
}

/// `Buffs.CheckDamageImmune(damageType)`: full immunity from a flagged buff that has no
/// `immune_damage` threshold (229 real rows). A flagged buff with a positive `immune_damage`
/// only ignores hits up to that amount; see [`damage_immune_threshold`].
pub fn check_damage_immune(
    server: &GameServer,
    owner: &WorldObject,
    dt: Option<DamageType>,
) -> bool {
    let data = &server.game_data.systems.skills;
    let ids: Vec<u32> =
        owner.with_base_unit(|b| b.buffs().effects.iter().map(|e| e.template.id).collect());
    ids.into_iter().any(|id| {
        let Some(t) = data.buff(id) else { return false };
        t.immune_damage <= 0 && has_immune_flag(t, dt)
    })
}

/// The largest `buffs.immune_damage` threshold among the owner's live buffs that flag `dt`.
/// The six threshold buffs (275, 276, 176, 2071, 3615, 4609) ignore only a hit whose final
/// amount is at or below their value, unlike the `immune_damage == 0` full immunity above.
/// The 1.2 client loads the column (`LoadBuffDescs`, dll.c:1579862) and reads it at column
/// `0x42` (dll.c:1580052); their descriptions say "ignore hits of N or less".
pub fn damage_immune_threshold(
    server: &GameServer,
    owner: &WorldObject,
    dt: Option<DamageType>,
) -> Option<i64> {
    let data = &server.game_data.systems.skills;
    let ids: Vec<u32> =
        owner.with_base_unit(|b| b.buffs().effects.iter().map(|e| e.template.id).collect());
    ids.into_iter()
        .filter_map(|id| data.buff(id))
        .filter(|t| t.immune_damage > 0 && has_immune_flag(t, dt))
        .map(|t| i64::from(t.immune_damage))
        .max()
}

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
    owner.with_base_unit(|b| b.buffs_mut().add_buff_with_duration(buff, index, now));
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

// ------------------------------------------------------------------ events

/// `UnitHooks.on_buff_event`: the inline C# behaviour around `_effects` changes.
pub fn on_buff_event(server: &GameServer, owner: &WorldObject, event: &BuffEvent) {
    match event {
        BuffEvent::Started(b) => {
            // An earlier event in the same batch can already have removed this effect.
            if current_buff(owner, b).is_some() {
                on_started(server, owner, b);
                remove_broken_buffs(server, owner);
            }
        }
        BuffEvent::Refreshed {
            buff,
            previous_caster_obj_id,
        } => {
            if current_buff(owner, buff).is_some() {
                on_refreshed(server, owner, buff, *previous_caster_obj_id);
                remove_broken_buffs(server, owner);
            }
        }
        BuffEvent::Removed(b) => on_removed(server, owner, b),
    }
}

/// Reconcile authored breaker tags after an accepted start/refresh, including effects
/// applied by buff timers. Immunity/requirements/tolerance/stack rejection happens before
/// these events. Immediate accepted-state reconciliation is the server compatibility
/// policy. The native loaders establish direction, but the available client/zone dumps
/// do not establish an authoritative consumer's apply-vs-tick timing.
fn remove_broken_buffs(server: &GameServer, owner: &WorldObject) {
    let data = &server.game_data.systems.skills;
    let mut broken: Vec<Buff> = owner.with_base_unit(|u| {
        let effects = &u.buffs().effects;
        effects
            .iter()
            .filter(|b| !b.is_ended())
            .filter(|b| {
                data.buff_breakers.get(&b.template.id).is_some_and(|tags| {
                    effects.iter().any(|other| {
                        other.index != b.index
                            && !other.is_ended()
                            && data
                                .buff_tags(other.template.id)
                                .iter()
                                .any(|t| tags.contains(t))
                    })
                })
            })
            .cloned()
            .collect()
    });
    // Decide from one accepted snapshot. Mutual breakers and duplicate tags must not
    // depend on HashMap order, or on callbacks changing the remaining candidates.
    broken.sort_unstable_by_key(|b| b.index);
    for b in broken {
        // A preceding removal callback may have replaced a later candidate at its index.
        if current_buff(owner, &b).is_none() {
            continue;
        }
        let channel = owner
            .with_unit(|u| u.unit().skill_state.skill_task.clone())
            .flatten()
            .filter(|task| {
                b.caster_obj_id == owner.obj_id()
                    && task.skill.template.id == b.skill_id
                    && (task.skill.template.channeling_buff_id == b.template.id
                        || task.skill.template.toggle_buff_id == b.template.id)
            });
        exit(server, owner, b.index, false);
        // A batch may contain an effect whose Started hook has not run yet. Finish its
        // explicit exit now; the stale Started event above must not revive its controller.
        if current_buff(owner, &b).is_some_and(|e| e.state == effect_state::FINISHING) {
            schedule_effect(server, owner, b.index, false);
        }
        // Remove first: stopping a channel can itself remove its channel/toggle buffs.
        // Only cancel the channel that owns this buff, outside all object/controller locks.
        if let Some(task) = channel {
            // A removal callback can start a different cast of the same skill.
            let still_current = owner
                .with_unit(|u| {
                    u.unit()
                        .skill_state
                        .skill_task
                        .as_ref()
                        .is_some_and(|live| Arc::ptr_eq(&live.skill, &task.skill))
                })
                .unwrap_or(false);
            if still_current {
                if let super::state::SkillTaskKind::EndChanneling { channel_doodad } = task.kind {
                    super::skill::stop(server, &task.skill, owner, channel_doodad);
                }
            }
        }
    }
}

fn on_started(server: &GameServer, owner: &WorldObject, b: &Buff) {
    let data = &server.game_data.systems.skills;
    let template = full_template(server, &b.template);
    // Triggers.SubscribeEvents + Events.OnBuffStarted
    subscribe_triggers(server, owner, b);
    fire_buff_trigger(server, owner, b.index, BuffEventTriggerKind::Started);
    // Started effects run synchronously and can dispel/end this CREATED buff, apply a
    // breaker, or replace it at the same explicit index. Preserve trigger order, but do
    // not install modifiers, unmount, interrupt, or start a controller/timer afterwards.
    let Some(b) = resume_started_buff(server, owner, b) else {
        return;
    };
    if template.id > 0 {
        let skill_mods = data.skill_modifiers_by_owner(template.id).to_vec();
        let buff_mods = data.buff_modifiers_for_buff(template.id).to_vec();
        let combat = data.combat_buffs_for(template.id).to_vec();
        owner.with_unit(|u| {
            let u = u.unit_mut();
            u.skill_modifiers.add_modifiers(&skill_mods);
            u.buff_modifiers.add_modifiers(&buff_mods);
            u.combat_buffs.add_combat_buffs(&combat);
        });
        // A riding character is thrown off its mount (vehicles subsystem).
        if template.stun || template.sleep || template.root {
            if let Some(p) = owner.as_player() {
                crate::systems::vehicles::mates::unmount_riding_character(server, p, false);
            }
        }
        if template.stun || template.silence || template.sleep {
            if resume_started_buff(server, owner, &b).is_none() {
                return;
            }
            super::skill::interrupt_skills(server, owner);
        }
    }
    let Some(b) = resume_started_buff(server, owner, &b) else {
        return;
    };
    // 10.x NpcAggroTable::TauntedBy on buff creation (10x-ONLY; see PORTING.md).
    combat::on_taunt_buff_applied(server, owner, &b);
    if b.duration > 0 || !template.tick_effects.is_empty() {
        // SetInUse(true, false) -> ScheduleEffect(false)
        schedule_effect(server, owner, b.index, false);
    } else {
        set_buff(owner, b.index, |e| {
            e.in_use = true;
            e.state = effect_state::ACTING;
        });
        start(server, owner, b.index);
    }
    super::aura::on_buff_started(server, owner, &b);
    let tags = data.buff_tags(template.id);
    if (tags.contains(&tags::NO_FIGHT) || tags.contains(&tags::RETURNING)) && owner.is_unit() {
        combat::clear_all_aggro(server, owner);
        combat::set_in_battle(server, owner, false);
    }
}

/// Finish an explicit exit deferred while the buff was CREATED. The existing queued
/// CREATED removal path still runs Started triggers before reaching this boundary.
fn resume_started_buff(server: &GameServer, owner: &WorldObject, b: &Buff) -> Option<Buff> {
    let live = current_buff(owner, b)?;
    if live.is_ended() {
        if live.state == effect_state::FINISHING {
            schedule_effect(server, owner, live.index, false);
        }
        return None;
    }
    Some(live)
}

fn on_refreshed(server: &GameServer, owner: &WorldObject, b: &Buff, previous_caster_obj_id: u32) {
    // SetInUse(true, true) -> UpdateEffect
    let template = full_template(server, &b.template);
    start(server, owner, b.index);
    if !current_buff(owner, b).is_some_and(|live| !live.is_ended()) {
        return;
    }
    // Re-stamp the taunt on refresh (TAUNT-1: the 10x refresh path is unknown); a refresh by a
    // different caster also releases the previous caster's stamp.
    combat::on_taunt_buff_refreshed(server, owner, b, previous_caster_obj_id);
    let now = now_ms();
    let Some(gen) = set_buff(owner, b.index, |e| {
        if e.duration == 0 && !e.permanent {
            e.duration = template.get_duration(e.ab_level);
        }
        e.tick = template.tick;
        if e.tick > 0.0 {
            let time = e.time_left(now);
            e.count = if time > 0.0 {
                (time / e.tick + 0.5 + 1.0) as i32
            } else {
                -1
            };
        }
        e.task_gen = super::state::next_task_gen();
        (e.task_gen, e.tick, e.time_left(now))
    }) else {
        return;
    };
    let (gen, tick, time_left) = gen;
    // GetTimeLeft=-1 denotes a permanent buff, not an immediate expiry. Keep
    // the new generation above to invalidate old tasks; only timed/ticking
    // effects need another task after refresh.
    if tick > 0.0 || time_left >= 0.0 {
        schedule_dispel_task(
            server,
            owner,
            b.index,
            gen,
            if tick > 0.0 { tick } else { time_left },
        );
    }
}

fn remove_modifiers(server: &GameServer, owner: &WorldObject, buff_id: u32) {
    let data = &server.game_data.systems.skills;
    let skill_mods = data.skill_modifiers_by_owner(buff_id).to_vec();
    let buff_mods = data.buff_modifiers_for_buff(buff_id).to_vec();
    let combat = data.combat_buffs_for(buff_id).to_vec();
    owner.with_unit(|u| {
        let u = u.unit_mut();
        u.skill_modifiers.remove_modifiers(&skill_mods);
        u.buff_modifiers.remove_modifiers(&buff_mods);
        u.combat_buffs.remove_combat_buffs(&combat);
    });
}

fn on_removed(server: &GameServer, owner: &WorldObject, b: &Buff) {
    // An aura / crowd source unregisters and strips what it applied before the rest of the
    // removal behaviour runs (AURA-3 / AURA-6 / CROWD-4).
    super::aura::on_buff_removed(server, owner, b);
    let template = full_template(server, &b.template);
    let caster = find_obj(server, b.caster_obj_id);
    if b.state == effect_state::FINISHED {
        // `Buff.StopEffectTask` -> `Buffs.RemoveEffect(buff)` -> `Template.Dispel(replace)`.
        remove_modifiers(server, owner, template.id);
        if template.gliding {
            trigger_remove_on(server, owner, BuffRemoveOn::Land, 0);
        }
        dispel(server, &template, caster.as_ref(), owner, b, b.exit_replace);
        restore_faction(server, owner, &template);
    } else {
        // Explicit removal fires Dispelled through unsubscribe, never Timeout.
        dispel(server, &template, caster.as_ref(), owner, b, false);
        unsubscribe_triggers(server, owner, b);
        restore_faction(server, owner, &template);
        remove_modifiers(server, owner, template.id);
    }
    if template.taunt {
        // 10x CancelTauntedBy, after Dispelled has run (10x-ONLY; see PORTING.md).
        combat::cancel_taunted_by(server, owner, b.caster_obj_id);
    }
    super::stats::refresh_cached_stats(server, owner);
}

fn restore_faction(server: &GameServer, owner: &WorldObject, template: &BuffTemplate) {
    if template.faction_id == 0 || !owner.is_unit() {
        return;
    }
    if owner.as_npc().is_some() {
        // npc.SetFaction(npc.Template.FactionId): the template faction is the origin faction.
        let origin = owner.with_base_unit(|b| b.faction_id());
        let origin = owner
            .with_unit(|u| u.unit().origin_faction_id)
            .filter(|f| *f != 0)
            .unwrap_or(origin);
        combat::set_faction(server, owner, origin);
    } else {
        // owner.SetFaction(saveFactions[owner.Id]): never filled in the C# (KeyNotFound).
        tracing::error!(
            "Buff {}: saveFactions has no entry for owner {} (KeyNotFoundException in C#)",
            template.id,
            owner.obj_id()
        );
    }
}

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
fn stop_effect_task(server: &GameServer, owner: &WorldObject, index: u32, timed_out: bool) {
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

fn schedule_dispel_task(
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
    // Buff-driven SkillController for non-character owners (pull / lift / fear / leap / dash);
    // characters move themselves from the buff payload.
    let caster = (owner.is_unit() && owner.as_player().is_none())
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
fn dispel(
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

/// `BuffTemplate.DoAreaTick`.
pub(crate) fn do_area_tick(
    server: &GameServer,
    template: &Arc<BuffTemplate>,
    caster: Option<&WorldObject>,
    owner: &WorldObject,
    b: &Buff,
) {
    let mut units: Vec<WorldObject> = super::geometry::around_radius(
        server,
        &Target::Obj(owner.clone()),
        template.tick_area_radius,
        false,
    )
    .into_iter()
    .filter(WorldObject::is_unit)
    .collect();
    if template.tick_area_exclude_source && owner.is_unit() {
        units.retain(|u| !u.same(owner));
    } else if owner.is_unit() && !units.iter().any(|u| u.same(owner)) {
        units.push(owner.clone());
    }
    let Some(caster) = caster else {
        tracing::error!(
            "DoAreaTick of buff {} without a caster (NullReferenceException)",
            template.id
        );
        return;
    };
    let units = if template.tick_area_relation_id == super::relation::TICK_AREA_HOLDER_ALLIES {
        super::relation::filter_holder_allies(server, owner, caster, units)
    } else {
        let relation = SkillTargetRelation::from_raw(i64::from(template.tick_area_relation_id))
            .unwrap_or(SkillTargetRelation::Any);
        super::relation::filter_with_relation(server, relation, &Target::Obj(caster.clone()), units)
    };
    let data = &server.game_data.systems.skills;
    let cast = CastAction::Buff {
        buff_id: template.id,
        owner_obj_id: owner.obj_id(),
        index: b.index,
    };
    for tick in &template.tick_effects {
        let eff = data.effect_by_type_id(tick.effect_id);
        for trg in &units {
            if tick.target_buff_tag_id > 0
                && !check_buffs_by_tag(server, trg, tick.target_buff_tag_id)
            {
                continue;
            }
            if tick.target_no_buff_tag_id > 0
                && check_buffs_by_tag(server, trg, tick.target_no_buff_tag_id)
            {
                continue;
            }
            let Some((kind, eff)) = eff else {
                tracing::error!(
                    "Tick effect {} of buff {} has no template (NullReferenceException)",
                    tick.effect_id,
                    template.id
                );
                continue;
            };
            let skill_object = SkillObject::None;
            let mut source = EffectSource::from_buff(template.clone());
            source.buff_permanent = b.permanent || (b.duration == 0 && b.count == -1);
            let args = EffectArgs {
                caster: Some(caster.clone()),
                caster_caster: buff_skill_caster(b),
                target: Target::Obj(trg.clone()),
                target_caster: SkillCastTarget::Unit(trg.obj_id()),
                cast,
                source,
                skill_object: &skill_object,
            };
            effects::apply(server, kind, eff, &args, None);
        }
    }
}

// ------------------------------------------------------------------ Buffs removal helpers

/// `Buffs.RemoveBuffs(kind, count, buffTagId)`.
pub fn remove_buffs_by_kind(
    server: &GameServer,
    owner: &WorldObject,
    kind: i32,
    mut count: i32,
    buff_tag_id: u32,
) {
    let data = &server.game_data.systems.skills;
    let tagged = data.buffs_by_tag(buff_tag_id);
    let effects: Vec<(u32, u32)> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .map(|e| (e.index, e.template.id))
            .collect()
    });
    for (index, id) in effects {
        let t = full_template(
            server,
            &NetBuffTemplate::simple(id, crate::data::skills::BuffKind::from(0)),
        );
        if buff_tag_id == 0 && t.system {
            continue;
        }
        if buff_tag_id == 0 && t.kind != kind {
            continue;
        }
        if buff_tag_id > 0 {
            match tagged {
                Some(l) if l.contains(&id) => {}
                Some(_) => continue,
                None => {
                    tracing::error!(
                        "RemoveBuffs: tag {buff_tag_id} has no buffs (NullReferenceException)"
                    );
                    return;
                }
            }
        }
        exit(server, owner, index, false);
        count -= 1;
        if count == 0 {
            return;
        }
    }
}

/// `Buffs.RemoveBuffs(buffTagId, count)`.
pub fn remove_buffs_by_tag(
    server: &GameServer,
    owner: &WorldObject,
    buff_tag_id: u32,
    mut count: i32,
) {
    let Some(ids) = server
        .game_data
        .systems
        .skills
        .buffs_by_tag(buff_tag_id)
        .map(<[u32]>::to_vec)
    else {
        tracing::error!("RemoveBuffs: tag {buff_tag_id} has no buffs (NullReferenceException)");
        return;
    };
    let effects: Vec<(u32, u32)> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .map(|e| (e.index, e.template.id))
            .collect()
    });
    for (index, id) in effects {
        if !ids.contains(&id) {
            continue;
        }
        exit(server, owner, index, false);
        count -= 1;
        if count == 0 {
            return;
        }
    }
}

/// `Buffs.RemoveAllEffects()`.
pub fn remove_all_effects(server: &GameServer, owner: &WorldObject) {
    let effects: Vec<u32> =
        owner.with_base_unit(|b| b.buffs().effects.iter().map(|e| e.index).collect());
    for index in effects {
        exit(server, owner, index, false);
    }
}

/// Takes the unit's resurrection buff (`buffs.resurrection_health` / `resurrection_mana`; the
/// only 1.2 row with either column set is 239 "Nui's Favor"). Returns the restore values and
/// removes the effect ("once").
///
/// The select and the claim are one critical section under the owner's data lock: the effect
/// is moved to `FINISHING` there, so a timer or another removal cannot take it in between.
/// The buff lifecycle (dispel triggers, `StopEffectTask`, the queued `BuffEvent`s) then ends
/// through [`exit`] once the lock is released.
pub fn take_resurrection_buff(
    server: &GameServer,
    owner: &WorldObject,
) -> Option<(i32, i32, bool)> {
    let data = &server.game_data.systems.skills;
    let (index, values) = owner.with_base_unit(|b| {
        let (index, template_id) = b
            .buffs()
            .effects
            .iter()
            .find(|e| {
                !e.is_ended()
                    && data
                        .buff(e.template.id)
                        .is_some_and(|t| t.resurrection_health != 0 || t.resurrection_mana != 0)
            })
            .map(|e| (e.index, e.template.id))?;
        let t = data.buff(template_id)?;
        let values = (
            t.resurrection_health,
            t.resurrection_mana,
            t.resurrection_percent,
        );
        for e in b.buffs_mut().effects.iter_mut() {
            if e.index == index {
                e.state = effect_state::FINISHING;
                break;
            }
        }
        Some((index, values))
    })?;
    exit(server, owner, index, false);
    Some(values)
}

/// `Buffs.RemoveEffectsOnDeath()`.
pub fn remove_effects_on_death(server: &GameServer, owner: &WorldObject) {
    let data = &server.game_data.systems.skills;
    let effects: Vec<(u32, u32)> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .map(|e| (e.index, e.template.id))
            .collect()
    });
    for (index, id) in effects {
        if data.buff(id).is_some_and(|t| t.remove_on_death) {
            exit(server, owner, index, false);
        }
    }
}

/// `Buffs.RemoveStealth()`.
pub fn remove_stealth(server: &GameServer, owner: &WorldObject) {
    let data = &server.game_data.systems.skills;
    let effects: Vec<(u32, u32)> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .map(|e| (e.index, e.template.id))
            .collect()
    });
    for (index, id) in effects {
        if data.buff(id).is_some_and(|t| t.stealth) {
            exit(server, owner, index, false);
        }
    }
}

/// `Buffs.RemoveBuff(buffId)` with the behaviour (first match only).
pub fn remove_buff(server: &GameServer, owner: &WorldObject, buff_id: u32) {
    owner.with_base_unit(|b| b.buffs_mut().remove_buff(buff_id));
    crate::systems::dispatch_buff_events(server, owner);
}

/// `Buffs.RemoveEffect(templateId, skillId)` with the behaviour.
pub fn remove_effect_by_skill(
    server: &GameServer,
    owner: &WorldObject,
    template_id: u32,
    skill_id: u32,
) {
    owner.with_base_unit(|b| b.buffs_mut().remove_effect_by_skill(template_id, skill_id));
    crate::systems::dispatch_buff_events(server, owner);
}

/// The end of a buff that is removed because the skill that applied it ran its course (a
/// completed channel): `Buffs.RemoveEffect(templateId, skillId)` as in the C#, whose
/// `SetInUse(false)` -> `StopEffectTask` always raised `OnTimeout`. The Rust removal
/// ([`remove_effect_by_skill`]) deliberately skips the Timeout triggers of explicit removals
/// (dispels, cancelled channels), so a running buff is ended here the way its own timer ends
/// it: ACTING -> FINISHED, then [`stop_effect_task`] (Timeout, unsubscribe, removal).
///
/// The buff's own timer and the end of the channel can come together (the data pairs a channel
/// with a target buff of the very same length: skill 14054 and buff 1728, 5 s each), and the
/// Timeout must run once, so taking the buff from ACTING is a claim: the loser leaves the buff
/// alone (removing it would also take the trigger from the winner, which is still to run it).
pub fn finish_effect_by_skill(
    server: &GameServer,
    owner: &WorldObject,
    template_id: u32,
    skill_id: u32,
) {
    let indices: Vec<u32> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| e.template.id == template_id && e.skill_id == skill_id)
            .map(|e| e.index)
            .collect()
    });
    for index in indices {
        let claim = set_buff(owner, index, |e| match e.state {
            effect_state::ACTING => {
                e.state = effect_state::FINISHED;
                e.in_use = false;
                e.exit_replace = false;
                Claim::Won
            }
            effect_state::CREATED => Claim::NotStarted,
            _ => Claim::Taken,
        });
        match claim {
            Some(Claim::Won) => stop_effect_task(server, owner, index, true),
            Some(Claim::NotStarted) => {
                fire_buff_trigger(server, owner, index, BuffEventTriggerKind::Timeout);
                owner.with_base_unit(|b| b.buffs_mut().remove_effect_by_index(index));
                crate::systems::dispatch_buff_events(server, owner);
            }
            // Already ending (its timer, or an explicit removal): that path finishes it.
            Some(Claim::Taken) | None => {}
        }
    }
}

/// Who ends a buff at the end of its channel ([`finish_effect_by_skill`]).
enum Claim {
    /// The buff was running: this caller ends it.
    Won,
    /// The buff has not started yet (its first schedule step is still to come).
    NotStarted,
    /// The buff is already being ended by someone else.
    Taken,
}

/// The stealth state the 1.2 client caches per unit (`ClientBuffMan` +4 / +8, recomputed by
/// `ClientBuffMan::UpdateCache` and its per-buff callback, x2game `FUN_39387e00` /
/// `FUN_3989f230`): `stealthed` and `level` from the active `buffs.stealth` effects, and
/// `anti_stealth` from the active `buffs.anti_stealth` effects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StealthState {
    /// Any active effect whose template has `buffs.stealth`.
    pub stealthed: bool,
    /// Highest `ab_level` among those effects (the client's `stealth_level` = the instance
    /// word at +0x2E, the `sourceAbLevel` of `SCBuffCreated`).
    pub level: u32,
    /// Any active effect whose template has `buffs.anti_stealth` (the client's "cannot hide"
    /// byte at +8 that `CanDetect` reads).
    pub anti_stealth: bool,
}

/// The cached stealth state of `owner` (see [`StealthState`]).
pub fn stealth_state(server: &GameServer, owner: &WorldObject) -> StealthState {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        let mut state = StealthState::default();
        for e in &b.buffs().effects {
            let Some(t) = data.buff(e.template.id) else {
                continue;
            };
            if t.stealth {
                state.stealthed = true;
                // The client stores `sourceAbLevel` as a 16-bit word (+0x2E).
                state.level = state.level.max(u32::from(e.ab_level as u16));
            }
            if t.anti_stealth {
                state.anti_stealth = true;
            }
        }
        state
    })
}

/// Whether `owner` carries an active buff whose 1.2 template is a stealth effect
/// (`buffs.stealth`). The 1.2 client hides a unit by this column (`ClientUnit::ApplyStealth` /
/// `CanDetect`, x2game `FUN_3935c270` / `FUN_3935a2b0`), not by buff tag 100 (`TagsEnum.Stealth`);
/// 30 of the 71 stealth buffs lack tag 100 and 10 tag-100 buffs are not stealth.
pub fn has_stealth(server: &GameServer, owner: &WorldObject) -> bool {
    stealth_state(server, owner).stealthed
}

/// Whether `owner` carries an active `buffs.detect_stealth` effect. The column's rows say the
/// holder detects stealth ("stealth detection", 2232 / 2233), but the 1.2 client's `CanDetect`
/// does not read it; the server applies it as a reveal (see PORTING.md).
pub fn has_detect_stealth(server: &GameServer, owner: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| data.buff(e.template.id).is_some_and(|t| t.detect_stealth))
    })
}

/// `Buffs.TriggerRemoveOn(on, value)`.
pub fn trigger_remove_on(server: &GameServer, owner: &WorldObject, on: BuffRemoveOn, value: u32) {
    trigger_remove_on_inner(server, owner, on, value, false)
}

/// `Buffs.TriggerRemoveOn` from a skill whose template keeps stealth (`skills.keep_stealth`):
/// the skill-start / skill-use triggers do not remove the owner's active stealth buffs.
/// Without this a stealth skill (10082 "Stealth", 10496 "Shadow Strike", the stealth potions)
/// removed the stealth buff it was meant to keep through its own `cancel_ongoing_buffs` loop.
pub fn trigger_remove_on_keep_stealth(
    server: &GameServer,
    owner: &WorldObject,
    on: BuffRemoveOn,
    value: u32,
) {
    trigger_remove_on_inner(server, owner, on, value, true)
}

fn trigger_remove_on_inner(
    server: &GameServer,
    owner: &WorldObject,
    on: BuffRemoveOn,
    value: u32,
    keep_stealth: bool,
) {
    let data = &server.game_data.systems.skills;
    let effects: Vec<(u32, u32, u32)> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .map(|e| (e.index, e.template.id, e.caster_obj_id))
            .collect()
    });
    use BuffRemoveOn as R;
    for (index, id, caster) in effects {
        let Some(t) = data.buff(id) else { continue };
        let exit_now = (t.remove_on_attack_buff_trigger && on == R::AttackBuffTrigger)
            || (t.remove_on_attacked_buff_trigger && on == R::AttackedBuffTrigger)
            || (t.remove_on_attacked_etc && on == R::AttackedEtc)
            || (t.remove_on_attacked_etc_dot && on == R::AttackedEtcDot)
            || (t.remove_on_attacked_spell_dot && on == R::AttackedSpellDot)
            || (t.remove_on_attack_etc && on == R::AttackEtc)
            || (t.remove_on_attack_etc_dot && on == R::AttackEtcDot)
            || (t.remove_on_attack_spell_dot && on == R::AttackSpellDot)
            || (t.remove_on_auto_attack && on == R::AutoAttack)
            || (t.remove_on_damage_buff_trigger && on == R::DamageBuffTrigger)
            || (t.remove_on_damaged_buff_trigger && on == R::DamagedBuffTrigger)
            || (t.remove_on_damaged_etc && on == R::DamagedEtc)
            || (t.remove_on_damaged_etc_dot && on == R::DamagedEtcDot)
            || (t.remove_on_damaged_spell_dot && on == R::DamagedSpellDot)
            || (t.remove_on_damage_etc && on == R::DamageEtc)
            || (t.remove_on_damage_etc_dot && on == R::DamageEtcDot)
            || (t.remove_on_damage_spell_dot && on == R::DamageSpellDot)
            || (t.remove_on_death && on == R::Death)
            || (t.remove_on_exempt && on == R::Exempt)
            || (t.remove_on_interaction && on == R::Interaction)
            || (t.remove_on_land && on == R::Land)
            || (t.remove_on_mount && on == R::Mount)
            || (t.remove_on_move && on == R::Move)
            || (t.remove_on_source_dead && on == R::SourceDead && value == caster);
        if exit_now {
            let event = match on {
                R::Move => Some(BuffEventTriggerKind::RemoveOnMove),
                R::DamagedEtc
                | R::DamagedEtcDot
                | R::DamagedSpellDot
                | R::DamagedBuffTrigger
                | R::AttackedEtc => Some(BuffEventTriggerKind::RemoveOnDamage),
                _ => None,
            };
            if let Some(event) = event {
                fire_buff_trigger(server, owner, index, event);
            }
            exit(server, owner, index, false);
            continue;
        }
        // `skills.keep_stealth`: the skill keeps the caster's stealth buffs through its own
        // start/use cancel loop (only these two triggers; attacking and damage still break
        // stealth through the buff's own remove flags).
        if keep_stealth && t.stealth && matches!(on, R::StartSkill | R::UseSkill) {
            continue;
        }
        if t.remove_on_start_skill && on == R::StartSkill {
            if value == 0 || !data.buff_tags(id).contains(&value) {
                exit(server, owner, index, false);
            }
            continue;
        }
        if (t.remove_on_unmount && on == R::Unmount) || (t.remove_on_use_skill && on == R::UseSkill)
        {
            exit(server, owner, index, false);
        }
    }
}

/// `Buffs.GetAbsorptionEffects()` indices.
pub fn absorption_effects(server: &GameServer, owner: &WorldObject) -> Vec<u32> {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| {
                data.buff(e.template.id)
                    .is_some_and(|t| t.damage_absorption_type_id > 0)
            })
            .map(|e| e.index)
            .collect()
    })
}

/// One live reflection buff of an owner whose `reflection_type_id` is the incoming hit's
/// `DamageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionBuff {
    pub index: u32,
    pub buff_id: u32,
    /// `reflection_chance` (percent, 0-100).
    pub chance: i32,
    /// `reflection_target_ratio` (percent of the received damage returned to the attacker).
    pub target_ratio: i32,
}

/// The owner's live reflection buffs that match an incoming hit, in buff order.
///
/// `reflection_type_id` is the `DamageType` of the hit to reflect (1 Melee, 2 Magic, 3 Siege,
/// 4 Ranged; verified against the 1.2 rows and their tooltips). `reflection_chance` is the
/// percent chance to reflect and `reflection_target_ratio` the percent of the received damage
/// that is returned; `reflection_ratio` has no settled meaning and is not read here
/// (REFLECT-1, capture-pending; see `PORTING.md`).
pub fn reflection_effects(
    server: &GameServer,
    owner: &WorldObject,
    damage_type: DamageType,
) -> Vec<ReflectionBuff> {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| !e.is_ended())
            .filter_map(|e| {
                let t = data.buff(e.template.id)?;
                (t.reflection_chance > 0 && t.reflection_type_id == damage_type as u32).then_some(
                    ReflectionBuff {
                        index: e.index,
                        buff_id: t.id,
                        chance: t.reflection_chance,
                        target_ratio: t.reflection_target_ratio,
                    },
                )
            })
            .collect()
    })
}

// ------------------------------------------------------------------ triggers

/// One subscribed `BuffTrigger` (`BuffTriggersHandler._triggers` entry).
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerSub {
    /// The buff owner whose unit events are listened to, 0 for buff events.
    pub source_obj: u32,
    pub owner_obj: u32,
    pub buff_index: u32,
    /// Internal buff lifetime, since an explicit index can be reused by a callback.
    pub buff_instance_id: u64,
    pub template: BuffTriggerTemplate,
    /// `DamagedBuffTrigger` (else the generic `BuffTrigger`).
    pub damaged: bool,
    /// `DamageBuffTrigger` (event 3, damage dealt by the owner): it carries the event amount
    /// like `damaged`, but targets the owner as the C# trigger does.
    pub damage: bool,
}

/// `BuffTriggersHandler.SubscribeEvents`.
fn subscribe_triggers(server: &GameServer, owner: &WorldObject, b: &Buff) {
    let data = &server.game_data.systems.skills;
    let templates = data.buff_triggers_for(b.template.id);
    if templates.is_empty() {
        return;
    }
    // `BuffTrigger.Execute` only warns and returns when the owner is not a Unit (a doodad hit by
    // an area skill), so such a trigger can never run: do not subscribe it.
    if !owner.is_unit() {
        tracing::trace!(
            "Buff {} on non-unit owner {}: triggers not subscribed",
            b.template.id,
            owner.obj_id()
        );
        return;
    }
    let mut subs = server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned");
    use BuffEventTriggerKind as K;
    for t in templates {
        let kind = K::from_raw(i64::from(t.kind));
        let (unit_event, damaged, damage) = match kind {
            Some(K::Damage) => (true, false, true),
            Some(
                K::Attack
                | K::Attacked
                | K::Death
                | K::Landing
                | K::ChannelingCancel
                | K::Unmount
                | K::Kill
                | K::KillAny
                | K::DamagedCollision
                | K::Immotality
                | K::Time,
            ) => (true, false, false),
            Some(
                K::Damaged | K::DamagedMelee | K::DamagedRanged | K::DamagedSpell | K::DamagedSiege,
            ) => (true, true, false),
            Some(K::Dispelled | K::Timeout | K::Started | K::RemoveOnMove | K::RemoveOnDamage) => {
                (false, false, false)
            }
            _ => {
                tracing::trace!("Unimplemented BuffTrigger[\"{:?}\"]", kind);
                continue;
            }
        };
        subs.push(TriggerSub {
            source_obj: if unit_event { owner.obj_id() } else { 0 },
            owner_obj: owner.obj_id(),
            buff_index: b.index,
            buff_instance_id: b.instance_id,
            template: t.clone(),
            damaged,
            damage,
        });
    }
}

/// `BuffTriggersHandler.UnsubscribeEvents` (fires `OnDispelled` first).
fn unsubscribe_triggers(server: &GameServer, owner: &WorldObject, b: &Buff) {
    fire_buff_trigger_with(server, owner, b, BuffEventTriggerKind::Dispelled);
    let owner_id = owner.obj_id();
    server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned")
        .retain(|s| {
            !(s.owner_obj == owner_id
                && s.buff_index == b.index
                && s.buff_instance_id == b.instance_id)
        });
}

/// Fires a buff-level event (`Started`, `Timeout`, `Dispelled`) of the buff at `index`.
fn fire_buff_trigger(
    server: &GameServer,
    owner: &WorldObject,
    index: u32,
    kind: BuffEventTriggerKind,
) {
    if let Some(b) = get_buff(owner, index) {
        fire_buff_trigger_with(server, owner, &b, kind);
    }
}

fn fire_buff_trigger_with(
    server: &GameServer,
    owner: &WorldObject,
    b: &Buff,
    kind: BuffEventTriggerKind,
) {
    let owner_id = owner.obj_id();
    let subs: Vec<TriggerSub> = server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned")
        .iter()
        .filter(|s| {
            s.owner_obj == owner_id
                && s.buff_index == b.index
                && s.buff_instance_id == b.instance_id
                && s.source_obj == 0
                && s.template.kind == kind as u16
        })
        .cloned()
        .collect();
    for s in subs {
        execute_trigger(server, owner, b, &s, 0);
    }
}

/// Fires a unit event (`OnAttack`, `OnDamage`, `OnDamaged*`, `OnDeath`) of `unit` for every
/// trigger subscribed to it.
pub fn fire_unit_event(server: &GameServer, unit: u32, kind: BuffEventTriggerKind, amount: i32) {
    let subs: Vec<TriggerSub> = server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned")
        .iter()
        .filter(|s| s.source_obj == unit && s.template.kind == kind as u16)
        .cloned()
        .collect();
    for s in subs {
        let Some(owner) = find_obj(server, s.owner_obj) else {
            continue;
        };
        let Some(b) = get_buff(&owner, s.buff_index) else {
            continue;
        };
        if b.instance_id != s.buff_instance_id {
            continue;
        }
        execute_trigger(server, &owner, &b, &s, amount);
    }
}

/// `BuffTrigger.Execute` / `DamagedBuffTrigger.Execute`.
fn execute_trigger(
    server: &GameServer,
    owner: &WorldObject,
    b: &Buff,
    s: &TriggerSub,
    amount: i32,
) {
    if !owner.is_unit() {
        tracing::warn!(
            "Owner is not a Unit (buff {}, owner obj {})",
            b.template.id,
            owner.obj_id()
        );
        return;
    }
    let data = &server.game_data.systems.skills;
    let Some(effect) = data.effect(s.template.effect.0, s.template.effect.1) else {
        return;
    };
    let t = &s.template;
    let caster = find_obj(server, b.caster_obj_id);
    let mut target = Some(owner.clone());
    let mut source = Some(owner.clone());
    if t.use_original_source {
        source = caster.clone();
    }
    if t.effect_on_source {
        target = source.clone();
    }
    let Some(target) = target else {
        return;
    };
    if t.target_buff_tag_id != 0 && !check_buff_tag(server, &target, t.target_buff_tag_id) {
        return;
    }
    if t.target_no_buff_tag_id != 0 && check_buff_tag(server, &target, t.target_no_buff_tag_id) {
        return;
    }
    let template = full_template(server, &b.template);
    let skill_object = SkillObject::None;
    // `DamagedBuffTrigger` and `DamageBuffTrigger` both apply the effect from the owner with
    // the buff template as the source and the event's damage amount. The C# `DamageBuffTrigger`
    // uses `args.Attacker`, which `OnDamageArgs` fills with the damage caster itself, so the
    // target stays the owner here too.
    let (effect_caster, source_info) = if s.damaged || s.damage {
        (
            Some(owner.clone()),
            EffectSource {
                skill: None,
                buff: Some(template.clone()),
                amount,
                is_trigger: true,
                buff_permanent: false,
                use_damage_amount: t.use_damage_amount,
                always_hit: false,
                source_item: None,
                source_item_debited: false,
            },
        )
    } else {
        (
            source,
            EffectSource {
                skill: buff_skill(server, b),
                buff: None,
                amount: 0,
                is_trigger: false,
                buff_permanent: false,
                use_damage_amount: t.use_damage_amount,
                always_hit: false,
                source_item: None,
                source_item_debited: false,
            },
        )
    };
    let args = EffectArgs {
        caster: effect_caster,
        caster_caster: SkillCaster::Unit(owner.obj_id()),
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: CastAction::Buff {
            buff_id: template.id,
            owner_obj_id: owner.obj_id(),
            index: b.index,
        },
        source: source_info,
        skill_object: &skill_object,
    };
    effects::apply(server, s.template.effect.0, effect, &args, None);
}

/// `NetSkillCaster` of a unit (`new SkillCasterUnit(objId)`).
pub fn unit_caster(obj_id: u32) -> NetSkillCaster {
    NetSkillCaster { kind: 0, obj_id }
}

/// `UnitAttribute` helper re-export for callers that build bonuses.
pub type Attr = UnitAttribute;

#[cfg(test)]
#[path = "buff_removal_tests.rs"]
mod buff_removal_tests;

#[cfg(test)]
#[path = "buff_breaker_tests.rs"]
mod buff_breaker_tests;

#[cfg(test)]
#[path = "buff_transform_tests.rs"]
mod buff_transform_tests;
