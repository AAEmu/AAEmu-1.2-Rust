//! Item procs (`item_procs`, `item_proc_bindings`, `holdables.item_proc_id`,
//! `equip_item_set_bonuses.proc_id`).
//!
//! The C# loaded the tables but never fired a proc: `UnitProcs.RollProcsForKind` skipped every
//! proc whose cooldown had elapsed (and `LastProc` started at `DateTime.MinValue`), the binding
//! and holdable sources were never read, and the roll passed at `rate + 1` percent. This module
//! implements the intended behaviour; the deviations are recorded in `PORTING.md`.
//!
//! Decisions that the 1.2 server data does not settle are marked capture-pending:
//! * the chance roll is `rng in [0, 100) < chance_rate`, i.e. `chance_rate` is a percentage;
//! * `item_level_based_chance_bonus` is not applied (`CAPTURE-PENDING item-procs-1`);
//! * a proc cannot fire again until `cooldown_sec` after it last fired; the cooldown is kept per
//!   unit by proc id and survives re-equipping the same proc in the session (not persisted);
//! * the proc skill is cast by the owner on itself (75 of the 92 1.2 proc skills are
//!   self-targeted), bypassing the GCD; a non-self proc skill targets the other unit of the
//!   damage event (`CAPTURE-PENDING item-procs-2`);
//! * kinds 17-20 (heal / skill triggers) have no 1.2 data and are not rolled.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use rand::Rng;

use crate::data::items::ItemData;
use crate::models::item::{Item, ItemProc};
use crate::server::GameServer;
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::types::{
    skill_result, DamageType, SkillCastTarget, SkillCaster, SkillHitType, SkillTargetType,
};
use crate::world::{now_ms, WorldObject};

/// `ProcChanceKind` (`item_procs.chance_kind_id`).
pub mod chance_kind {
    /// 1: any damage dealt.
    pub const HIT_ANY: u32 = 1;
    /// 2: melee damage dealt.
    pub const HIT_MELEE: u32 = 2;
    /// 3: melee critical damage dealt.
    pub const HIT_MELEE_CRIT: u32 = 3;
    /// 4: magic damage dealt.
    pub const HIT_SPELL: u32 = 4;
    /// 5: magic critical damage dealt.
    pub const HIT_SPELL_CRIT: u32 = 5;
    /// 6: ranged damage dealt.
    pub const HIT_RANGED: u32 = 6;
    /// 7: ranged critical damage dealt.
    pub const HIT_RANGED_CRIT: u32 = 7;
    /// 8: siege damage dealt.
    pub const HIT_SIEGE: u32 = 8;
    /// 9: any damage taken.
    pub const TAKE_DAMAGE_ANY: u32 = 9;
    /// 10: melee damage taken.
    pub const TAKE_DAMAGE_MELEE: u32 = 10;
    /// 11: melee critical damage taken.
    pub const TAKE_DAMAGE_MELEE_CRIT: u32 = 11;
    /// 12: magic damage taken.
    pub const TAKE_DAMAGE_SPELL: u32 = 12;
    /// 13: magic critical damage taken.
    pub const TAKE_DAMAGE_SPELL_CRIT: u32 = 13;
    /// 14: ranged damage taken.
    pub const TAKE_DAMAGE_RANGED: u32 = 14;
    /// 15: ranged critical damage taken.
    pub const TAKE_DAMAGE_RANGED_CRIT: u32 = 15;
    /// 16: siege damage taken.
    pub const TAKE_DAMAGE_SIEGE: u32 = 16;
}

thread_local! {
    /// A proc skill is being cast on this thread: the damage it deals in the same call must not
    /// roll procs again. This covers only the synchronous part of `use_skill`; a proc skill's
    /// projectile / `effect_delay` / `effect_speed` / plot effects and the DoTs it applies run
    /// later, when the guard is gone, and can roll procs again (bounded by the item-proc and
    /// skill cooldowns; **CAPTURE-PENDING item-procs-4**).
    static PROC_CASTING: Cell<bool> = const { Cell::new(false) };
}

/// Set while a proc skill's synchronous effects run; their damage skips proc rolls. A proc
/// skill's own hit therefore does not roll the victim's take-damage procs either
/// (**CAPTURE-PENDING item-procs-5**).
struct ProcCastGuard;

impl ProcCastGuard {
    fn enter() -> Option<Self> {
        PROC_CASTING.with(|c| {
            if c.get() {
                None
            } else {
                c.set(true);
                Some(Self)
            }
        })
    }
}

impl Drop for ProcCastGuard {
    fn drop(&mut self) {
        PROC_CASTING.with(|c| c.set(false));
    }
}

