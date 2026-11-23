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

use crate::data::skills::BuffTemplate as NetBuffTemplate;
use crate::models::buff::{effect_state, Buff, BuffEvent, SkillCaster as NetSkillCaster};
use crate::models::unit::UnitAttribute;
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use self::lifecycle::{dispel, is_nerta_bomb_drop, schedule_dispel_task};
use self::triggers::{fire_buff_trigger, subscribe_triggers, unsubscribe_triggers};
use super::data::BuffTemplate;
use super::effects::{EffectArgs, EffectSource};
use super::geometry::Target;
use super::state::SkillRun;
use super::types::{
    tags, BuffEventTriggerKind, BuffRemoveOn, CastAction, DamageType, SkillCastTarget, SkillCaster,
    SkillObject, SkillTargetRelation,
};
use super::{combat, controllers, effects, geometry, regen, skill, state, stats, tasks, types};

mod application;
mod combat_effects;
mod lifecycle;
mod removal;
mod triggers;

pub use self::application::{
    add_buff, add_buff_by_id, add_buff_by_id_with_charge, apply_template, buff_ab_level,
};
pub use self::combat_effects::{
    absorption_effects, has_detect_stealth, has_stealth, reflection_effects, stealth_state,
    ReflectionBuff, StealthState,
};
pub(super) use self::lifecycle::nerta_bomb_drop_active;
pub use self::lifecycle::{
    buff_created_packet, consume_charge, dispel_task, exit, schedule_effect, start,
};
pub use self::removal::{
    finish_effect_by_skill, remove_all_effects, remove_buff, remove_buffs_by_kind,
    remove_buffs_by_tag, remove_effect_by_skill, remove_effects_on_death, remove_stealth,
    take_resurrection_buff, trigger_remove_on, trigger_remove_on_keep_stealth,
};
pub(super) use self::removal::{remove_on_attack, remove_on_damage, remove_on_source_death};
pub use self::triggers::{fire_unit_event, TriggerSub};

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
    removal::track_source_death(server, owner, b);
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
    if b.caster_obj_id != previous_caster_obj_id {
        removal::untrack_source_death(server, owner, b, previous_caster_obj_id);
        removal::track_source_death(server, owner, b);
    }
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
    removal::untrack_source_death(server, owner, b, b.caster_obj_id);
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
    if is_nerta_bomb_drop(owner, b.template.id) && !nerta_bomb_drop_active(owner) {
        combat::set_invisible(server, owner, false);
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
            source.range_multiplier = Some(b.range_damage_multiplier);
            source.height_multiplier = Some(b.height_damage_multiplier);
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

/// `NetSkillCaster` of a unit (`new SkillCasterUnit(objId)`).
pub fn unit_caster(obj_id: u32) -> NetSkillCaster {
    NetSkillCaster { kind: 0, obj_id }
}

/// `UnitAttribute` helper re-export for callers that build bonuses.
pub type Attr = UnitAttribute;

#[cfg(test)]
mod buff_removal_tests;

#[cfg(test)]
mod buff_breaker_tests;

#[cfg(test)]
mod buff_transform_tests;
