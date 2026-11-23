use std::sync::Arc;

use crate::models::unit::UnitAttribute;
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::super::data::SkillTemplate;
use super::super::geometry::{distance_to, Target};
use super::super::relation::can_attack;
use super::super::state::{AutoAttackSlot, CancelToken, SkillRef, SkillRun};
use super::super::stats::{equip_slot, slot_type, stat, StatKind};
use super::super::types::{SkillCastTarget, SkillCaster, SkillObject};
use super::{cast_skill_level, rank_inc, use_skill};

/// `SkillManager.GetAttackDelay(template, caster, includeCooldown, additionalDelay)`.
///
/// `level` is the effective skill level `L` the caster runs the skill at (see
/// [`cast_skill_level`]), for the 1.2 `(int)(casting_time * (1 + T))` term of the
/// non-auto-attack branch.
pub fn attack_delay(
    server: &GameServer,
    t: &SkillTemplate,
    level: i32,
    caster: &WorldObject,
    include_cooldown: bool,
    additional: f64,
) -> f64 {
    let gcd_mul = stat(server, caster, StatKind::GlobalCooldownMul);
    if matches!(t.id, 2..=4) && caster.as_player().is_some() {
        let slot = match t.weapon_slot_for_auto_attack_id {
            equip_slot::MAINHAND | equip_slot::OFFHAND | equip_slot::RANGED => {
                t.weapon_slot_for_auto_attack_id
            }
            _ => match t.id {
                2 => equip_slot::MAINHAND,
                3 => equip_slot::OFFHAND,
                _ => equip_slot::RANGED,
            },
        };
        let fallback = if slot == equip_slot::RANGED {
            1800.0
        } else {
            1500.0
        };
        let speed = server
            .systems
            .skills
            .ext
            .items()
            .weapon_holdable(server, caster, slot)
            .filter(|h| h.speed.is_finite() && h.speed > 0.0)
            .map_or(fallback, |h| f64::from(h.speed));
        let attr = if slot == equip_slot::RANGED {
            UnitAttribute::RangedSpeedMul
        } else {
            UnitAttribute::MeleeSpeedMul
        };
        let haste = caster
            .with_unit(|u| u.unit().calculate_with_bonuses(0.0, attr, now_ms()))
            .unwrap_or(0.0);
        // Native 1.2 sub_398B49E0: weapon delay * 1000 / (1000 + haste),
        // guarding the denominator at 1. Offhand uses melee haste, ranged its own.
        // Only this server repeat timer applies haste; the GCD remains once-only.
        return (speed * (gcd_mul / 100.0) * 1000.0 / (1000.0 + haste.max(-999.0)))
            .max(0.0)
            .min(f64::from(i32::MAX));
    }
    let cast_mul = stat(server, caster, StatKind::CastTimeMul);
    let cast =
        f64::from((f64::from(t.casting_time) * (1.0 + rank_inc(t, level))) as i32) * cast_mul;
    let cd = if include_cooldown {
        f64::from(t.cooldown_time) * (gcd_mul / 100.0)
    } else {
        0.0
    };
    cast + cd + additional * (gcd_mul / 100.0)
}

/// `Unit.StartAutoSkill(skill)` (characters only).
pub fn start_auto_skill(server: &GameServer, caster: &WorldObject, skill: &SkillRef) {
    let Some(p) = caster.as_player() else { return };
    if p.data().unit.skill_state.auto_attack.is_some() {
        return;
    }
    let cancel = CancelToken::new();
    p.data().unit.skill_state.auto_attack = Some(AutoAttackSlot {
        skill: skill.clone(),
        cancel: cancel.clone(),
    });
    let level = cast_skill_level(server, skill, caster);
    let delay = attack_delay(server, &skill.template, level, caster, true, 1000.0) as i64;
    let obj_id = p.obj_id;
    let skill = skill.clone();
    if !schedule_auto_attack(server, obj_id, skill, cancel, delay) {
        // No scheduler: clear the slot so the stale auto-attack state does not stay behind.
        let mut c = p.data();
        c.unit.is_auto_attack = false;
        c.unit.skill_state.auto_attack = None;
    }
}