/// Whether the current stack is running a proc skill's effects.
pub fn proc_casting() -> bool {
    PROC_CASTING.with(Cell::get)
}

/// The item procs of a unit (`UnitProcs`): the active proc ids (deduplicated) indexed by
/// `ProcChanceKind`, plus the last fire time of every proc the unit has fired this session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitProcs {
    /// Active procs in registration order (sets first, then item bindings, then holdables).
    procs: Vec<ItemProc>,
    /// Chance kind -> proc ids (the [`ItemProc`] instances live in `procs`).
    by_kind: HashMap<u32, Vec<u32>>,
    /// `LastProc` by proc id. Kept when a proc is removed so re-equipping it does not reset the
    /// cooldown in the same session (not persisted).
    last_fired_ms: HashMap<u32, i64>,
}

impl UnitProcs {
    /// Replaces the active proc set. Proc ids with no `item_procs` row are skipped (a bad data
    /// row must not disable the rest of the set), duplicates are dropped. A proc that stays
    /// active keeps its cooldown from `last_fired_ms`.
    pub fn set_active(&mut self, data: &ItemData, ids: impl IntoIterator<Item = u32>) {
        self.procs.clear();
        self.by_kind.clear();
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id) {
                continue;
            }
            let Some(template) = data.item_proc(id) else {
                tracing::debug!("item proc {id} has no item_procs row");
                continue;
            };
            let mut proc = ItemProc::new(id);
            proc.last_proc_ms = self.last_fired_ms.get(&id).copied().unwrap_or(0);
            self.procs.push(proc);
            self.by_kind
                .entry(template.chance_kind)
                .or_default()
                .push(id);
        }
    }

    /// Whether the unit has no active proc (the common case; lets the damage path skip the
    /// per-kind rolls).
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

    /// The active proc template ids, in registration order.
    pub fn active_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.procs.iter().map(|p| p.template_id)
    }

    /// Whether `proc_id` is in the active set.
    pub fn is_active(&self, proc_id: u32) -> bool {
        self.procs.iter().any(|p| p.template_id == proc_id)
    }

    /// The active proc ids that roll on `kind`.
    pub fn procs_for_kind(&self, kind: u32) -> Vec<u32> {
        self.by_kind.get(&kind).cloned().unwrap_or_default()
    }

    /// `ItemProc.Apply` with the cooldown bookkeeping of `UnitProcs.RollProcsForKind`:
    /// check the cooldown, draw `roll`, and stamp `last_proc_ms` only when it fires. Returns the
    /// proc skill and the previous `last_proc_ms` (so a refused cast can put the cooldown back).
    /// The check and the stamp happen under the same lock (the caller holds the unit's data lock).
    pub fn try_fire(
        &mut self,
        data: &ItemData,
        proc_id: u32,
        now_ms: i64,
        roll: impl FnOnce() -> i32,
    ) -> Option<(u32, i64)> {
        let proc = self.procs.iter_mut().find(|p| p.template_id == proc_id)?;
        let previous = proc.last_proc_ms;
        let skill_id = proc.apply(data, now_ms, roll)?;
        proc.last_proc_ms = now_ms;
        self.last_fired_ms.insert(proc_id, now_ms);
        Some((skill_id, previous))
    }

    /// Puts a cooldown stamp back after the proc skill was refused.
    pub fn restore_last_fired(&mut self, proc_id: u32, previous_ms: i64) {
        if let Some(proc) = self.procs.iter_mut().find(|p| p.template_id == proc_id) {
            proc.last_proc_ms = previous_ms;
        }
        if previous_ms > 0 {
            self.last_fired_ms.insert(proc_id, previous_ms);
        } else {
            self.last_fired_ms.remove(&proc_id);
        }
    }
}

