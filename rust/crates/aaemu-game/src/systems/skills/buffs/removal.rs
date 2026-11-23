use crate::data::skills::BuffTemplate as NetBuffTemplate;
use crate::models::buff::{effect_state, Buff};
use crate::server::GameServer;
use crate::world::WorldObject;

use super::super::effects::EffectArgs;
use super::super::types::{BuffEventTriggerKind, BuffRemoveOn, CastAction, DamageType};
use super::lifecycle::stop_effect_task;
use super::triggers::fire_buff_trigger;
use super::{exit, find_obj, full_template, set_buff};

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

enum DamageProvenance {
    Direct,
    BuffTrigger,
    SpellDot,
    EtcDot,
}

impl DamageProvenance {
    fn of(args: &EffectArgs<'_>, damage_type: Option<DamageType>) -> Self {
        // Generic buff triggers keep the original skill in EffectSource; only damage
        // amount triggers set is_trigger. The Buff cast action distinguishes the former.
        if args.source.is_trigger
            || (matches!(args.cast, CastAction::Buff { .. }) && args.source.buff.is_none())
        {
            Self::BuffTrigger
        } else if args.source.buff.is_some() {
            if damage_type == Some(DamageType::Magic) {
                Self::SpellDot
            } else {
                Self::EtcDot
            }
        } else {
            Self::Direct
        }
    }
}

pub(in crate::systems::skills) fn remove_on_attack(
    server: &GameServer,
    owner: &WorldObject,
    args: &EffectArgs<'_>,
    damage_type: Option<DamageType>,
    incoming: bool,
) {
    use BuffRemoveOn as R;
    let (attack, attacked) = match DamageProvenance::of(args, damage_type) {
        DamageProvenance::Direct => (R::AttackEtc, R::AttackedEtc),
        DamageProvenance::BuffTrigger => (R::AttackBuffTrigger, R::AttackedBuffTrigger),
        DamageProvenance::SpellDot => (R::AttackSpellDot, R::AttackedSpellDot),
        DamageProvenance::EtcDot => (R::AttackEtcDot, R::AttackedEtcDot),
    };
    trigger_remove_on(server, owner, if incoming { attacked } else { attack }, 0);
}

pub(in crate::systems::skills) fn remove_on_damage(
    server: &GameServer,
    owner: &WorldObject,
    args: &EffectArgs<'_>,
    damage_type: Option<DamageType>,
    incoming: bool,
) {
    use BuffRemoveOn as R;
    let (damage, damaged) = match DamageProvenance::of(args, damage_type) {
        DamageProvenance::Direct => (R::DamageEtc, R::DamagedEtc),
        DamageProvenance::BuffTrigger => (R::DamageBuffTrigger, R::DamagedBuffTrigger),
        DamageProvenance::SpellDot => (R::DamageSpellDot, R::DamagedSpellDot),
        DamageProvenance::EtcDot => (R::DamageEtcDot, R::DamagedEtcDot),
    };
    trigger_remove_on(server, owner, if incoming { damaged } else { damage }, 0);
}

pub(super) fn track_source_death(server: &GameServer, owner: &WorldObject, buff: &Buff) {
    if let Some(source) = source_death_caster(server, owner, buff.template.id, buff.caster_obj_id) {
        source.with_unit(|u| {
            u.unit_mut()
                .skill_state
                .source_death_buffs
                .insert(buff.instance_id, owner.obj_id());
        });
    }
}

pub(super) fn untrack_source_death(
    server: &GameServer,
    owner: &WorldObject,
    buff: &Buff,
    caster_id: u32,
) {
    if let Some(source) = source_death_caster(server, owner, buff.template.id, caster_id) {
        source.with_unit(|u| {
            u.unit_mut()
                .skill_state
                .source_death_buffs
                .remove(&buff.instance_id);
        });
    }
}

fn source_death_caster(
    server: &GameServer,
    owner: &WorldObject,
    buff_id: u32,
    caster_id: u32,
) -> Option<WorldObject> {
    if !server
        .game_data
        .systems
        .skills
        .buff(buff_id)
        .is_some_and(|t| t.remove_on_source_dead)
    {
        return None;
    }
    if caster_id == owner.obj_id() {
        Some(owner.clone())
    } else {
        find_obj(server, caster_id)
    }
}

pub(in crate::systems::skills) fn remove_on_source_death(
    server: &GameServer,
    source: &WorldObject,
) {
    // Take the subscriptions before any callback can remove or reapply a buff. The
    // index lives on the caster handle, so recycling its object id cannot inherit it.
    let holders = source
        .with_unit(|u| std::mem::take(&mut u.unit_mut().skill_state.source_death_buffs))
        .unwrap_or_default();
    for (instance_id, owner_id) in holders {
        let owner = if owner_id == source.obj_id() {
            Some(source.clone())
        } else {
            find_obj(server, owner_id)
        };
        if let Some(owner) = owner {
            trigger_remove_on_inner(
                server,
                &owner,
                BuffRemoveOn::SourceDead,
                source.obj_id(),
                false,
                |b| b.instance_id == instance_id && b.caster_obj_id == source.obj_id(),
            );
        }
    }
}

pub(super) fn remove_on_exempt(server: &GameServer, owner: &WorldObject, grant: &Buff) {
    trigger_remove_on_inner(server, owner, BuffRemoveOn::Exempt, 0, false, |b| {
        b.instance_id != grant.instance_id
    });
}

/// `Buffs.TriggerRemoveOn(on, value)`.
pub fn trigger_remove_on(server: &GameServer, owner: &WorldObject, on: BuffRemoveOn, value: u32) {
    trigger_remove_on_inner(server, owner, on, value, false, |_| true)
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
    trigger_remove_on_inner(server, owner, on, value, true, |_| true)
}

fn trigger_remove_on_inner(
    server: &GameServer,
    owner: &WorldObject,
    on: BuffRemoveOn,
    value: u32,
    keep_stealth: bool,
    filter: impl Fn(&Buff) -> bool,
) {
    let data = &server.game_data.systems.skills;
    let effects: Vec<_> = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| filter(e))
            .map(|e| (e.index, e.template.id, e.caster_obj_id, e.instance_id))
            .collect()
    });
    use BuffRemoveOn as R;
    for (index, id, caster, instance_id) in effects {
        let is_current = || {
            super::get_buff(owner, index)
                .is_some_and(|b| b.instance_id == instance_id && filter(&b) && !b.is_ended())
        };
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
        if exit_now && is_current() {
            let event = match on {
                R::Move => Some(BuffEventTriggerKind::RemoveOnMove),
                R::DamagedEtc
                | R::DamagedEtcDot
                | R::DamagedSpellDot
                | R::DamagedBuffTrigger
                | R::AttackedEtc
                | R::AttackedEtcDot
                | R::AttackedSpellDot
                | R::AttackedBuffTrigger => Some(BuffEventTriggerKind::RemoveOnDamage),
                _ => None,
            };
            if let Some(event) = event {
                fire_buff_trigger(server, owner, index, event);
            }
            if is_current() {
                exit(server, owner, index, false);
            }
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