pub(super) fn schedule_auto_attack(
    server: &GameServer,
    obj_id: u32,
    skill: SkillRef,
    cancel: CancelToken,
    delay: i64,
) -> bool {
    super::tasks::schedule(server, delay, move |srv| {
        if cancel.is_cancelled() {
            return;
        }
        let Some(p) = srv.world.character_by_obj_id(obj_id) else {
            return;
        };
        let next = auto_attack_tick(&srv, &p, &skill, &cancel);
        if let Some(next) = next {
            schedule_auto_attack(&srv, obj_id, skill, cancel, next);
        }
    })
}

/// `UseAutoAttackSkillTask.Execute`: `Some(next delay)` while it keeps running.
pub(super) fn auto_attack_tick(
    server: &GameServer,
    p: &crate::world::PlayerRef,
    skill: &SkillRef,
    cancel: &CancelToken,
) -> Option<i64> {
    let owns_slot = p
        .data()
        .unit
        .skill_state
        .auto_attack
        .as_ref()
        .is_some_and(|slot| slot.cancel == *cancel && Arc::ptr_eq(&slot.skill, skill));
    if !owns_slot {
        cancel.cancel();
        return None;
    }
    let me = WorldObject::from(p);
    let (hp, target_id, has_task, gcd) = {
        let c = p.data();
        (
            c.unit.hp,
            c.unit.current_target,
            c.unit.skill_state.skill_task.is_some(),
            c.unit.global_cooldown_ms,
        )
    };
    let target = (target_id != 0)
        .then(|| server.world.object(target_id))
        .flatten()
        .filter(WorldObject::is_unit);
    let target_hp = target
        .as_ref()
        .and_then(|t| t.with_unit(|u| u.unit().hp))
        .unwrap_or(0);
    if hp <= 0
        || target.is_none()
        || target_hp <= 0
        || target_id == p.obj_id
        || cancel.is_cancelled()
    {
        // StopAutoAttack
        cancel.cancel();
        let mut c = p.data();
        c.unit.is_auto_attack = false;
        c.unit.skill_state.auto_attack = None;
        return None;
    }
    let target = target?;
    let level = cast_skill_level(server, skill, &me);
    let delay = attack_delay(server, &skill.template, level, &me, true, 1000.0) as i64;
    if has_task || gcd >= now_ms() {
        return Some(delay);
    }
    let me_t = Target::Obj(me.clone());
    let tt = Target::Obj(target.clone());
    if !can_attack(server, &me_t, &tt) {
        return Some(delay);
    }
    let items = server.systems.skills.ext.items();
    let slot = if skill.template.id == 4 {
        equip_slot::RANGED
    } else {
        equip_slot::MAINHAND
    };
    let max_range = items
        .weapon_holdable(server, &me, slot)
        .filter(|h| h.max_range > 0.0)
        .map(|h| h.max_range)
        .unwrap_or(if skill.template.max_range > 0 {
            skill.template.max_range as f32
        } else {
            3.0
        });
    if distance_to(server, &me_t, &tt, false) > max_range {
        return Some(delay);
    }
    let mut sct = SkillCastTarget::Unit(target.obj_id());
    use_skill(
        server,
        skill,
        &me,
        SkillCaster::Unit(me.obj_id()),
        &mut sct,
        Some(SkillObject::None),
        true,
    );
    if skill.template.id == 2 {
        let off = items.weapon_holdable(server, &me, equip_slot::OFFHAND);
        if let Some(h) = off {
            if h.slot_type_id != slot_type::SHIELD {
                if let Some(t3) = server.game_data.systems.skills.skill(3).cloned() {
                    let off_skill = SkillRun::new(t3, 1);
                    let mut sct = SkillCastTarget::Unit(target.obj_id());
                    use_skill(
                        server,
                        &off_skill,
                        &me,
                        SkillCaster::Unit(me.obj_id()),
                        &mut sct,
                        Some(SkillObject::None),
                        true,
                    );
                }
            }
        }
    }
    Some(delay)
}
