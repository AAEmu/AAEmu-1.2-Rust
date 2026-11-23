use rand::Rng;

use crate::systems::instances::encounters::registry as encounters;
use crate::systems::npcs::npc::spawner::random_element_by_weight;
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster};
use crate::world::{now_ms, WorldObject};

use super::super::framework::{AiCtx, BehaviorKind};
use super::super::params::{AiParams, AiSkill, AiSkillList, SkillList};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::npc;
use super::support::{behavior_check_interval, owner_pos, template_scalars};
use super::targeting::current_target;

/// The reach of the melee base skill (id 2): the `PickSkillAndUseIt` guard distance and the cap
/// of the `MoveInRange` stop distance.
pub(super) const MELEE_ATTACK_RANGE: f32 = 4.0;

/// `Behavior.PickSkillAndUseIt(kind, target, targetDist)`.
pub fn pick_skill_and_use_it(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    kind: SkillUseConditionKind,
    target: &WorldObject,
    target_dist: f32,
) -> u8 {
    // The callers pass the 2D attack-range distance. Flying NPCs fight at altitude, so the
    // skill selection below and the melee skill-2 guard need the 3D distance instead.
    let target_dist = if c.npc.data().can_fly {
        npc::distance_to(c.server, &c.obj, target, true)
    } else {
        target_dist
    };
    let mut res = skill_result::INVALID_SKILL;
    let t = template_scalars(c);
    let mut skills = ai_skills::template_skills(c, kind);
    if !skills.is_empty() {
        skills.retain(|s| {
            !ai_skills::on_cooldown(c, *s)
                && c.server
                    .game_data
                    .systems
                    .skills
                    .skill(*s)
                    .is_some_and(|tpl| {
                        (target_dist >= tpl.min_range as f32 && target_dist <= tpl.max_range as f32)
                            || ai_skills::is_self_target(tpl)
                    })
        });
    }
    if target_dist == 0.0 && kind == SkillUseConditionKind::InIdle {
        // This SkillTargetType.Self & SkillUseConditionKind.InIdle
        if skills.is_empty() {
            return res;
        }
        let skill_self_id = skills[rand::thread_rng().gen_range(0..skills.len())];
        let Some(skill) = ai_skills::new_skill(c, skill_self_id) else {
            return res;
        };
        let mut delay1 = (t.base_skill_delay * 1000.0) as i64;
        if t.base_skill_delay == 0.0 {
            delay1 = rand::thread_rng().gen_range(10000..13000);
        }
        if behavior_check_interval(c, k, 1, delay1) {
            tracing::debug!(
                "PickSkillAndUseIt:UseSelfSkill Owner.ObjId {}, SkillId {skill_self_id}",
                c.npc.obj_id
            );
            res = use_skill(c, k, skill, Some(target), 0.0);
        }
        return res;
    }
    // This SkillUseConditionKind.InCombat
    let mut picked = t.base_skill_id as u32;
    if !skills.is_empty() {
        picked = skills[rand::thread_rng().gen_range(0..skills.len())];
    }
    let Some(skill) = ai_skills::new_skill(c, picked) else {
        // new Skill(null) throws NullReferenceException in the C#.
        tracing::error!(
            "PickSkillAndUseIt: skill {picked} of npc {} has no template",
            c.npc.obj_id
        );
        return res;
    };
    // Deviation from the C#: SetWeaponRange runs before the melee hackfix below. The C# returned
    // TOO_FAR_RANGE first, so after a ranged skill went on cooldown the stale (long) weapon range
    // kept MoveInRange parking the NPC at that range while it only had skill 2 to use.
    set_weapon_range(c, k, &skill);
    // Hackfix for Melee attack. Needs to look at the held weapon (if any) or default to 3m
    if picked == 2 && target_dist > MELEE_ATTACK_RANGE {
        return skill_result::TOO_FAR_RANGE;
    }
    let mut delay2 = (t.base_skill_delay * 1000.0) as i64;
    if t.base_skill_delay == 0.0 {
        delay2 = rand::thread_rng().gen_range(1500..1550);
    }
    if behavior_check_interval(c, k, 2, delay2) {
        tracing::debug!(
            "PickSkillAndUseIt:UseSkill Owner.ObjId {}, SkillId {picked} on Target {}",
            c.npc.obj_id,
            target.obj_id()
        );
        res = use_skill(c, k, skill, Some(target), 0.0);
    }
    res
}

