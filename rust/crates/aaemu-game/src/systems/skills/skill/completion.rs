use std::sync::Arc;

use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::WorldObject;

use super::super::packets::{
    SCCastingStoppedPacket, SCSkillCooldownResetPacket, SCSkillEndedPacket, SCSkillStoppedPacket,
};
use super::super::relation::owner_character;
use super::super::state::{DirectCastEnd, SkillRef, SkillRun, SkillTaskKind};
use super::super::types::SkillCaster;
use super::super::{broadcast, buffs};
use super::cooldowns::cooldown_exempt;
use super::end_channeling;

/// `Skill.EndSkill(caster)`.
pub fn end_skill(server: &GameServer, skill: &SkillRef, caster: &WorldObject) {
    // The cast (completed, cancelled or interrupted) no longer holds a `once_one_*` doodad.
    // A seat occupation of the same unit keeps its own bond flag. Only a use that took the hold
    // visits the occupancy map: every other skill ends here, combat skills included.
    let (tl, generation, target, holds_doodad) = {
        let mut state = skill.state();
        if state.tl_id == 0 || state.direct_cast_end != DirectCastEnd::Pending {
            return;
        }
        state.direct_cast_end = DirectCastEnd::Sending;
        (
            state.tl_id,
            state.generation,
            state.initial_target,
            std::mem::take(&mut state.holds_doodad),
        )
    };
    if holds_doodad && target != 0 {
        crate::systems::npcs::doodad::occupancy::end_cast(server, target, caster);
    }
    let t = &skill.template;
    if let Some(p) = caster.as_player() {
        let cancelled = skill.cancelled();
        let cost = super::validation::labor_cost(skill, p);
        let have = i32::from(p.data().labor_power);
        if cost > 0 && !cancelled && have >= cost {
            super::progress::change_labor(server, p, -(cost as i16), t.actability_group_id);
        }
        if t.gain_life_point > 0 && !cancelled {
            let rate = server.systems.skills.config().vocation_rate;
            super::progress::change_game_points(
                server,
                p,
                super::progress::GAME_POINT_VOCATION,
                (rate * f64::from(t.gain_life_point)).ceil() as i32,
            );
        }
    }
    if caster.is_unit() {
        broadcast(server, caster, &SCSkillEndedPacket { tl }, true);
    }
    // Cast and plot packets share this timeline. The last part to send its end also
    // completes the skill callback and returns the id to the pool.
    let released = {
        let mut state = skill.state();
        if state.generation == generation && state.tl_id == tl {
            state.direct_cast_end = DirectCastEnd::Sent;
            if state.active_plot.is_none() {
                std::mem::take(&mut state.tl_id)
            } else {
                0
            }
        } else {
            0
        }
    };
    if released != 0 {
        skill.invoke_callback();
        server.systems.skills.tl_ids.release(released);
    }
    if !caster.is_unit() {
        return;
    }
    reset_cooldown_if_ignored(server, skill, caster);
    // USE_SKILL: one completed (not cancelled) cast.
    if let Some(p) = caster.as_player() {
        if !skill.cancelled() {
            crate::systems::report_achievement(
                server,
                p,
                AchievementInput::Increment {
                    kind_id: char_record_kind::USE_SKILL,
                    amount: 1,
                    matcher: RecordMatch::Value1(t.id),
                },
            );
        }
    }
}

/// A `plot_only` skill commits its GCD and cooldown when it is accepted. A plot cancelled
/// while it is still in its casting phase (before any damage or other effect landed) restores the
/// caster's cooldown state from before the use (so the shared tag cooldown returns to what it was
/// rather than being wiped) and tells the client.
pub(in crate::systems::skills) fn refund_cancelled_plot_cooldown(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
) {
    let Some(snap) = skill.state().plot_cooldown_snapshot.take() else {
        return;
    };
    let id = skill.template.id;
    let restored = caster.with_unit(|u| {
        let u = u.unit_mut();
        let restore = |map: &mut std::collections::HashMap<u32, i64>,
                       key: u32,
                       prev: Option<i64>| match prev {
            Some(end) => {
                map.insert(key, end);
            }
            None => {
                map.remove(&key);
            }
        };
        restore(&mut u.cooldowns.cooldowns, id, snap.skill_cooldown_end);
        if snap.tag_id != 0 {
            restore(
                &mut u.cooldowns.tag_cooldowns,
                snap.tag_id,
                snap.tag_cooldown_end,
            );
        }
        u.global_cooldown_ms = snap.global_cooldown_ms;
    });
    if restored.is_none() {
        return;
    }
    if let Some(o) = owner_character(server, caster) {
        for gcd in [false, true] {
            o.send(SCSkillCooldownResetPacket {
                obj_id: caster.obj_id(),
                skill_id: id,
                tag_id: if gcd { 0 } else { snap.tag_id },
                gcd,
            });
        }
    }
}