/// The active proc ids of a unit's equipped items:
/// * `equip_item_set_bonuses.proc_id` when the piece count reaches `num_pieces` (destroyed
///   pieces count, as the existing set-bonus buffs do);
/// * `item_proc_bindings` of every equipped, non-destroyed item;
/// * `holdables.item_proc_id` of every equipped, non-destroyed item with a holdable.
pub fn active_ids_for(server: &GameServer, items: &[Item]) -> Vec<u32> {
    let data = &server.game_data.items;
    let mut ids = Vec::new();
    let mut set_pieces: Vec<(u32, i32)> = Vec::new();
    for item in items {
        let Some(template) = data.template(item.template_id) else {
            continue;
        };
        let Some(equip) = template.equip() else {
            continue;
        };
        if equip.equip_item_set_id == 0 {
            continue;
        }
        match set_pieces
            .iter_mut()
            .find(|(set, _)| *set == equip.equip_item_set_id)
        {
            Some((_, count)) => *count += 1,
            None => set_pieces.push((equip.equip_item_set_id, 1)),
        }
    }
    for (set_id, count) in set_pieces {
        let Some(set) = data.equip_item_set(set_id) else {
            // The set-bonus buff pass logs the unknown set.
            continue;
        };
        for bonus in &set.bonuses {
            if count >= bonus.num_pieces && bonus.item_proc_id != 0 {
                ids.push(bonus.item_proc_id);
            }
        }
    }
    for item in items {
        if !item.is_not_destroyed(data) {
            continue;
        }
        ids.extend_from_slice(data.item_proc_bindings(item.template_id));
        let Some(template) = data.template(item.template_id) else {
            continue;
        };
        if let Some(holdable) = data.holdable_of(template) {
            if holdable.item_proc_id > 0 {
                ids.push(holdable.item_proc_id as u32);
            }
        }
    }
    ids
}

/// Replaces the unit's active procs from its equipped items. Called from the gear-bonus pass
/// (`Unit.UpdateGearBonuses`), which every equipment change, repair, gear break and login runs.
pub fn refresh(server: &GameServer, unit: &WorldObject, items: &[Item]) {
    let ids = active_ids_for(server, items);
    unit.with_unit(|u| {
        u.unit_mut()
            .item_procs
            .set_active(&server.game_data.items, ids)
    });
}

/// Rolls every active proc of `kind` on `unit`. `other` is the other unit of the damage event
/// (the victim for attacker kinds, the attacker for victim kinds); a non-self proc skill is cast
/// on it. `now` is injectable for the cooldown tests.
pub fn roll_for_kind_with(
    server: &GameServer,
    unit: &WorldObject,
    kind: u32,
    other: &WorldObject,
    now: i64,
) {
    if proc_casting() {
        return;
    }
    let ids = unit
        .with_unit(|u| u.unit().item_procs.procs_for_kind(kind))
        .unwrap_or_default();
    for proc_id in ids {
        let Some(template) = server.game_data.items.item_proc(proc_id).copied() else {
            continue;
        };
        // `chance_rate` is a percentage: fire for rng in [0, 100) < rate.
        let roll = rand::thread_rng().gen_range(0..100i32);
        let fired = unit.with_unit(|u| {
            u.unit_mut()
                .item_procs
                .try_fire(&server.game_data.items, proc_id, now, || roll)
        });
        let Some(Some((skill_id, previous))) = fired else {
            continue;
        };
        if !cast_proc_skill(server, unit, other, skill_id, template.skill_id) {
            unit.with_unit(|u| {
                u.unit_mut()
                    .item_procs
                    .restore_last_fired(proc_id, previous)
            });
        }
    }
}

/// [`roll_for_kind_with`] at the current time.
pub fn roll_for_kind(server: &GameServer, unit: &WorldObject, kind: u32, other: &WorldObject) {
    roll_for_kind_with(server, unit, kind, other, now_ms());
}

/// Rolls the attacker's kinds 1-8 and the victim's kinds 9-16 for a landed damage hit.
/// Misses and immune hits return before this is called. A proc skill's own damage is skipped
/// by [`proc_casting`].
pub fn roll_damage(
    server: &GameServer,
    attacker: &WorldObject,
    victim: &WorldObject,
    damage_type: Option<DamageType>,
    hit_type: SkillHitType,
) {
    if proc_casting() {
        return;
    }
    let has_procs = |o: &WorldObject| {
        o.with_unit(|u| !u.unit().item_procs.is_empty())
            .unwrap_or(false)
    };
    if attacker.is_unit() && has_procs(attacker) {
        for kind in attacker_kinds(damage_type, hit_type) {
            roll_for_kind_with(server, attacker, kind, victim, now_ms());
        }
    }
    if victim.is_unit() && has_procs(victim) {
        for kind in victim_kinds(damage_type, hit_type) {
            roll_for_kind_with(server, victim, kind, attacker, now_ms());
        }
    }
}