/// `Behavior.UseSkill(skill, target, delay)`.
pub fn use_skill(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    skill: crate::systems::skills::state::SkillRef,
    target: Option<&WorldObject>,
    delay: f32,
) -> u8 {
    let Some(target) = target else {
        return skill_result::NO_TARGET;
    };
    if ai_skills::on_cooldown(c, skill.id) {
        return skill_result::COOLDOWN_TIME;
    }
    let can_fly = c.npc.data().can_fly;
    // Flying NPCs check the template's min / max range in 3D: a victim far below is out of
    // range even when it is directly under the NPC (ground NPCs keep the 2D range).
    let target_dist = npc::distance_to(c.server, &c.obj, target, can_fly);
    if target_dist < skill.template.min_range as f32 {
        return skill_result::TOO_CLOSE_RANGE;
    }
    if target_dist > skill.template.max_range as f32 {
        return skill_result::TOO_FAR_RANGE;
    }
    // These self-target plots project their strikes from the caster's heading.
    let directional = encounters::is_directional_self_skill(skill.id);
    if !ai_skills::is_self_target(&skill.template) || directional {
        if cast_path_blocked(c, &skill.template, target) {
            return skill_result::OBSTACLE;
        }
        // Turn before constructing the aim point and before the skill validates its cone.
        let position = npc::world_pos(c.server, target);
        npc::look_towards(c.server, &c.npc, position, 4);
    }
    c.st(k).next_time_to_delay = delay;
    let caster = SkillCaster::Unit(c.npc.obj_id);
    let mut cast_target = if ai_skills::is_pos_target(&skill.template) {
        let p = crate::systems::skills::geometry::world_pos(
            c.server,
            &crate::systems::skills::geometry::Target::Obj(c.obj.clone()),
        );
        SkillCastTarget::Position {
            obj_id: c.npc.obj_id,
            x: p.x,
            y: p.y,
            z: p.z,
            rot: p.rot_z.to_degrees(),
            obj_id1: 0,
            obj_id2: 0,
        }
    } else {
        SkillCastTarget::Unit(target.obj_id())
    };
    // skill.Callback = OnSkillEnded
    ai_skills::set_skill_ended_callback(c, &skill, k);
    ai_skills::use_skill(c, &skill, caster, &mut cast_target, false)
}

pub(super) fn cast_path_blocked(
    c: &AiCtx<'_>,
    skill: &crate::systems::skills::data::SkillTemplate,
    target: &WorldObject,
) -> bool {
    if !skill.check_obstacle || target.same(&c.obj) {
        return false;
    }
    let from = owner_pos(c);
    let to = npc::world_pos(c.server, target);
    // Caster AI needs a clear casting position, including terrain between the two units.
    // The cast validator can only see the caster's own position for a self/zone plot.
    super::collision::line_of_sight(
        c.server,
        c.obj.transform().instance_id,
        [from.x, from.y, from.z + 1.0],
        [to.x, to.y, to.z + 1.0],
    ) == Some(false)
}

/// `Behavior.OnSkillEnded()` (`_delayEnd = now + _nextTimeToDelay`).
pub fn on_skill_ended(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let s = c.st(k);
    let add = f64::from(s.next_time_to_delay) * 1000.0;
    if add.is_finite() {
        s.delay_end = now_ms() + add as i64;
    }
}

/// `Behavior.SetWeaponRange(skill, target)`.
pub(super) fn set_weapon_range(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    skill: &crate::systems::skills::state::SkillRef,
) {
    let skill_range = crate::systems::skills::skill::apply_skill_modifiers(
        c.server,
        skill,
        &c.obj,
        crate::models::unit::SkillAttribute::Range,
        f64::from(skill.template.max_range),
    );
    let mut min_range_check = f64::from(skill.template.min_range);
    let mut max_range_check = skill_range;
    if skill.template.weapon_slot_for_range_id > 0 {
        // Deviation from the C#: the range comes from the NPC's own weapon. The C# read the
        // target's (the player's) holdable, so the chase range followed the player's weapon.
        let (mut min_w, mut max_w) = (0.0f32, 3.0f32); // Fist default
        if let Some(h) =
            ai_skills::weapon_holdable(c, &c.obj, skill.template.weapon_slot_for_range_id)
        {
            min_w = h.0;
            max_w = h.1;
        }
        min_range_check = f64::from(min_w);
        max_range_check = f64::from(max_w);
    }
    let mut max_range_check = max_range_check as f32;
    // Deviation from the C#: MoveInRange stops at `attack_start_range_scale * max_weapon_range`,
    // but the melee base skill (id 2) is only used within MELEE_ATTACK_RANGE. Cap the chase range
    // so a scale above 1 (or a long-range holdable) cannot park the NPC out of melee reach.
    if skill.id == 2 {
        let scale = template_scalars(c).attack_start_range_scale;
        if scale > 0.0 {
            max_range_check = max_range_check.min(MELEE_ATTACK_RANGE / scale);
        }
    }
    let s = c.st(k);
    s.min_weapon_range = min_range_check as f32;
    s.max_weapon_range = max_range_check;
}

/// `BaseCombatBehavior.CanStrafe`.
pub(super) fn can_strafe(c: &mut AiCtx<'_>, k: BehaviorKind) -> bool {
    let s = c.st(k);
    now_ms() > s.delay_end || s.strafe_during_delay
}