pub(in crate::systems::skills) fn reset_cooldown_if_ignored(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
) {
    if cooldown_exempt(caster) {
        return;
    }
    if let Some(o) = owner_character(server, caster) {
        if o.data().skill_char.ignore_skill_cooldowns {
            let tag = skill.template.cooldown_tag_id.max(0) as u32;
            caster.with_unit(|u| {
                u.unit_mut()
                    .cooldowns
                    .remove_skill_cooldown(skill.template.id, tag);
            });
            o.send(SCSkillCooldownResetPacket {
                obj_id: caster.obj_id(),
                skill_id: skill.template.id,
                tag_id: tag,
                gcd: false,
            });
        }
    }
}

/// `Skill.Stop(caster, channelDoodad, casterCaster)`.
pub fn stop(server: &GameServer, skill: &SkillRef, caster: &WorldObject, channel_doodad: u32) {
    if !caster.is_unit() || skill.tl_id() == 0 {
        return;
    }
    let t = &skill.template;
    skill.state().cancelled = true;
    let task = caster
        .with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            if slot
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
            {
                slot.take()
            } else {
                None
            }
        })
        .flatten();
    if let Some(task) = task {
        task.cancel.cancel();
    }
    let tl = skill.tl_id();
    broadcast(
        server,
        caster,
        &SCCastingStoppedPacket { tl, duration: 0 },
        true,
    );
    if t.toggle_buff_id != 0 {
        buffs::remove_effect_by_skill(server, caster, t.toggle_buff_id, t.id);
    }
    if t.channeling_time > 0 {
        buffs::fire_unit_event(
            server,
            caster.obj_id(),
            super::types::BuffEventTriggerKind::ChannelingCancel,
            0,
        );
        end_channeling(
            server,
            skill,
            caster,
            channel_doodad,
            &SkillCaster::Unit(caster.obj_id()),
            false,
        );
    } else {
        end_skill(server, skill, caster);
    }
}

/// `Skill.StopSkill(caster)` (base melee / ranged skills).
pub fn stop_skill(server: &GameServer, skill: &SkillRef, caster: &WorldObject) {
    if !caster.is_unit() {
        return;
    }
    if let Some(a) = caster
        .with_unit(|u| u.unit().skill_state.auto_attack.clone())
        .flatten()
    {
        a.cancel.cancel();
    }
    // Take-and-clear atomically (as `end_skill` does): a concurrent plot end or skill end then
    // releases 0, a no-op, instead of releasing the same id twice.
    let tl = skill.take_tl_id();
    broadcast(server, caster, &SCSkillEndedPacket { tl }, true);
    broadcast(
        server,
        caster,
        &SCSkillStoppedPacket {
            obj_id: caster.obj_id(),
            skill_id: skill.id,
        },
        true,
    );
    server.systems.skills.tl_ids.release(tl);
}

/// `Unit.InterruptSkills()`.
pub fn interrupt_skills(server: &GameServer, unit: &WorldObject) {
    let Some((plot, task)) = unit.with_unit(|u| {
        let s = &u.unit().skill_state;
        (s.active_plot.clone(), s.skill_task.clone())
    }) else {
        return;
    };
    if let Some(p) = plot {
        p.0.request_cancellation();
    }
    let Some(task) = task else {
        return;
    };
    match task.kind {
        SkillTaskKind::EndChanneling { channel_doodad } => {
            stop(server, &task.skill, unit, channel_doodad)
        }
        SkillTaskKind::Cast => stop(server, &task.skill, unit, 0),
    }
}