/// The attacker (`Hit*`) kinds of a landed hit. `HIT_ANY` always applies; the typed kinds map
/// the categories the Rust combat distinguishes (`DamageType` + `SkillHitType`).
fn attacker_kinds(damage_type: Option<DamageType>, hit_type: SkillHitType) -> Vec<u32> {
    use chance_kind as k;
    let mut kinds = vec![k::HIT_ANY];
    match damage_type {
        Some(DamageType::Melee) => {
            kinds.push(k::HIT_MELEE);
            if hit_type == SkillHitType::MeleeCritical {
                kinds.push(k::HIT_MELEE_CRIT);
            }
        }
        Some(DamageType::Magic) => {
            kinds.push(k::HIT_SPELL);
            if hit_type == SkillHitType::SpellCritical {
                kinds.push(k::HIT_SPELL_CRIT);
            }
        }
        Some(DamageType::Ranged) => {
            kinds.push(k::HIT_RANGED);
            if hit_type == SkillHitType::RangedCritical {
                kinds.push(k::HIT_RANGED_CRIT);
            }
        }
        Some(DamageType::Siege) => kinds.push(k::HIT_SIEGE),
        Some(DamageType::Heal) | None => {}
    }
    kinds
}

/// The victim (`TakeDamage*`) kinds of a landed hit.
fn victim_kinds(damage_type: Option<DamageType>, hit_type: SkillHitType) -> Vec<u32> {
    use chance_kind as k;
    let mut kinds = vec![k::TAKE_DAMAGE_ANY];
    match damage_type {
        Some(DamageType::Melee) => {
            kinds.push(k::TAKE_DAMAGE_MELEE);
            if hit_type == SkillHitType::MeleeCritical {
                kinds.push(k::TAKE_DAMAGE_MELEE_CRIT);
            }
        }
        Some(DamageType::Magic) => {
            kinds.push(k::TAKE_DAMAGE_SPELL);
            if hit_type == SkillHitType::SpellCritical {
                kinds.push(k::TAKE_DAMAGE_SPELL_CRIT);
            }
        }
        Some(DamageType::Ranged) => {
            kinds.push(k::TAKE_DAMAGE_RANGED);
            if hit_type == SkillHitType::RangedCritical {
                kinds.push(k::TAKE_DAMAGE_RANGED_CRIT);
            }
        }
        Some(DamageType::Siege) => kinds.push(k::TAKE_DAMAGE_SIEGE),
        Some(DamageType::Heal) | None => {}
    }
    kinds
}

/// `new Skill(template).Use(owner, SkillCasterUnit(owner), target, null, bypassGcd)` of
/// `ItemProc.Apply`. The C# used `SkillCasterUnit(owner)` / `SkillCastTargetDoodad(owner)` and
/// did not bypass the GCD. The orchestrator decision (see `PORTING.md`) casts the proc
/// instantly: the cloned template has `casting_time` / `mana_cost` zeroed (two 1.2 proc
/// skills, 14921 and 14922, carry a 2.0 / 2.67 s cast time; 14921 is also the one proc skill
/// with a nonzero mana cost, 100). No proc skill has a reagent row, but the rest of
/// `use_skill` runs unchanged: the cast is refused while the owner is casting or channeling
/// (and can stop a channel for a skill with `stop_channeling_on_start_skill`), it resets the
/// owner's `skill_cancelled` / `condition_chance`, a unit requirement failure warns per hit and
/// an NPC `NO_TARGET` pokes its AI (**CAPTURE-PENDING item-procs-3**).
fn cast_proc_skill(
    server: &GameServer,
    owner: &WorldObject,
    other: &WorldObject,
    skill_id: u32,
    template_skill_id: u32,
) -> bool {
    let Some(template) = server.game_data.systems.skills.skill(skill_id).cloned() else {
        tracing::warn!(
            "item proc skill {skill_id} (item_procs.skill_id {template_skill_id}) has no template"
        );
        return false;
    };
    let mut template = (*template).clone();
    template.casting_time = 0;
    template.mana_cost = 0;
    let self_targeted = SkillTargetType::from_raw(i64::from(template.target_type))
        == Some(SkillTargetType::SelfTarget);
    // CAPTURE-PENDING item-procs-2: the 1.2 server target for a non-self proc skill is unknown;
    // the orchestrator decision is the other unit of the damage event. In the 1.2 data 75 proc
    // skills are self-targeted, 15 hostile and 2 any-unit.
    let target_id = if self_targeted {
        owner.obj_id()
    } else {
        other.obj_id()
    };
    let Some(_guard) = ProcCastGuard::enter() else {
        return false;
    };
    let skill = SkillRun::new(std::sync::Arc::new(template), 1);
    let mut target = SkillCastTarget::Unit(target_id);
    let (result, _) = crate::systems::skills::skill::use_skill(
        server,
        &skill,
        owner,
        SkillCaster::Unit(owner.obj_id()),
        &mut target,
        None,
        true,
    );
    result == skill_result::SUCCESS
}