/// `BaseCombatBehavior.IsUsingSkill`.
pub(crate) fn is_using_skill(c: &AiCtx<'_>) -> bool {
    let n = c.npc.data();
    n.unit.skill_state.skill_task.is_some() || n.unit.skill_state.active_plot.is_some()
}

/// `BaseCombatBehavior.CanUseSkill`.
pub(super) fn can_use_skill(c: &mut AiCtx<'_>, k: BehaviorKind) -> bool {
    if is_using_skill(c) {
        return false;
    }
    if npc::skill_controller_running(&c.obj) {
        return false;
    }
    if npc::is_silenced(c.server, &c.obj) {
        return false;
    }
    let now = now_ms();
    let delay_end = c.st(k).delay_end;
    // `!Ai.Owner.IsGlobalCooldownDone` where IsGlobalCooldownDone = GlobalCooldown > now
    let gcd = c.npc.data().unit.global_cooldown_ms;
    now >= delay_end && gcd <= now
}

/// `RequestAvailableAiSkillList` / `RequestAvailableSkillList` (same filter on both list kinds).
pub(super) fn health_and_time_ok(
    health_ratio: i32,
    time_elapsed: f64,
    hmin: i32,
    hmax: i32,
    tstart: f32,
    tend: f32,
) -> bool {
    if !((hmin == 0 && hmax == 0) || (hmin < health_ratio && health_ratio <= hmax)) {
        return false;
    }
    let (ts, te) = (f64::from(tstart), f64::from(tend));
    if (ts >= 0.0 && te > 0.0) || (ts > 0.0 && te >= 0.0) {
        if ts <= time_elapsed && te == 0.0 {
            return true;
        }
        return ts <= time_elapsed && time_elapsed <= te;
    }
    ts == 0.0 && te == 0.0
}

pub(super) fn health_ratio(c: &AiCtx<'_>) -> i32 {
    let (hp, max) = {
        let n = c.npc.data();
        (n.unit.hp, n.unit.max_hp)
    };
    (hp as f32 / max as f32 * 100.0) as i32
}

/// `BaseCombatBehavior.RefreshSkillQueue(skillLists, aiParams)`.
pub(super) fn refresh_skill_queue(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    lists: &[AiSkillList],
    params: &AiParams,
) -> bool {
    let target_dist = match current_target(c) {
        Some(t) => npc::distance_to(c.server, &c.obj, &t, false),
        None => 0.0,
    };
    let hr = health_ratio(c);
    let elapsed = (now_ms() - c.st(k).combat_start_time) as f64 / 1000.0;
    let available: Vec<&AiSkillList> = lists
        .iter()
        .filter(|s| {
            health_and_time_ok(
                hr,
                elapsed,
                s.health_range_min,
                s.health_range_max,
                s.time_range_start,
                s.time_range_end,
            )
        })
        .collect();
    if !available.is_empty() {
        // select a set of skills by dice
        if let Some(selected) = random_element_by_weight(&available, |s| s.dice as f32) {
            let selected = *selected;
            {
                let s = c.st(k);
                s.pipe_name = selected.pipe_name.clone();
                s.phase_type = selected.phase_type;
            }
            params.set_restoration_on_return(selected.restoration);
            params.set_go_return_state(selected.go_return);
            // add startAiSkill first to the queue if it is available
            if !selected.start_ai_skills.is_empty() && !c.st(k).starting_skill_already_used {
                for skill in &selected.start_ai_skills {
                    if ai_skills::on_cooldown(c, skill.skill_id) {
                        continue;
                    }
                    let s = c.st(k);
                    s.skill_queue.push_back(*skill);
                    s.starting_skill_already_used = true;
                }
            }
            let avail_lists: Vec<&SkillList> = selected
                .skill_lists
                .iter()
                .filter(|s| {
                    health_and_time_ok(
                        hr,
                        elapsed,
                        s.health_range_min,
                        s.health_range_max,
                        s.time_range_start,
                        s.time_range_end,
                    )
                })
                .collect();
            let Some(list) = random_element_by_weight(&avail_lists, |s| s.dice as f32) else {
                return !c.st(k).skill_queue.is_empty();
            };
            for skill in &list.skills {
                if ai_skills::on_cooldown(c, skill.skill_id) {
                    continue;
                }
                let Some(tpl) = c.server.game_data.systems.skills.skill(skill.skill_id) else {
                    continue;
                };
                if (target_dist >= tpl.min_range as f32 && target_dist <= tpl.max_range as f32)
                    || ai_skills::is_self_target(tpl)
                {
                    c.st(k).skill_queue.push_back(*skill);
                }
            }
        }
        return !c.st(k).skill_queue.is_empty();
    }
    let t = template_scalars(c);
    if t.base_skill_id == 0 {
        return false;
    }
    c.st(k).skill_queue.push_back(AiSkill {
        skill_id: t.base_skill_id as u32,
        strafe: t.base_skill_strafe,
        delay: t.base_skill_delay,
    });
    true
}
