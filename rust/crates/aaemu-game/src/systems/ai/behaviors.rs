//! `Models/Game/AI/v2/Framework/Behavior` (skill picking, aggression / alert checks, aggro
//! help), `Behaviors/BaseCombatBehavior` (`MoveInRange`, `UpdateTarget`, `ShouldReturn`,
//! `RefreshSkillQueue`, ...) and every concrete behavior of `Behaviors/**` (Common, Archer,
//! BigMonster, Flytrap, WildBoar).
//!
//! Each C# behavior method is a function taking the [`AiCtx`] and the behavior's
//! [`BehaviorKind`] (its instance); the instance fields live in [`BehaviorSlot`] and are only
//! borrowed for single statements, so nested `Ai.GoToX()` calls behave like the C# ones.

use std::sync::Arc;

use rand::Rng;

use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::packets::world::SCUnitPointsPacket;
use crate::server::GameServer;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::npcs::npc::packets::{
    SCAggroTargetChangedPacket, SCNpcChatMessagePacket, SCUnitModelPostureChangedPacket,
};
use crate::systems::npcs::npc::spawner::random_element_by_weight;
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::systems::skills::state::AggroKind;
use crate::systems::skills::types::{skill_result, KillReason, SkillCastTarget, SkillCaster};
use crate::systems::skills::{buffs, combat as skills_combat};
use crate::world::{now_ms, WorldObject};

use super::combat::{self, add_unit_aggro, can_attack, clear_aggro_of_unit, set_in_battle};
use super::framework::{AiCtx, ArcherPhase, BehaviorKind, ChasePath, NpcIntervalSite};
use super::npc::{self, MoveAi};
use super::params::{AiParamType, AiParams, AiParamsKind, AiSkill, AiSkillList, SkillList};
use super::path_handler::{AiPathPoint, AiPathPointAction, PathStep, TARGET_STALL_TIMEOUT_MS};
use super::skills::{self as ai_skills, SkillUseConditionKind};

use BehaviorKind as B;

// ===========================================================================================
// dispatch
// ===========================================================================================

/// `behavior.Enter()`.
pub fn enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // A chase route never survives a state change into or out of combat: its start and end
    // belong to the previous fight (the victim may stand in the very same spot again).
    if matches!(
        k,
        B::AlmightyAttack
            | B::ArcherAttack
            | B::Attack
            | B::BigMonsterAttack
            | B::FlytrapAttack
            | B::WildBoarAttack
            | B::Dead
            | B::ReturnState
    ) {
        reset_chase_path(c);
    }
    match k {
        B::Alert => alert_enter(c, k),
        B::AlmightyAttack => almighty_attack_enter(c, k),
        B::Attack => attack_enter(c, k),
        B::Dead => dead_enter(c, k),
        B::Despawning => despawning_enter(c),
        B::DoNothing | B::Dummy => {
            npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
            npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
        }
        B::FollowPath => follow_path_enter(c, k),
        B::FollowUnit => {
            npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
            npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
            c.st(k).enter = true;
        }
        B::HoldPosition => hold_position_enter(c, k),
        B::Idle => idle_enter(c, k),
        B::ReturnState => return_state_enter(c, k),
        B::Roaming => {
            ai_skills::interrupt_skills(c);
            npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
            npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
            c.st(k).enter = true;
        }
        B::RunCommandSet => {
            npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
            npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
        }
        B::Spawning => spawning_enter(c, k),
        B::Talk => talk_enter(c, k),
        B::ArcherAttack => archer_attack_enter(c, k),
        B::BigMonsterAttack => big_monster_attack_enter(c, k),
        B::FlytrapAlert => flytrap_alert_enter(c, k),
        B::FlytrapAttack => flytrap_attack_enter(c, k),
        B::WildBoarAttack => wild_boar_attack_enter(c, k),
    }
}

/// `behavior.Tick(delta)` (`delta_ms` = the AI manager's tick delta).
pub fn tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    match k {
        B::Alert => alert_tick(c, k),
        B::AlmightyAttack => almighty_attack_tick(c, k, delta_ms),
        B::Attack => attack_tick(c, k, delta_ms),
        B::Dead => {
            if c.st(k).enter && c.npc.data().unit.hp == 0 {
                c.ai.already_targeted = false;
            }
        }
        B::Despawning | B::DoNothing | B::Dummy => {}
        B::FollowPath => follow_path_tick(c, k, delta_ms),
        B::FollowUnit => follow_unit_tick(c, k, delta_ms),
        B::HoldPosition => hold_position_tick(c, k, delta_ms),
        B::Idle => idle_tick(c, k, delta_ms),
        B::ReturnState => return_state_tick(c, k, delta_ms),
        B::Roaming => roaming_tick(c, k, delta_ms),
        B::RunCommandSet => run_command_set_tick(c, delta_ms),
        B::Spawning => spawning_tick(c, k),
        B::Talk => talk_tick(c, k),
        B::ArcherAttack => archer_attack_tick(c, k, delta_ms),
        B::BigMonsterAttack => big_monster_attack_tick(c, k, delta_ms),
        B::FlytrapAlert => {
            if c.st(k).enter {
                check_aggression(c, k);
            }
        }
        B::FlytrapAttack => flytrap_attack_tick(c, k, delta_ms),
        B::WildBoarAttack => wild_boar_attack_tick(c, k, delta_ms),
    }
}

/// `behavior.Exit()`.
pub fn exit(c: &mut AiCtx<'_>, k: BehaviorKind) {
    match k {
        B::Alert => {
            let posture = posture_packet(c, true);
            npc::broadcast_packet(c, &posture);
            c.st(k).enter = false;
        }
        B::AlmightyAttack => almighty_attack_exit(c, k),
        B::ReturnState => {
            let posture = posture_packet(c, true);
            npc::broadcast_packet(c, &posture);
            buffs::remove_buff(c.server, &c.obj, npc::BUFF_NPC_RETURN);
            c.st(k).enter = false;
        }
        B::HoldPosition => {
            let s = c.st(k);
            if s.initialized {
                s.initialized = false;
                s.following_npc = false;
            }
        }
        B::Idle | B::Talk => {
            let s = c.st(k);
            if s.initialized {
                s.initialized = false;
            }
        }
        B::Despawning | B::DoNothing | B::Dummy | B::RunCommandSet => {}
        _ => c.st(k).enter = false,
    }
}

// ===========================================================================================
// shared helpers (Behavior / BaseCombatBehavior)
// ===========================================================================================

fn posture_packet(c: &AiCtx<'_>, activate: bool) -> SCUnitModelPostureChangedPacket {
    let n = c.npc.data();
    SCUnitModelPostureChangedPacket {
        obj_id: c.npc.obj_id,
        posture: crate::models::unit::UnitData::model_posture_type(&*n),
        has_loot_left: n.unit.has_loot_left,
        anim_action_id: n.anim_action_id,
        activate_animation: activate,
        turret_pitch: 0.0,
        turret_yaw: 0.0,
    }
}

/// `Ai.Owner.BroadcastPacket(new SCUnitModelPostureChangedPacket(Ai.Owner, AnimActionId, x), false)`.
fn broadcast_posture(c: &AiCtx<'_>, activate: bool) {
    let p = posture_packet(c, activate);
    npc::broadcast_packet(c, &p);
}

/// `this.CheckInterval(interval)` at call site `site` of behavior instance `k`.
fn behavior_check_interval(c: &mut AiCtx<'_>, k: BehaviorKind, site: u8, interval_ms: i64) -> bool {
    let now = now_ms();
    let s = c.st(k);
    let next = s.intervals.get(&site).copied().unwrap_or(now);
    if next > now {
        return false;
    }
    s.intervals.insert(site, now + interval_ms);
    true
}

/// `Behavior.Throttle(interval = 0.1)`.
fn throttle(c: &mut AiCtx<'_>, k: BehaviorKind) -> bool {
    let now = now_ms();
    let s = c.st(k);
    if let Some(last) = s.last_tick {
        if now - last < 100 {
            return false;
        }
    }
    s.last_tick = Some(now);
    true
}

fn template_scalars(c: &AiCtx<'_>) -> TemplateScalars {
    match npc::template(c.server, &c.npc) {
        Some(t) => TemplateScalars {
            aggression: t.aggression,
            attack_start_range_scale: t.attack_start_range_scale,
            sight_range_scale: t.sight_range_scale,
            sight_fov_scale: t.sight_fov_scale,
            base_skill_id: t.base_skill_id,
            base_skill_delay: t.base_skill_delay,
            base_skill_strafe: t.base_skill_strafe,
            use_range_mod: t.use_range_mod,
            skills_count: t.skills.len(),
            return_distance: t.return_distance,
            absolute_return_distance: t.absolute_return_distance,
            npc_ai_param_id: t.npc_ai_param_id,
            npc_nickname_id: t.npc_nickname_id,
            scale: t.scale,
        },
        None => TemplateScalars::default(),
    }
}

/// The `NpcTemplate` values the behaviors read.
#[derive(Debug, Clone, Copy, Default)]
struct TemplateScalars {
    aggression: bool,
    attack_start_range_scale: f32,
    sight_range_scale: f32,
    sight_fov_scale: f32,
    base_skill_id: i32,
    base_skill_delay: f32,
    base_skill_strafe: bool,
    use_range_mod: bool,
    skills_count: usize,
    return_distance: f32,
    absolute_return_distance: f32,
    npc_ai_param_id: i32,
    npc_nickname_id: i32,
    scale: f32,
}

/// `Ai.Owner.Template.AiParams`.
fn template_ai_params(c: &AiCtx<'_>) -> Option<Arc<AiParams>> {
    let id = template_scalars(c).npc_ai_param_id;
    c.server.game_data.systems.ai.ai.ai_params_for_id(id as u32)
}

fn delta_seconds(delta_ms: i64) -> f64 {
    // `delta.Milliseconds / 1000.0`: the millisecond component of the TimeSpan.
    (delta_ms.rem_euclid(1000)) as f64 / 1000.0
}

fn move_ai(c: &AiCtx<'_>) -> Option<MoveAi> {
    Some(MoveAi {
        hold_or_idle: matches!(c.ai.current_behavior(), Some(B::HoldPosition | B::Idle)),
    })
}

/// `Ai.Owner.MoveTowards(target, distance, flags, rangeTolerance)`.
pub fn npc_move_towards(
    c: &AiCtx<'_>,
    target: Vec3,
    distance: f32,
    flags: u8,
    tolerance: f32,
) -> bool {
    npc::move_towards(
        c.server,
        &c.npc,
        move_ai(c),
        target,
        distance,
        flags,
        tolerance,
    )
}

/// `Ai.Owner.BaseMoveSpeed`.
fn base_move_speed(c: &AiCtx<'_>) -> f32 {
    npc::base_move_speed(
        c.server,
        &c.npc,
        c.ai.current_behavior() == Some(B::ReturnState),
    )
}

/// `Ai.Owner.SetTarget(other)`.
pub fn set_target(c: &mut AiCtx<'_>, other: Option<&WorldObject>) {
    npc::set_target(c.server, &c.npc, other);
    c.ai.already_targeted = other.is_some();
    if other.is_none() {
        reset_chase_path(c);
    }
}

/// `Ai.Owner.CurrentTarget` (as a world object).
fn current_target(c: &AiCtx<'_>) -> Option<WorldObject> {
    let id = c.npc.data().unit.current_target;
    if id == 0 {
        return None;
    }
    if id == c.npc.obj_id {
        return Some(c.obj.clone());
    }
    c.server.world.object(id)
}

fn owner_pos(c: &AiCtx<'_>) -> Vec3 {
    npc::world_pos(c.server, &c.obj)
}

/// The reach of the melee base skill (id 2): the `PickSkillAndUseIt` guard distance and the cap
/// of the `MoveInRange` stop distance.
const MELEE_ATTACK_RANGE: f32 = 4.0;

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
    // Face the target before the skill validates its cone. Turning only after a
    // successful cast would leave an NPC permanently unable to attack to its side.
    if !ai_skills::is_self_target(&skill.template) {
        let position = npc::world_pos(c.server, target);
        npc::look_towards(c.server, &c.npc, position, 4);
    }
    // skill.Callback = OnSkillEnded
    ai_skills::set_skill_ended_callback(c, &skill, k);
    ai_skills::use_skill(c, &skill, caster, &mut cast_target, false)
}

/// `Behavior.OnSkillEnded()` (`_delayEnd = now + _nextTimeToDelay`).
pub fn on_skill_ended(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let s = c.st(k);
    let add = f64::from(s.next_time_to_delay) * 1000.0;
    if add.is_finite() {
        s.delay_end = now_ms() + add as i64;
    }
}

/// `Behavior.OnEnemySeen(target)`.
fn on_enemy_seen(c: &mut AiCtx<'_>, target: &WorldObject) {
    add_unit_aggro(c.server, &c.obj, AggroKind::Damage, target, 1);
    c.go_to_combat();
}

/// The units sorted by 3D distance (`CheckAggression` / `CheckAlert`).
fn units_by_distance(c: &AiCtx<'_>, radius: f32) -> Vec<(WorldObject, f32)> {
    let me = c.obj.clone();
    let mut v: Vec<(WorldObject, f32)> = npc::around(c.server, &me, radius, false)
        .into_iter()
        .filter(WorldObject::is_unit)
        .map(|u| {
            let d = npc::distance_raw(c.server, &me, &u, true);
            (u, d)
        })
        .collect();
    v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

fn unit_dead(u: &WorldObject) -> bool {
    u.with_unit(|x| x.unit().is_dead() || x.unit().hp <= 0)
        .unwrap_or(false)
}

/// The shared scan of `CheckAggression` / `CheckAlert`.
fn scan_for_enemy(
    c: &AiCtx<'_>,
    radius: f32,
    gap_fly: f32,
    gap_ground: f32,
    near_mul: f32,
) -> Option<WorldObject> {
    let t = template_scalars(c);
    let (can_fly, scale) = {
        let n = c.npc.data();
        (n.can_fly, n.unit.scale)
    };
    let model_size = npc::model_size(c.server, &c.obj);
    let my_z = owner_pos(c).z;
    for (unit, range) in units_by_distance(c, radius) {
        if unit_dead(&unit) {
            continue; // not counting dead Npc
        }
        // Arbitrary value
        let max_height_gap = if can_fly {
            model_size * scale * gap_fly
        } else {
            model_size * scale * gap_ground
        };
        let uz = npc::world_pos(c.server, &unit).z;
        // Check if in front, and not too far up or down
        if npc::is_front(c.server, &c.obj, &unit, f64::from(t.sight_fov_scale))
            && (my_z - uz).abs() < max_height_gap
        {
            if can_attack(c.server, &c.obj, &unit) && npc::can_see_target(c.server, &c.obj, &unit) {
                return Some(unit);
            }
        } else if range < near_mul * t.sight_range_scale
            && can_attack(c.server, &c.obj, &unit)
            && npc::can_see_target(c.server, &c.obj, &unit)
        {
            return Some(unit);
        }
    }
    None
}

/// `Behavior.CheckAggression()`.
pub fn check_aggression(c: &mut AiCtx<'_>, _k: BehaviorKind) -> bool {
    let t = template_scalars(c);
    if !t.aggression {
        return false;
    }
    match scan_for_enemy(c, t.attack_start_range_scale * 10.0, 3.5, 1.5, 1.5) {
        Some(unit) => {
            on_enemy_seen(c, &unit);
            true
        }
        None => false,
    }
}

/// `Behavior.OnEnemyAlert(target)`.
fn on_enemy_alert(c: &mut AiCtx<'_>, target: &WorldObject) {
    let now = now_ms();
    c.ai.alert_end_time = now + 5000;
    c.ai.next_alert_check_time = now + 7000;
    set_target(c, Some(target));
    c.go_to_alert();
}

/// `Behavior.CheckAlert()`.
pub fn check_alert(c: &mut AiCtx<'_>, _k: BehaviorKind) -> bool {
    if c.ai.next_alert_check_time > now_ms() {
        return false;
    }
    // Don't do alerts if already in combat
    if c.npc.data().unit.is_in_battle {
        return false;
    }
    let t = template_scalars(c);
    match scan_for_enemy(c, t.sight_range_scale * 15.0, 4.0, 1.75, 2.0) {
        Some(unit) => {
            on_enemy_alert(c, &unit);
            true
        }
        None => false,
    }
}

/// Whether this AI-less NPC has not been reported yet (the help loop runs on every tick, and
/// the crawler produced 429 repeated errors for a handful of ambient NPCs).
fn report_npc_without_ai(obj_id: u32) -> bool {
    static REPORTED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u32>>> =
        std::sync::OnceLock::new();
    // Respawned NPCs get fresh object ids: bound the set so it cannot grow for the life of
    // the process. A clear only lets each NPC warn once more.
    const MAX_REPORTED: usize = 4096;
    let mut reported = REPORTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if reported.len() >= MAX_REPORTED {
        reported.clear();
    }
    reported.insert(obj_id)
}

/// 10.x candidate scan radius of the aggro-link help (`sub_39325250(..., 50.0, ...)`,
/// Z:823753). The guard help of a player / mate / slave victim scans the same radius around the
/// victim; the candidate's own `aggro_link_help_dist` (or its sight check) still applies.
const GUARD_HELP_SCAN_RADIUS: f32 = 50.0;

/// The player-victim guard scan runs at most once per second per `(victim, abuser)` pair:
/// DoT ticks and multi-hit skills would otherwise repeat the 50 m query for the same fight.
const GUARD_HELP_SCAN_INTERVAL_MS: i64 = 1_000;

/// Upper bound of the per-pair scan stamps; old entries are pruned at the cap.
const GUARD_HELP_SCAN_MAX_ENTRIES: usize = 4_096;

/// Rate limit of the player-victim guard scan (`server.systems.ai.guard_help_scans`): true when
/// this `(victim, abuser)` pair may scan now, and records the scan otherwise.
fn guard_help_scan_due(server: &GameServer, victim: u32, abuser: u32) -> bool {
    let now = now_ms();
    let mut scans = server
        .systems
        .ai
        .guard_help_scans
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if scans.len() >= GUARD_HELP_SCAN_MAX_ENTRIES {
        scans.retain(|_, at| now - *at < GUARD_HELP_SCAN_INTERVAL_MS);
        if scans.len() >= GUARD_HELP_SCAN_MAX_ENTRIES {
            scans.clear();
        }
    }
    match scans.get(&(victim, abuser)) {
        Some(at) if now - *at < GUARD_HELP_SCAN_INTERVAL_MS => false,
        _ => {
            scans.insert((victim, abuser), now);
            true
        }
    }
}

/// The `aggro_link_special_rule_id` switch of the 10.x help scan (Z:823790-823816) against the
/// victim: 1 = same faction, 2 = friendly, 3 = neutral, 4 = everyone.
fn guard_rule_helps(
    server: &GameServer,
    helper: &WorldObject,
    victim: &WorldObject,
    helper_faction: u32,
    victim_faction: u32,
    template: &NpcTemplate,
) -> bool {
    use crate::systems::skills::types::RelationState as R;
    match template.aggro_link_special_rule_id {
        1 => helper_faction == victim_faction,
        2 => {
            crate::systems::skills::relation::relation_to(
                server,
                &crate::systems::skills::geometry::Target::Obj(helper.clone()),
                &crate::systems::skills::geometry::Target::Obj(victim.clone()),
            ) == R::Friendly
        }
        3 => {
            crate::systems::skills::relation::relation_to(
                server,
                &crate::systems::skills::geometry::Target::Obj(helper.clone()),
                &crate::systems::skills::geometry::Target::Obj(victim.clone()),
            ) == R::Neutral
        }
        4 => true,
        _ => false,
    }
}

/// The guard branch of the 10.x aggro-link help (Z:823880-823909) on top of the rule switch:
/// a `aggro_link_special_guard` answers a victim whose faction has `system_factions.guard_help`,
/// never a duel between the victim and the attacker (`sub_39322ED0`), never an NPC attacker when
/// its template carries `aggro_link_special_ignore_npc_attacker` (0x40, so guards ignore monster
/// attackers), never another `aggro_link_special_guard` (0x20), and only an attacker the guard
/// may attack (`CanAttack`). The 10.x flag bits of the NPC template: 0x2 `accept_aggro_link`,
/// 0x10 `aggro_link_sight_check`, 0x20 `aggro_link_special_guard`,
/// 0x40 `aggro_link_special_ignore_npc_attacker` (inferred from the loader column order,
/// Z:2426767). The 10.x `guard_npc_no_help_zone` relation reason has no 1.2 zone column.
fn guard_may_help(
    server: &GameServer,
    victim: &WorldObject,
    abuser: &WorldObject,
    helper: &WorldObject,
    template: &NpcTemplate,
    victim_faction: u32,
) -> bool {
    if !server
        .game_data
        .factions
        .get(victim_faction)
        .is_some_and(|f| f.guard_help)
    {
        return false;
    }
    if let Some(abuser_npc) = abuser.as_npc() {
        if template.aggro_link_special_ignore_npc_attacker {
            return false;
        }
        if npc::template(server, abuser_npc).is_some_and(|t| t.aggro_link_special_guard) {
            return false;
        }
    }
    // A duel is not helped, including when the attacker or the victim is the duellist's pet or
    // slave (the 10.x check compares the units' relations, and `owner_character` resolves a
    // mate / slave to its owner).
    if let (Some(v), Some(a)) = (
        crate::systems::skills::relation::owner_character(server, victim),
        crate::systems::skills::relation::owner_character(server, abuser),
    ) {
        if server
            .systems
            .social
            .duels
            .is_in_duel_with(v.character_id, a.character_id)
        {
            return false;
        }
    }
    can_attack(server, helper, abuser)
}

/// 10.x aggro-link guard help for a victim without an NPC AI (player, mate, slave): the damage
/// path calls it when a unit is attacked, so special guards within
/// [`GUARD_HELP_SCAN_RADIUS`] of the victim join the fight against `abuser` (Z:823528-823950).
/// NPC victims keep the C# `UpdateAggroHelp` path (`update_aggro_help`) unchanged, so guards
/// still answer a fellow guard / group NPC attacked by a monster.
pub fn update_guard_help(server: &GameServer, victim: &WorldObject, abuser: &WorldObject) {
    if victim.same(abuser) || !abuser.is_unit() {
        return;
    }
    let Some(victim_faction) = victim.with_unit(|u| u.unit().faction_id) else {
        return;
    };
    // `system_factions.guard_help` is a property of the victim's faction; skip the scan when it
    // is off (monster factions, ...), so the cost is only paid near a protected victim.
    if !server
        .game_data
        .factions
        .get(victim_faction)
        .is_some_and(|f| f.guard_help)
    {
        return;
    }
    // An NPC attacker can only pull guards whose template does not carry the 0x40
    // `aggro_link_special_ignore_npc_attacker` flag; when no loaded guard template lacks it,
    // the scan is pointless (cheap data-level check before the 50 m query).
    if abuser.as_npc().is_some()
        && !server
            .game_data
            .systems
            .npcs
            .npc
            .special_guards_allow_npc_attacker
    {
        return;
    }
    // DoT ticks and repeated hits of the same fight reuse the first scan for one second.
    if !guard_help_scan_due(server, victim.obj_id(), abuser.obj_id()) {
        return;
    }
    let candidates: Vec<WorldObject> = npc::around(server, victim, GUARD_HELP_SCAN_RADIUS, false)
        .into_iter()
        .filter(|o| o.as_npc().is_some())
        .collect();
    for other in candidates {
        let Some(other_npc) = other.as_npc().cloned() else {
            continue;
        };
        let Some(ot) = npc::template(server, &other_npc) else {
            continue;
        };
        if !ot.aggro_link_special_guard || !ot.accept_aggro_link {
            continue;
        }
        if other.same(victim) || other.same(abuser) {
            continue;
        }
        let (in_battle, helper_faction) = {
            let d = other_npc.data();
            (d.unit.is_in_battle, d.unit.faction_id)
        };
        if in_battle {
            continue;
        }
        let distance = npc::distance_to(server, &other, victim, false);
        if distance > GUARD_HELP_SCAN_RADIUS {
            continue;
        }
        // 10.x: with `aggro_link_sight_check` the guard must see the victim and the per-template
        // help distance does not gate it; without it, `aggro_link_help_dist` does (Z:823780-790).
        let in_range = if ot.aggro_link_sight_check {
            npc::can_see_target(server, &other, victim)
        } else {
            distance <= ot.aggro_link_help_dist
        };
        if !in_range
            || !guard_rule_helps(server, &other, victim, helper_faction, victim_faction, ot)
            || !guard_may_help(server, victim, abuser, &other, ot, victim_faction)
        {
            continue;
        }
        if !super::manager::has_ai(server, other_npc.obj_id) {
            continue;
        }
        add_unit_aggro(server, &other, AggroKind::Damage, abuser, 1);
        super::manager::post(server, other_npc.obj_id, |o| o.on_aggro_target_changed());
        tracing::debug!(
            guard = other_npc.obj_id,
            victim = victim.obj_id(),
            abuser = abuser.obj_id(),
            "guard help: joined the fight"
        );
    }
}

/// `Behavior.UpdateAggroHelp(abuser, radius)`.
pub fn update_aggro_help(c: &mut AiCtx<'_>, abuser: &WorldObject, radius: f32) {
    // `npc_groups.aggro_rule_id == 2` mirrors the aggro table; rule 1 members answer the help
    // call through the group-link path below (10.x `NpcAggroTable::CollectSharedAggroMembers`
    // and the link-help group-instance bypass).
    super::group::share_aggro_with_group(c, abuser);
    let t = template_scalars(c);
    let (my_template_id, my_group_instance, my_group_rule, my_faction) = {
        let d = c.npc.data();
        (
            d.unit.template_id,
            d.npc_group_instance,
            d.npc_group_aggro_rule_id,
            d.unit.faction_id,
        )
    };
    let link_data = &c.server.game_data.systems.npcs.npc.aggro_links;
    let npcs: Vec<WorldObject> =
        npc::around(c.server, &c.obj, t.attack_start_range_scale * radius, false)
            .into_iter()
            .filter(|o| o.as_npc().is_some())
            .collect();
    for other in npcs {
        let Some(other_npc) = other.as_npc().cloned() else {
            continue;
        };
        let Some(ot) = npc::template(c.server, &other_npc) else {
            continue;
        };
        let (in_battle, faction, other_group_instance) = {
            let d = other_npc.data();
            (d.unit.is_in_battle, d.unit.faction_id, d.npc_group_instance)
        };
        // 10.x link help with `npc_groups.aggro_rule_id == 1` (Z:823640-823646 /
        // 823849-823851): a candidate of the same spawned batch is accepted even when its own
        // template relation test fails, so the group mates answer the help call together. The
        // batch's rule 2 members are already on the table (mirrored above); rule 0 neither
        // mirrors nor answers by group.
        let group_link = my_group_instance != 0
            && my_group_rule == 1
            && other_group_instance == my_group_instance;
        // 1.2 data: the members of an `aggro_links` group help each other even when their own
        // template (`accept_aggro_link`, `aggression` / `aggro_link_special_rule_id`) would
        // decline. No consumer was found in the 10.x decompile, so this rule is data-inferred
        // (capture-pending AGGROLINK-1). Non-members keep the template rules exactly.
        let same_group = link_data.share_group(my_template_id, ot.id);
        if in_battle || (!ot.accept_aggro_link && !same_group && !group_link) {
            continue;
        }
        // Rule-1 batch mates answer out to their own aggression radius as a fallback: the 1.2
        // `aggro_link_help_dist` is 6 m for 91 of the 101 rule-1 groups (376 member rows:
        // median 6, max 50, none 0), too short once the batch spreads beyond its 2-3 m
        // formation offsets. The 10.x link-help scan collects candidates within 50 m
        // (`sub_39325250(..., 50.0, ...)`, Z:823753) and then applies the per-template
        // distance, so a group-link mate uses `max(aggro_link_help_dist,
        // attack_start_range_scale * 10)` (the `check_aggression` scan radius).
        let help_dist = if group_link {
            ot.aggro_link_help_dist
                .max(ot.attack_start_range_scale * 10.0)
        } else {
            ot.aggro_link_help_dist
        };
        if npc::distance_to(c.server, &other, &c.obj, false) > help_dist {
            continue;
        }
        // The template path is the C# `UpdateAggroHelp` (Behavior.cs, `a2ab80abd^`): the
        // special-rule branch requires `aggro_link_sight_check` to be enabled AND the abuser
        // visible; a template with sight check off never helps through rules 1..4.
        let template_helps = if ot.aggression && ot.aggro_link_special_rule_id == 0 {
            true
        } else {
            if !(ot.aggro_link_sight_check && npc::can_see_target(c.server, &other, abuser)) {
                false
            } else {
                let relation = || {
                    crate::systems::skills::relation::relation_to(
                        c.server,
                        &crate::systems::skills::geometry::Target::Obj(other.clone()),
                        &crate::systems::skills::geometry::Target::Obj(c.obj.clone()),
                    )
                };
                use crate::systems::skills::types::RelationState as R;
                match ot.aggro_link_special_rule_id {
                    1 => faction == my_faction,
                    2 => relation() == R::Friendly,
                    3 => relation() == R::Neutral,
                    4 => true,
                    _ => false,
                }
            }
        };
        // Link path (AGGROLINK-1): the sight check applies only when the helper enables it.
        let link_helps = same_group
            && (!ot.aggro_link_sight_check || npc::can_see_target(c.server, &other, abuser));
        // Rule-1 group mates always answer the help call (the relation test above is bypassed);
        // the helper's own sight check still applies, as in the 10.x candidate scan.
        let group_link_helps = group_link
            && (!ot.aggro_link_sight_check || npc::can_see_target(c.server, &other, abuser));
        if !template_helps && !link_helps && !group_link_helps {
            continue;
        }
        // npc.Ai.Owner.AddUnitAggro(...) + npc.Ai.OnAggroTargetChanged(): an NPC without an
        // AI threw a NullReferenceException in C#, which ended the whole help loop. Keep
        // helping the remaining linked NPCs and report each AI-less one once.
        if !super::manager::has_ai(c.server, other_npc.obj_id) {
            if report_npc_without_ai(other_npc.obj_id) {
                tracing::warn!(
                    "UpdateAggroHelp: npc {} has no AI; skipped (C# threw a NullReferenceException)",
                    other_npc.obj_id
                );
            }
            continue;
        }
        let was_engaged = other_npc
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&abuser.obj_id());
        add_unit_aggro(c.server, &other, AggroKind::Damage, abuser, 1);
        if (link_helps || group_link_helps) && !template_helps && !was_engaged {
            let engaged = other_npc
                .data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&abuser.obj_id());
            if engaged {
                tracing::debug!(
                    helper = other_npc.obj_id,
                    leader = c.npc.obj_id,
                    abuser = abuser.obj_id(),
                    group_link = group_link_helps,
                    "aggro link: group member joined the fight"
                );
            }
        }
        super::manager::post(c.server, other_npc.obj_id, |o| o.on_aggro_target_changed());
    }
}

/// `Behavior.SetWeaponRange(skill, target)`.
fn set_weapon_range(
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

/// `Behavior.CheckFollowPath()`.
fn check_follow_path(c: &AiCtx<'_>) -> bool {
    c.ai.path_handler.has_path_movement_data()
}

/// `BaseCombatBehavior.CanStrafe`.
fn can_strafe(c: &mut AiCtx<'_>, k: BehaviorKind) -> bool {
    let s = c.st(k);
    now_ms() > s.delay_end || s.strafe_during_delay
}

/// `BaseCombatBehavior.IsUsingSkill`.
fn is_using_skill(c: &AiCtx<'_>) -> bool {
    let n = c.npc.data();
    n.unit.skill_state.skill_task.is_some() || n.unit.skill_state.active_plot.is_some()
}

/// `BaseCombatBehavior.CanUseSkill`.
fn can_use_skill(c: &mut AiCtx<'_>, k: BehaviorKind) -> bool {
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

/// `BaseCombatBehavior.ShouldReturn`.
fn should_return(c: &AiCtx<'_>) -> bool {
    // Never trigger return while a SkillController is actively driving the NPC.
    if npc::skill_controller_running(&c.obj) {
        return false;
    }
    let t = template_scalars(c);
    let mut return_distance = 50.0f32;
    let mut absolute_return_distance = 200.0f32;
    if t.return_distance > 0.0 {
        return_distance = t.return_distance;
    }
    if t.absolute_return_distance > 0.0 {
        absolute_return_distance = t.absolute_return_distance;
    }
    let Some(target) = current_target(c) else {
        return true; // no target, returning
    };
    let me = owner_pos(c);
    let distance_to_target = calculate_distance(me, npc::world_pos(c.server, &target), true);
    let distance_to_idle = calculate_distance(me, c.ai.idle_position, true);
    let mut res = distance_to_target > return_distance || distance_to_idle > return_distance;
    if res {
        // if it's greater, then we need a teleport to the spawn point
        res = distance_to_idle <= absolute_return_distance;
    }
    res
}

fn geo_data_mode(c: &AiCtx<'_>) -> bool {
    c.server.config.world.geo_data_mode
}

/// `BaseCombatBehavior.UpdateTarget()`: the abuser with the most aggro becomes the target.
fn update_target(c: &mut AiCtx<'_>) -> bool {
    if npc::skill_controller_running(&c.obj) {
        return current_target(c).is_some();
    }
    let mut abusers = sorted_abusers(c);
    if c.server.config.ai.pull_aggro_enabled {
        apply_pull_aggro(c, &mut abusers);
    }
    for abuser in abusers {
        let Some(abuser) = abuser else { continue };
        if geo_data_mode(c) {
            if npc::unit_is_visible(c.server, &c.obj, &abuser) && !npc::is_dead(&abuser) {
                let current_aggro = c.npc.data().current_aggro_target;
                if current_aggro != abuser.obj_id() && !c.ai.already_targeted {
                    find_path(c, &abuser, "new target");
                }
                npc::set_current_aggro_target(&c.npc, Some(&abuser));
                set_target(c, Some(&abuser));
                update_aggro_help(c, &abuser, 200.0);
                return true;
            }
        } else if npc::unit_is_visible(c.server, &c.obj, &abuser) && !npc::is_dead(&abuser) {
            // check that such a Npc is in the database
            let in_world = abuser.obj_id() > 0
                && super::npc::world_unit(c.server, &c.obj, abuser.obj_id()).is_some();
            if !in_world {
                continue;
            }
            npc::set_current_aggro_target(&c.npc, Some(&abuser));
            set_target(c, Some(&abuser));
            update_aggro_help(c, &abuser, 200.0);
            return true;
        }
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
    }
    // Only remove CurrentTarget if either no unit selected, or if target is already dead
    match current_target(c) {
        Some(t) if t.is_unit() => {
            let hp = t.with_unit(|u| u.unit().hp).unwrap_or(0);
            if hp <= 0 || npc::is_dead(&t) {
                npc::set_current_aggro_target(&c.npc, None);
                set_target(c, None);
            }
        }
        _ => {
            npc::set_current_aggro_target(&c.npc, None);
            set_target(c, None);
        }
    }
    false
}

/// `AggroTable.Values.OrderByDescending(TotalAggro).Select(Owner)` (missing objects stay as
/// `None` and are skipped), extended with the 10.x taunt order (10x-ONLY): an entry with a
/// taunt stamp sorts above every unstamped entry and the latest stamp wins. Unstamped entries
/// keep the total order; the stable sort keeps the table (obj id) order on ties.
fn sorted_abusers(c: &AiCtx<'_>) -> Vec<Option<WorldObject>> {
    sorted_entries(c)
        .into_iter()
        .map(|(id, _, _)| c.server.world.object(id))
        .collect()
}

/// The aggro table as `(obj id, total, taunt stamp)` in the order of [`sorted_abusers`].
fn sorted_entries(c: &AiCtx<'_>) -> Vec<(u32, i32, u64)> {
    let mut entries: Vec<(u32, i32, u64)> = c
        .npc
        .data()
        .unit
        .skill_state
        .aggro_table
        .iter()
        .map(|(k, a)| (*k, a.total(), a.taunt_time))
        .collect();
    entries.sort_by(|a, b| match (a.2, b.2) {
        (0, 0) => b.1.cmp(&a.1),
        (0, _) => std::cmp::Ordering::Greater,
        (_, 0) => std::cmp::Ordering::Less,
        (x, y) => y.cmp(&x),
    });
    entries
}

/// Whether `update_target` would accept `abuser` as the target (visible, alive and, without
/// geodata, present in the world).
fn abuser_is_valid(c: &AiCtx<'_>, abuser: &WorldObject) -> bool {
    if !npc::unit_is_visible(c.server, &c.obj, abuser) || npc::is_dead(abuser) {
        return false;
    }
    geo_data_mode(c)
        || (abuser.obj_id() > 0
            && super::npc::world_unit(c.server, &c.obj, abuser.obj_id()).is_some())
}

/// Pull aggro (added deviation, `PORTING.md`, config `Ai.PullAggro*`): the NPC keeps its valid
/// current aggro target until a challenger beats it. A taunt stamp beats an unstamped target
/// (and an earlier stamp); otherwise the challenger's total has to exceed the target's total
/// times the melee or ranged ratio. The chosen entry is moved to the front of `abusers`, so
/// the normal selection loop picks it; with no valid current target nothing changes.
fn apply_pull_aggro(c: &AiCtx<'_>, abusers: &mut Vec<Option<WorldObject>>) {
    let current_id = c.npc.data().current_aggro_target;
    if current_id == 0 {
        return;
    }
    let entries = sorted_entries(c);
    let Some(&(_, cur_total, cur_taunt)) = entries.iter().find(|e| e.0 == current_id) else {
        return;
    };
    let Some(current) = c.server.world.object(current_id) else {
        return;
    };
    if !abuser_is_valid(c, &current) {
        return;
    }
    let cfg = &c.server.config.ai;
    let base = f64::from(cur_total.max(0));
    let mut chosen = current_id;
    for &(id, total, taunt) in &entries {
        if id == current_id {
            continue;
        }
        let Some(challenger) = c.server.world.object(id) else {
            continue;
        };
        if !abuser_is_valid(c, &challenger) {
            continue;
        }
        let wins = if taunt != 0 {
            cur_taunt == 0 || taunt > cur_taunt
        } else if cur_taunt != 0 {
            false
        } else {
            let melee =
                npc::distance_to(c.server, &c.obj, &challenger, true) <= cfg.pull_aggro_melee_range;
            let ratio = if melee {
                cfg.pull_aggro_melee_ratio
            } else {
                cfg.pull_aggro_ranged_ratio
            };
            f64::from(total) > base * ratio
        };
        if wins {
            chosen = id;
            break;
        }
    }
    if let Some(i) = abusers
        .iter()
        .position(|a| a.as_ref().is_some_and(|a| a.obj_id() == chosen))
    {
        let entry = abusers.remove(i);
        abusers.insert(0, entry);
    }
}

/// Drops the chase route and its bookkeeping, so the next chase step plans from scratch.
fn reset_chase_path(c: &mut AiCtx<'_>) {
    c.ai.path_node.found_path.clear();
    c.ai.path_node.end_point_pos = Vec3::default();
    c.ai.chase_path = None;
}

/// Flying and swimming NPCs (`ActorModel.MovementId == 2` / `bUnderwaterCreature`) move in
/// 3D. The A* graph is a walking-surface navmesh, so following its (ground level, detouring)
/// waypoints only drags them off the straight line to the victim.
fn moves_freely(c: &AiCtx<'_>) -> bool {
    let n = c.npc.data();
    n.can_fly
        || c.server
            .game_data
            .systems
            .ai
            .ai
            .actor_model(n.unit.model_id)
            .is_some_and(|m| m.underwater_creature)
}

/// `Npc.FindPath(abuser)` (geodata A*); `reason` is only logged. Free movers
/// ([`moves_freely`]) never get a route (deviation from the C# code, see `PORTING.md`).
fn find_path(c: &mut AiCtx<'_>, abuser: &WorldObject, reason: &str) {
    if moves_freely(c) {
        reset_chase_path(c);
        return;
    }
    super::geodata::npc_find_path(c, abuser);
    let had_route = !c.ai.path_node.found_path.is_empty();
    c.ai.chase_path = Some(ChasePath {
        target: abuser.obj_id(),
        at_ms: now_ms(),
        had_route,
    });
    tracing::debug!(
        npc = c.npc.obj_id,
        target = abuser.obj_id(),
        reason,
        steps = c.ai.path_node.found_path.len(),
        "chase path planned"
    );
}

/// The victim has to move this far (m) from the route's end before the route is planned
/// again; the lower bound of [`REPATH_MOVE_FRACTION`] of the distance to the victim. A
/// player's reported position wobbles by a few tenths of a metre while standing or fighting,
/// and every replan starts at the NPC's new position, so replanning at the 0.5 m model-size
/// threshold flipped the route around navmesh obstacles on every other tick.
const REPATH_MIN_MOVE: f32 = 2.0;
/// Fraction of the NPC-to-victim distance the victim may move before a replan: a far chaser
/// does not need a fresh route for a small sidestep, a near one is limited by the minimum.
const REPATH_MOVE_FRACTION: f32 = 0.2;
/// Minimum time between two plans for the same victim (ms); the AI ticks every 100 ms.
const REPATH_MIN_INTERVAL_MS: i64 = 500;

/// Why the chase route must be planned again, or `None` to keep following it.
fn repath_reason(
    c: &AiCtx<'_>,
    target_id: u32,
    my_pos: Vec3,
    target_pos: Vec3,
    now: i64,
) -> Option<&'static str> {
    let end = c.ai.path_node.end_point_pos;
    let moved = calculate_distance(end, target_pos, true)
        > REPATH_MIN_MOVE.max(REPATH_MOVE_FRACTION * calculate_distance(my_pos, target_pos, true));
    let Some(last) = c.ai.chase_path else {
        // No bookkeeping: reset (zero end point) or a route set outside the chase loop.
        return if end == Vec3::default() {
            Some("new target")
        } else {
            moved.then_some("target moved")
        };
    };
    if last.target != target_id {
        return Some("new target");
    }
    if now - last.at_ms < REPATH_MIN_INTERVAL_MS {
        return None;
    }
    if moved {
        Some("target moved")
    } else if last.had_route && c.ai.path_node.found_path.is_empty() {
        Some("path exhausted")
    } else {
        None
    }
}

/// `BaseCombatBehavior.CheckPipeName()`.
fn check_pipe_name(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let (pipe, phase) = {
        let s = c.st(k);
        (s.pipe_name.clone(), s.phase_type)
    };
    if pipe == "phase_dragon_ground" || phase == 1 {
        // try to find Z first in GeoData, and then in HeightMaps, if not found, leave Z as it is
        let pos = owner_pos(c);
        let z = super::geodata::geo_data_height(c.server, &c.obj, pos);
        c.obj.with_transform(|t| t.set_height(z));
    } else if pipe == "phase_dragon_fly_hovering" || phase == 2 {
        c.obj.with_transform(|t| t.z += 15.0);
        npc::stop_movement(c.server, &c.npc);
    } else if pipe == "phase_dragon_fly_path" {
        c.go_to_follow_path();
    }
}

/// `BaseCombatBehavior.MoveInRange(target, delta)`.
fn move_in_range(c: &mut AiCtx<'_>, k: BehaviorKind, target: Option<&WorldObject>, delta_ms: i64) {
    if npc::is_displaced(&c.obj) {
        return;
    }
    if npc::is_rooted(c.server, &c.obj) || npc::is_dead(&c.obj) {
        return;
    }
    if npc::is_shackled_or_snared(c.server, &c.obj) {
        return;
    }
    if npc::skill_controller_running(&c.obj) {
        return;
    }
    let mut speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
    let move_flags = npc::real_movement_flags(speed);
    speed *= delta_seconds(delta_ms);
    // Sport-fish direction plots provide bounded movement through SkillController.
    // The former C# experimental global-axis drift added movement after each controller
    // and changed the heading used by the next relative plot (Dive even moved along Y).
    // Between authored controllers, stop without changing fish position or rotation.
    if buffs::check_buffs_by_tag(c.server, &c.obj, npc::tags::FISH) {
        npc::stop_movement(c.server, &c.npc);
        return;
    }
    let t = template_scalars(c);
    let mut range = t.attack_start_range_scale;
    let max_weapon_range = c.st(k).max_weapon_range;
    if t.use_range_mod && max_weapon_range != 0.0 {
        range *= max_weapon_range;
    }
    if t.base_skill_id == 2 && t.skills_count == 0 && range == 4.0 {
        range -= 1.0; // Fix that ID=7927, Plateau Earth Elemental can hit with a melee attack
    }
    let distance_to_target = match target {
        Some(t) => npc::distance_to(c.server, &c.obj, t, true),
        None => 0.0,
    };
    let Some(target) = target else {
        npc::stop_movement(c.server, &c.npc);
        return;
    };
    let target_pos = npc::world_pos(c.server, target);
    if distance_to_target <= range {
        npc::stop_movement(c.server, &c.npc);
        return;
    }
    if geo_data_mode(c) && moves_freely(c) {
        // Flyers and swimmers chase in a straight 3D line; drop any route left over.
        if c.ai.chase_path.is_some() || !c.ai.path_node.found_path.is_empty() {
            reset_chase_path(c);
        }
    } else if geo_data_mode(c) {
        // COPStick::Execute (10.x cryaisystem_dedicate 0x310F24D8) retains its route
        // until the target moves beyond the regeneration distance. The C# test was
        // reversed: a stationary target restarted the route at its first node every tick.
        let target_id = target.obj_id();
        if let Some(reason) = repath_reason(c, target_id, owner_pos(c), target_pos, now_ms()) {
            let started = std::time::Instant::now();
            find_path(c, target, reason);
            if started.elapsed() >= std::time::Duration::from_millis(1) {
                tracing::warn!(
                    "FindPath took {:?} for Ai.Owner.ObjId:{}",
                    started.elapsed(),
                    c.npc.obj_id
                );
            }
            c.ai.path_node.end_point_pos = target_pos;
        }
        // Navigation heights can differ from the collision floor, and a walking NPC follows
        // the ground, so waypoints are reached in XY; attack range applies to the victim,
        // not path corners.
        let pos = owner_pos(c);
        const WAYPOINT_TOLERANCE: f32 = 0.25;
        while let Some(next) = c.ai.path_node.found_path.front().copied() {
            if calculate_distance(pos, next, false) > WAYPOINT_TOLERANCE {
                npc_move_towards(c, next, speed as f32, move_flags, WAYPOINT_TOLERANCE);
                return;
            }
            c.ai.path_node.current_target_pos = next;
            c.ai.path_node.found_path.pop_front();
        }
    }
    npc_move_towards(c, target_pos, speed as f32, move_flags, 1.0);
}

/// `RequestAvailableAiSkillList` / `RequestAvailableSkillList` (same filter on both list kinds).
fn health_and_time_ok(
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

fn health_ratio(c: &AiCtx<'_>) -> i32 {
    let (hp, max) = {
        let n = c.npc.data();
        (n.unit.hp, n.unit.max_hp)
    };
    (hp as f32 / max as f32 * 100.0) as i32
}

/// `BaseCombatBehavior.RefreshSkillQueue(skillLists, aiParams)`.
fn refresh_skill_queue(
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

/// `BaseCombatBehavior.GetPlayersInRange(owner, range, fovScale, greeted, cooldown)`.
fn players_in_range(
    c: &AiCtx<'_>,
    range: f32,
    fov: f64,
    k_greeted: &std::collections::BTreeMap<u32, i64>,
    cooldown_ms: i64,
) -> Vec<WorldObject> {
    let now = now_ms();
    npc::around(c.server, &c.obj, range, true)
        .into_iter()
        .filter(|o| o.as_player().is_some())
        .filter(|p| npc::is_front(c.server, &c.obj, p, fov))
        .filter(|p| {
            k_greeted
                .get(&p.obj_id())
                .is_none_or(|last| now - last >= cooldown_ms)
        })
        .collect()
}

const GREET_RANGE: f32 = 5.0;
const GREET_FOV: f64 = 0.6667;
const GREET_COOLDOWN_MS: i64 = 5 * 60 * 1000;

/// `CheckForTalk()` of Idle / HoldPosition.
fn check_for_talk(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if c.ai.current_behavior() == Some(B::Talk) {
        return;
    }
    let now = now_ms();
    let greeted = c.st(k).greeted.clone();
    let players = players_in_range(c, GREET_RANGE, GREET_FOV, &greeted, GREET_COOLDOWN_MS);
    let ids: Vec<u32> = players.iter().map(WorldObject::obj_id).collect();
    let s = c.st(k);
    for id in &ids {
        if s.greeted
            .get(id)
            .is_none_or(|last| now - last >= GREET_COOLDOWN_MS)
        {
            s.greeted.insert(*id, now);
        }
    }
    s.greeted
        .retain(|id, t| ids.contains(id) || now - *t < GREET_COOLDOWN_MS);
    if !ids.is_empty() {
        c.go_to_talk();
    }
}

// ===========================================================================================
// Common behaviors
// ===========================================================================================

fn alert_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    check_pipe_name(c, k);
    npc::stop_movement(c.server, &c.npc);
    broadcast_posture(c, false);
    if let Some(t) = current_target(c) {
        let r = c.obj.transform();
        c.st(k).old_rotation = (r.rot_x, r.rot_y, r.rot_z);
        let tp = npc::world_pos(c.server, &t);
        npc::look_towards(c.server, &c.npc, tp, 4);
    }
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::ALERT);
    super::npc_events::in_alert(c);
    let s = c.st(k);
    s.pipe_name = "phase_dragon_ground".into();
    s.enter = true;
}

fn alert_tick(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if !c.st(k).enter {
        return; // not initialized yet Enter()
    }
    if let Some(t) = current_target(c) {
        let tp = npc::world_pos(c.server, &t);
        npc::look_towards(c.server, &c.npc, tp, 4);
    }
    let skill_task = c.npc.data().unit.skill_state.skill_task.is_some();
    if now_ms() > c.ai.alert_end_time && !skill_task {
        let (x, y, z) = c.st(k).old_rotation;
        c.obj.with_transform(|t| {
            t.rot_x = x;
            t.rot_y = y;
            t.rot_z = z;
        });
        set_target(c, None);
        c.go_to_idle();
        return;
    }
    check_aggression(c, k);
}

fn almighty_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    c.st(k).skill_queue.clear();
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    c.st(k).combat_start_time = now_ms();
    if !c.npc.data().unit.is_in_battle {
        super::npc_events::on_combat_started(c);
    }
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

fn almighty_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::Almighty { ai_skill_lists, .. } = &params.kind else {
        return;
    };
    c.st(k).ai_params = Some(params.clone());
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    check_pipe_name(c, k);
    if !can_use_skill(c, k) {
        return;
    }
    c.st(k).strafe_during_delay = false;
    // Will delay for 150 Milliseconds to eliminate the hanging of the skill
    if !c.npc_check_interval(NpcIntervalSite::AlmightyAttackBehavior, 150) {
        tracing::trace!("Skill: CooldownTime [150]!");
        return;
    }
    if c.st(k).skill_queue.is_empty() && !refresh_skill_queue(c, k, ai_skill_lists, &params) {
        return;
    }
    let Some(selected) = c.st(k).skill_queue.pop_front() else {
        return;
    };
    let Some(skill) = ai_skills::new_skill(c, selected.skill_id) else {
        return;
    };
    let target = current_target(c);
    use_skill(c, k, skill, target.as_ref(), selected.delay);
    c.st(k).strafe_during_delay = selected.strafe;
}

fn almighty_attack_exit(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // Experimental handling of guards returning to their home position after chasing somebody
    let aggro_empty = c.npc.data().unit.skill_state.aggro_table.is_empty();
    if aggro_empty && c.ai.path_handler.ai_path_points_remaining.is_empty() {
        c.ai.path_handler.target_position = Vec3::default();
        npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
        npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
        let home = c.ai.home_position;
        c.enqueue_path_point(AiPathPoint {
            action: AiPathPointAction::Speed,
            param: "3".into(),
            position: home,
        });
        c.go_to_follow_path();
    }
    c.st(k).enter = false;
}

fn attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    set_in_battle(c.server, &c.obj, true);
    super::npc_events::on_combat_started(c);
    c.st(k).enter = true;
}

fn attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    let Some(target) = current_target(c) else {
        return;
    };
    if can_strafe(c, k) && !is_using_skill(c) {
        move_in_range(c, k, Some(&target), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    // Will delay for 150 Milliseconds to eliminate the hanging of the skill
    if !c.npc_check_interval(NpcIntervalSite::AttackBehavior, 150) {
        tracing::trace!("Skill: CooldownTime [150]!");
        return;
    }
    let target = current_target(c).unwrap_or(target);
    let target_dist = npc::distance_to(c.server, &c.obj, &target, false);
    pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
}

fn dead_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::stop_movement(c.server, &c.npc);
    combat::npc_clear_all_aggro(c);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    // The set is over (release); a window still open on the dying NPC is closed, as for a
    // despawn (INFERRED, see PORTING.md WP-5b).
    super::interaction::release(c);
    super::interaction::end_open_interactions(c, c.npc.obj_id);
    super::npc_events::on_death(c);
    c.st(k).enter = true;
}

fn despawning_enter(c: &mut AiCtx<'_>) {
    super::interaction::release(c);
    super::interaction::end_open_interactions(c, c.npc.obj_id);
    super::npc_events::on_despawn(c);
}

fn follow_path_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    c.st(k).skill_queue.clear();
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    c.st(k).combat_start_time = now_ms();
    if !c.npc.data().unit.is_in_battle {
        super::npc_events::on_combat_started(c);
    }
    c.ai.param = template_ai_params(c);
    c.npc.data().unit.is_in_patrol = true;
    if !c.ai.has_simulation {
        // Ai.Owner.Simulation is null for NPCs not spawned by a spawner: NullReferenceException.
        tracing::error!(
            "FollowPathBehavior.Enter: npc {} has no Simulation (NullReferenceException in C#)",
            c.npc.obj_id
        );
        return;
    }
    // Simulation.MoveToPathEnabled = true; Simulation.GoToPath(owner, true): GoToPath toggles
    // MoveToPathEnabled back to false and runs StopMove (StopMovement + IsInPatrol = false).
    npc::stop_movement(c.server, &c.npc);
    c.npc.data().unit.is_in_patrol = false;
    // A target carried over from an interrupted run gets a fresh stall window.
    let pos = owner_pos(c);
    let include_z = c.npc.data().can_fly;
    c.ai.path_handler
        .reset_target_progress(pos, include_z, now_ms());
    c.st(k).enter = true;
}

fn follow_path_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !update_target(c) {
        set_target(c, None);
    }
    if check_aggression(c, k) {
        return;
    }
    if check_alert(c, k) {
        return;
    }
    // If still aggro, go back to combat
    let (in_battle, aggro_empty) = {
        let n = c.npc.data();
        (
            n.unit.is_in_battle,
            n.unit.skill_state.aggro_table.is_empty(),
        )
    };
    if in_battle && !aggro_empty {
        c.go_to_combat();
        return;
    }
    if !run_current_path(c, delta_ms) {
        c.go_to_idle();
        return;
    }
    let h = &c.ai.path_handler;
    let t = h.target_position;
    if t.x == 0.0
        && t.y == 0.0
        && t.z == 0.0
        && h.ai_path_points.is_empty()
        && h.ai_path_points_remaining.is_empty()
        // A sentinel just sent us back to RunCommandSet (the C# then overrode that with
        // `GoToIdle`, stranding the rest of the command set).
        && c.ai.current_behavior() == Some(k)
    {
        c.go_to_idle();
    }
}

/// `AiPathHandler.RunCurrentPath(delta)`.
fn run_current_path(c: &mut AiCtx<'_>, delta_ms: i64) -> bool {
    let pos = owner_pos(c);
    let scale = template_scalars(c).scale;
    // Ground walkers follow the terrain; only flying NPCs move along the authored Z.
    let include_z = c.npc.data().can_fly;
    match c
        .ai
        .path_handler
        .run_current_path_at(pos, scale, include_z, now_ms())
    {
        PathStep::ReturnToCommandSet => {
            c.go_to_run_command_set();
            true
        }
        PathStep::Move {
            target,
            speed,
            actor_flags,
            result,
        } => {
            let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(speed));
            move_speed *= delta_seconds(delta_ms);
            // Stop within the same radius that counts as an arrival (`Template.Scale`, at least
            // the movement default 1 m); a smaller tolerance kept the NPC short of the point.
            npc_move_towards(c, target, move_speed as f32, actor_flags, scale.max(1.0));
            // Move the idle "home" location along with the path
            c.ai.idle_position = owner_pos(c);
            result
        }
        PathStep::Idle(r) => r,
        PathStep::Stalled {
            target,
            path,
            result,
        } => {
            tracing::warn!(
                "NPC {}: follow path '{}' target ({}, {}, {}) made no progress for {} ms, skipping it",
                c.npc.obj_id,
                if path.is_empty() { "?" } else { &path },
                target.x,
                target.y,
                target.z,
                TARGET_STALL_TIMEOUT_MS
            );
            result
        }
        PathStep::Unsupported(msg) => {
            tracing::error!("{msg} (NotSupportedException)");
            true
        }
    }
}

fn follow_unit_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !update_target(c) {
        set_target(c, None);
    }
    if check_aggression(c, k) {
        return;
    }
    if check_alert(c, k) {
        return;
    }
    let follow = c.ai.follow_unit.clone();
    let alive = follow.as_ref().is_some_and(|f| {
        f.with_unit(|u| u.unit().hp > 0 && !u.unit().is_dead())
            .unwrap_or(false)
    });
    let Some(follow) = follow.filter(|_| alive) else {
        c.ai.follow_unit = None;
        // A `FollowUnit` AI command's set ends with the follow (INFERRED, see PORTING.md).
        super::interaction::release(c);
        c.go_to_idle();
        return;
    };
    let target_distance = npc::distance_to(c.server, &c.obj, &follow, true);
    let follow_speed_multiplier = (f64::from(target_distance) / 1.5).min(5.0) as f32;
    let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)))
        * f64::from(follow_speed_multiplier);
    let move_flags = npc::real_movement_flags(move_speed);
    move_speed *= delta_seconds(delta_ms);
    let fp = npc::world_pos(c.server, &follow);
    npc_move_towards(c, fp, move_speed as f32, move_flags, 1.0);
    c.ai.idle_position = owner_pos(c);
}

/// `Ai.Owner.InterruptSkills()` of the settling behaviors (`Idle`, `HoldPosition`). Deviation: a
/// tower defense controller keeps the wave plot it cast on spawn (`OnSpawn` `np_skills`), which
/// runs for seconds while the AI settles from `Spawning` through `RunCommandSet` into
/// `Idle`/`HoldPosition`; the C# interrupts it there too, so no wave ever appeared (PORTING.md,
/// tower defense).
fn interrupt_unless_event_controller(c: &AiCtx<'_>) {
    if !crate::systems::instances::tower_def::owns_npc(c.server, &c.npc) {
        ai_skills::interrupt_skills(c);
    }
}

fn hold_position_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    interrupt_unless_event_controller(c);
    npc::stop_movement(c.server, &c.npc);
    // Ai.Owner.CurrentTarget = Ai.Owner (no packet)
    c.npc.data().unit.current_target = c.npc.obj_id;
    let s = c.st(k);
    s.last_skill_check = now_ms();
    s.following_npc = false;
    s.initialized = true;
}

fn hold_position_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).initialized {
        tracing::warn!(
            "HoldPositionBehavior.Tick called before initialization for unit {}",
            c.npc.obj_id
        );
        return;
    }
    if !throttle(c, k) {
        return;
    }
    // 1. Combat states
    if check_aggression(c, k) {
        c.st(k).following_npc = false;
        return;
    }
    if check_alert(c, k) {
        c.st(k).following_npc = false;
        return;
    }
    // 2. A group follower's `formation_GoToOnPoint` movement replaces the hold-position walk;
    // its idle activity (self-skills, follow-default, talk) still runs below (1.2
    // `hold_position.UpdateHoldPosition`).
    super::group::follower_tick(c, delta_ms);
    // 3. Try to use idle self-skills (1 s between tries)
    let now = now_ms();
    if now - c.st(k).last_skill_check >= 1000 {
        c.st(k).last_skill_check = now;
        if let Some(t) = current_target(c) {
            let dist = npc::distance_to(c.server, &c.obj, &t, false);
            let me = c.obj.clone();
            pick_skill_and_use_it(c, k, SkillUseConditionKind::InIdle, &me, dist);
        }
    }
    // 4. Optional follow default NPC
    if !c.st(k).following_npc && c.do_follow_default_nearest_npc() {
        c.st(k).following_npc = true;
    }
    // 5. Talk has low priority
    check_for_talk(c, k);
}

fn idle_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // Stop all actions and reset state
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    interrupt_unless_event_controller(c);
    npc::stop_movement(c.server, &c.npc);
    set_target(c, None);
    if let Some(t) = current_target(c) {
        if let Some(p) = t.as_player() {
            p.send(SCAggroTargetChangedPacket {
                npc_id: c.npc.obj_id,
                target_id: 0,
            });
        }
    }
    super::npc_events::in_idle(c);
    c.st(k).initialized = true;
}

fn idle_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).initialized {
        tracing::warn!(
            "IdleBehavior.Tick called before initialization for unit {}",
            c.npc.obj_id
        );
        return;
    }
    if !throttle(c, k) {
        return;
    }
    // Try to use a skill if possible
    if let Some(t) = current_target(c) {
        let dist = npc::distance_to(c.server, &c.obj, &t, false);
        let me = c.obj.clone();
        pick_skill_and_use_it(c, k, SkillUseConditionKind::InIdle, &me, dist);
    }
    // Check for state transitions
    if check_aggression(c, k) {
        c.go_to_combat();
    } else if check_alert(c, k) {
        c.go_to_alert();
    } else if check_follow_path(c) {
        c.go_to_follow_path();
    } else {
        // A group follower's movement is the formation follow (1.2 `idle.UpdateIdle`); its idle
        // activity still runs.
        super::group::follower_tick(c, delta_ms);
        // Try to follow the nearest NPC, otherwise go to default behavior
        if c.do_follow_default_nearest_npc() {
            return;
        }
        c.go_to_default_behavior();
    }
    // Talk has low priority
    check_for_talk(c, k);
}

fn return_state_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // Ending a fight (the abuser died, left or was cleared) must let a later fight against the
    // same object share with the group again.
    c.npc.data().npc_group_last_share_abuser = 0;
    // A group follower returns to its formation point, not its own spawn (1.2
    // `idle` / `hold_position` re-anchor a member's idle position on the leader).
    super::group::sync_return_to_formation(c);
    if !c.npc.data().unit.skill_state.aggro_table.is_empty() {
        combat::npc_clear_all_aggro(c);
    }
    set_target(c, None);
    set_in_battle(c.server, &c.obj, false);
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    broadcast_posture(c, false);
    buffs::add_buff_by_id(c.server, &c.obj, npc::BUFF_NPC_RETURN, &c.obj, 0);
    let restore =
        c.ai.param
            .as_ref()
            .is_none_or(|p| p.restoration_on_return());
    if restore {
        let (hp, max_hp, max_mp) = {
            let n = c.npc.data();
            (n.unit.hp, n.unit.max_hp, n.unit.max_mp)
        };
        skills_combat::post_update_current_hp(
            c.server,
            &c.obj,
            Some(&c.obj),
            hp,
            max_hp,
            KillReason::Unknown,
        );
        {
            let mut n = c.npc.data();
            n.unit.hp = max_hp;
            n.unit.mp = max_mp;
        }
        npc::broadcast_packet(
            c,
            &SCUnitPointsPacket {
                obj_id: c.npc.obj_id,
                hp: max_hp,
                mp: max_mp,
            },
        );
    }
    if c.ai
        .param
        .as_ref()
        .is_some_and(|p| p.always_teleport_on_return)
    {
        on_completed_return(c);
        return;
    }
    if c.ai.param.as_ref().is_some_and(|p| !p.go_return_state()) {
        on_completed_return_no_teleport(c);
    } else {
        // Not for an NPC that was just sent idle.
        plan_return_route(c);
    }
    let s = c.st(k);
    s.timeout_time = now_ms() + 20_000;
    s.enter = true;
}

/// The straight way home crosses a wall: walk the navmesh route instead (deviation from the
/// C#, see `PORTING.md`). Without a route (no collision service, `GeoDataMode` off, no
/// navmesh) the NPC walks straight and the 20 s timeout teleport remains the backstop.
fn plan_return_route(c: &mut AiCtx<'_>) {
    reset_chase_path(c);
    if !geo_data_mode(c) || moves_freely(c) {
        return;
    }
    let idle = c.ai.idle_position;
    if wall_ahead(c, idle).is_none() {
        return;
    }
    let steps = super::geodata::plan_route(c, idle);
    tracing::debug!(
        npc = c.npc.obj_id,
        steps = c.ai.path_node.found_path.len(),
        raw = steps,
        "return route planned around a wall"
    );
}

fn return_state_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    // The leader may have moved since the fight ended: recompute the formation point from its
    // current position every tick instead of freezing the spot chosen at ReturnState entry.
    super::group::sync_return_to_formation(c);
    let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
    let move_flags = npc::real_movement_flags(move_speed);
    move_speed *= delta_seconds(delta_ms);
    let idle = c.ai.idle_position;
    // Corners of the planned detour first (see `plan_return_route`); reached in XY like the
    // chase waypoints.
    let mut goal = idle;
    let pos = owner_pos(c);
    while let Some(next) = c.ai.path_node.found_path.front().copied() {
        if calculate_distance(pos, next, false) > 0.25 {
            goal = next;
            break;
        }
        c.ai.path_node.found_path.pop_front();
    }
    let detour = goal != idle;
    let arrived = npc_move_towards(
        c,
        goal,
        move_speed as f32,
        move_flags,
        if detour { 0.25 } else { 1.0 },
    ) && !detour;
    let can_fly = c.npc.data().can_fly;
    // Flyers return through the air: the ground 2D distance would end the return while the
    // NPC is still below its home altitude at the same XY.
    let distance_to_idle = calculate_distance(idle, owner_pos(c), can_fly);
    // MoveTowards reports arrival at its 1 m tolerance; completing only below 1 m would leave
    // a flyer that stopped exactly 1 m above its home stuck in ReturnState.
    if arrived || distance_to_idle < 1.0 {
        on_completed_return_no_teleport(c);
        return;
    }
    if now_ms() > c.st(k).timeout_time {
        on_completed_return(c);
    }
}

fn on_completed_return(c: &mut AiCtx<'_>) {
    let idle = c.ai.idle_position;
    let can_fly = c.npc.data().can_fly;
    // The timeout teleport counts vertical displacement: a flyer below its home at the same
    // XY is still far away and must be lifted back to the idle position.
    let distance_to_idle = calculate_distance(idle, owner_pos(c), can_fly);
    if distance_to_idle > 2.0 * 2.0 {
        npc_move_towards(c, idle, 1_000_000.0, 4, 1.0);
        npc::stop_movement(c.server, &c.npc);
    }
    on_completed_return_no_teleport(c);
}

fn on_completed_return_no_teleport(c: &mut AiCtx<'_>) {
    reset_chase_path(c);
    c.go_to_idle();
}

fn roaming_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !check_aggression(c, k) {
        check_alert(c, k);
    }
    // A group follower that already left Roaming (combat / alert) must not move here; ordinary
    // NPCs keep the original control flow (no early return outside the feature).
    if crate::systems::npcs::npc::group::is_follower(&c.npc) && c.ai.current_behavior() != Some(k) {
        return;
    }
    // A group follower never roams on its own: it follows its move leader's formation point
    // (1.2 `roaming.UpdateRoaming`).
    if super::group::follower_tick(c, delta_ms) {
        return;
    }
    let zero = |v: Vec3| v.x == 0.0 && v.y == 0.0 && v.z == 0.0;
    if zero(c.st(k).roam_target) && now_ms() > c.st(k).next_roaming {
        match choose_roaming_position(c) {
            Some(next) => {
                c.st(k).roam_target = next;
                broadcast_posture(c, false);
            }
            // Walled in: skip this roam cycle (deviation from the C#, see PORTING.md).
            None => c.st(k).next_roaming = now_ms() + rand::thread_rng().gen_range(3..6) * 1000,
        }
    }
    let target = c.st(k).roam_target;
    if zero(target) {
        return;
    }
    let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
    let move_flags = npc::real_movement_flags(move_speed);
    move_speed *= delta_seconds(delta_ms);
    npc_move_towards(c, target, move_speed as f32, move_flags, 1.0);
    let dist = calculate_distance(owner_pos(c), target, false);
    if dist < 1.0 {
        npc::stop_movement(c.server, &c.npc);
        let s = c.st(k);
        s.roam_target = Vec3::default();
        s.next_roaming = now_ms() + rand::thread_rng().gen_range(3..6) * 1000;
        broadcast_posture(c, true);
    }
}

/// How far in front of a wall a shortened walk stops (m).
const WALL_MARGIN: f32 = 0.6;
/// A shortened roam walk must still be at least this long (m); roaming ends below 1 m.
const MIN_SHORT_WALK: f32 = 1.5;
/// Roam targets tried before the move is shortened or the cycle skipped.
const ROAM_CANDIDATES: usize = 4;

/// The horizontal distance to the first wall on the straight walk from the NPC to `to`, or
/// `None` when the way is clear, the NPC moves freely (flyers, swimmers) or the collision
/// service is off (the C# straight walk). The check runs once per chosen target / route
/// segment, not per movement step.
fn wall_ahead(c: &AiCtx<'_>, to: Vec3) -> Option<f32> {
    if moves_freely(c) {
        return None;
    }
    let from = owner_pos(c);
    super::collision::walk::wall_distance(
        c.server,
        c.obj.transform().instance_id,
        [from.x, from.y, from.z],
        [to.x, to.y, to.z],
    )
    .flatten()
}

/// `calc_next_roaming_position` that never picks a target behind a wall: a few random
/// candidates, else the walk to the farthest one cut short before its wall, else `None`
/// (skip this roam cycle). Without the collision service the first candidate is taken, as in
/// the C#.
fn choose_roaming_position(c: &AiCtx<'_>) -> Option<Vec3> {
    let mut best: Option<(f32, Vec3)> = None;
    for _ in 0..ROAM_CANDIDATES {
        let cand = calc_next_roaming_position(c);
        let Some(wall) = wall_ahead(c, cand) else {
            return Some(cand);
        };
        if best.is_none_or(|(d, _)| wall > d) {
            best = Some((wall, cand));
        }
    }
    let (wall, cand) = best?;
    let from = owner_pos(c);
    let stop = wall - WALL_MARGIN;
    let total = calculate_distance(from, cand, false);
    if stop < MIN_SHORT_WALK || total <= 0.0 {
        return None;
    }
    let f = stop / total;
    let (x, y) = (
        from.x + (cand.x - from.x) * f,
        from.y + (cand.y - from.y) * f,
    );
    let (can_fly, spawner_z, zone) = {
        let n = c.npc.data();
        (n.can_fly, n.spawner_z, n.unit.transform.zone_id)
    };
    let z = npc::reference_height_in(
        c.server,
        c.obj.transform().instance_id,
        Some(crate::systems::npcs::heightmap::ReferenceHeightAi {
            can_fly,
            hold_or_idle: false,
            spawner_z,
            world_z: from.z,
        }),
        x,
        y,
        from.z,
        zone,
    );
    Some(Vec3::new(x, y, z))
}

/// `AiUtils.CalcNextRoamingPosition(ai)` (taken from x2ai.lua).
pub fn calc_next_roaming_position(c: &AiCtx<'_>) -> Vec3 {
    let max_roaming_distance = 6.0f32;
    let idle = c.ai.idle_position;
    let mut rng = rand::thread_rng();
    let x = (rng.gen::<f32>() - 0.5) * max_roaming_distance * 2.0 + idle.x;
    let y = (rng.gen::<f32>() - 0.5) * max_roaming_distance * 2.0 + idle.y;
    // Get terrain height at new position
    let (can_fly, spawner_z, zone) = {
        let n = c.npc.data();
        (n.can_fly, n.spawner_z, n.unit.transform.zone_id)
    };
    let z = npc::reference_height_in(
        c.server,
        c.obj.transform().instance_id,
        Some(crate::systems::npcs::heightmap::ReferenceHeightAi {
            can_fly,
            hold_or_idle: matches!(c.ai.current_behavior(), Some(B::HoldPosition | B::Idle)),
            spawner_z,
            world_z: owner_pos(c).z,
        }),
        x,
        y,
        idle.z,
        zone,
    );
    Vec3::new(x, y, z)
}

fn run_command_set_tick(c: &mut AiCtx<'_>, delta_ms: i64) {
    // If everything is executed, and still time remaining, wait for it before going back
    if c.ai.current_command_run_time_ms > 0 {
        c.ai.current_command_run_time_ms -= delta_ms;
        return;
    }
    // If there are commands in the AI Command queue, execute those first
    if c.ai.current_command.is_some() || !c.ai.commands_queue.is_empty() {
        if c.ai.current_command.is_none() {
            c.ai.current_command = c.ai.commands_queue.pop_front();
            c.ai.current_command_start_time = now_ms();
        }
        tick_current_ai_command(c, delta_ms);
        return;
    }
    super::interaction::release(c);
    c.go_to_idle();
}

/// `FollowUnit`'s search radius for the fallback target (INFERRED from
/// `DoFollowNearestNpc(..., 100f)`).
const FOLLOW_UNIT_RANGE: f32 = 100.0;

/// The nearest player within `range` of the NPC (`DoFollowNearestNpc`'s search, filtered to
/// players).
fn nearest_player(c: &AiCtx<'_>, range: f32) -> Option<WorldObject> {
    let mut nearest: Option<(f32, WorldObject)> = None;
    for o in npc::around(c.server, &c.obj, range, true) {
        if o.as_player().is_none() {
            continue;
        }
        let dist = npc::distance_to(c.server, &c.obj, &o, false);
        if nearest.as_ref().is_none_or(|(closest, _)| dist < *closest) {
            nearest = Some((dist, o));
        }
    }
    nearest.map(|(_, o)| o)
}

/// `RunCommandSetBehavior.TickCurrentAiCommand(aiCommand, delta)`.
fn tick_current_ai_command(c: &mut AiCtx<'_>, delta_ms: i64) {
    use super::params::AiCommandCategory as Cmd;
    if c.ai.current_command_run_time_ms < 0 {
        c.ai.current_command = None;
        c.ai.current_command_run_time_ms = 0;
        return;
    }
    // Check if we're still waiting
    if c.ai.current_command_run_time_ms > 0 {
        c.ai.current_command_run_time_ms -= delta_ms;
        return;
    }
    let Some(cmd) = c.ai.current_command.clone() else {
        return;
    };
    tracing::debug!(
        "{} executing AI Command: {:?}, CommandSet: {}, P1: {}, P2: {}",
        c.npc.obj_id,
        cmd.cmd_id,
        cmd.cmd_set_id,
        cmd.param1,
        cmd.param2
    );
    // WP-5b: `ai_command_sets.can_interact` (INFERRED rule, see PORTING.md).
    super::interaction::apply_command_set(c, cmd.cmd_set_id);
    match cmd.cmd_id {
        Cmd::FollowUnit => {
            // `ai.follow_unit` (set by the quest bridge or an earlier command) wins; otherwise
            // follow the nearest player within 100 m, like `DoFollowNearestNpc`. `param1` is 1
            // in all three shipped rows and its meaning is UNKNOWN: log it and ignore it.
            tracing::debug!(
                "NPC {}: FollowUnit param1 {} is UNKNOWN and ignored",
                c.npc.obj_id,
                cmd.param1
            );
            if c.ai.follow_unit.is_none() {
                c.ai.follow_unit = nearest_player(c, FOLLOW_UNIT_RANGE);
            }
            if c.ai.follow_unit.is_some() {
                c.go_to_follow_unit();
            }
        }
        Cmd::FollowPath => {
            // The command names its own route. The C# loaded `AiFileName` (the *previous*
            // command's name) before assigning `Param2`, so the first FollowPath of a set
            // loaded nothing, every later one loaded the previous route and the last route
            // was never loaded at all.
            let name = if cmd.param2.trim().is_empty() {
                c.ai.ai_file_name.clone()
            } else {
                cmd.param2.clone()
            };
            c.load_ai_path_points(&name, cmd.param1 == 1);
            if cmd.param1 == 1 {
                c.enqueue_path_point(AiPathPoint {
                    position: Vec3::default(),
                    action: AiPathPointAction::ReturnToCommandSet,
                    param: String::new(),
                });
            }
            c.go_to_follow_path();
            if cmd.param1 == 1 {
                // An empty Param2 walked the current route (see above): keep its name.
                if !cmd.param2.trim().is_empty() {
                    c.ai.ai_file_name = cmd.param2.clone();
                }
            } else {
                c.ai.ai_file_name2 = cmd.param2.clone();
            }
        }
        Cmd::UseSkill => {
            c.ai.ai_skill_id = cmd.param1;
            let skill_id = c.ai.ai_skill_id;
            if let Some(tpl) = c.server.game_data.systems.skills.skill(skill_id).cloned() {
                let target = current_target(c)
                    .filter(WorldObject::is_unit)
                    .unwrap_or_else(|| c.obj.clone());
                let r = crate::systems::skills::skill::unit_use_skill(
                    c.server, &c.obj, skill_id, &target,
                );
                if r == skill_result::SUCCESS {
                    // The AI casts through `unit_use_skill`, whose NPC skills run at the
                    // unit level (`ab_level_override` / level): pace the command with the
                    // same effective level the cast uses.
                    let level =
                        crate::systems::skills::skill::ab_level(c.server, &c.obj, tpl.ability_id);
                    let cool_down = crate::systems::skills::skill::attack_delay(
                        c.server, &tpl, level, &c.obj, false, 0.0,
                    );
                    c.ai.current_command_run_time_ms = cool_down as i64;
                }
            }
        }
        Cmd::Timeout => {
            c.ai.ai_time_out = cmd.param1;
            c.ai.current_command_run_time_ms = i64::from(c.ai.ai_time_out);
        }
        Cmd::Other(v) => {
            tracing::error!("AI command {v}: NotSupportedException");
            return;
        }
    }
    if c.ai.current_command_run_time_ms == 0 {
        c.ai.current_command_run_time_ms = -1000;
    }
}

fn spawning_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    super::npc_events::on_spawn(c);
    if let Some(p) = template_ai_params(c) {
        if let AiParamsKind::Almighty { .. } = p.kind {
            if p.alert_to_attack && p.alert_duration == 0.0 {
                check_aggression(c, k);
            }
        }
    }
    c.st(k).enter = true;
}

fn spawning_tick(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if !c.st(k).enter {
        return;
    }
    if !c.st(k).used_spawn_skills {
        let skills = ai_skills::template_skills_if_any(c, SkillUseConditionKind::OnSpawn);
        if let Some(skills) = skills {
            c.st(k).used_spawn_skills = true;
            for id in skills {
                let Some(skill) = ai_skills::new_skill(c, id) else {
                    tracing::error!("Spawning: OnSpawn skill {id} has no template");
                    continue;
                };
                let mut t = SkillCastTarget::Unit(c.npc.obj_id);
                ai_skills::use_skill(c, &skill, SkillCaster::Unit(c.npc.obj_id), &mut t, true);
            }
        }
    }
    // This follows the game's way of doing it.
    c.go_to_run_command_set();
}

fn talk_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::stop_movement(c.server, &c.npc);
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    c.st(k).initialized = true;
}

fn talk_tick(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if !c.st(k).initialized {
        tracing::warn!(
            "TalkBehavior.Tick called before initialization for unit {}",
            c.npc.obj_id
        );
        return;
    }
    if !throttle(c, k) {
        return;
    }
    let now = now_ms();
    let greeted = c.st(k).greeted.clone();
    let players = players_in_range(c, GREET_RANGE, GREET_FOV, &greeted, GREET_COOLDOWN_MS);
    for p in &players {
        let id = p.obj_id();
        let fresh = c
            .st(k)
            .greeted
            .get(&id)
            .is_none_or(|last| now - last >= GREET_COOLDOWN_MS);
        if fresh {
            send_greeting(c, p);
            spawn_spyglass_if_needed(c);
            c.st(k).greeted.insert(id, now);
        }
    }
    let ids: Vec<u32> = players.iter().map(WorldObject::obj_id).collect();
    c.st(k)
        .greeted
        .retain(|id, t| ids.contains(id) || now - *t < GREET_COOLDOWN_MS);
    if players.is_empty() {
        c.go_to_default_behavior();
    }
}

/// `TalkBehavior.SendGreeting(player)`.
fn send_greeting(c: &mut AiCtx<'_>, player: &WorldObject) {
    let Some(p) = player.as_player() else { return };
    let npc_template_id = c.npc.data().unit.template_id as i32;
    let Some((event_name, weight)) = determine_event_type(c, player) else {
        return;
    };
    if event_name.is_empty() {
        // `eventType == null` is never true for the C# tuple ("" falls through); GetEvent with
        // an empty name finds nothing.
        return;
    }
    let data = &c.server.game_data.systems.ai.ai;
    let Some(selected) = data.event(npc_template_id, &event_name, weight).cloned() else {
        return;
    };
    let Some(bubble) = data.chat_bubble(selected.id).cloned() else {
        return;
    };
    let (npc_name, faction, template_id) = {
        let n = c.npc.data();
        (n.unit.name.clone(), n.unit.faction_id, n.unit.template_id)
    };
    let msg = SCNpcChatMessagePacket {
        chat_type: 0, // ChatType.White
        faction_id: faction,
        npc_obj_id: c.npc.obj_id,
        npc_name: crate::systems::npcs::npc::packets::npc_chat_name(
            c.server,
            &npc_name,
            template_id,
        ),
        character_obj_id: p.obj_id,
        kind: 1,
        type_: bubble.id as u32,
        message: String::new(),
    };
    let snd = super::packets::SCPlaySoundPacket {
        kind: 2,
        bubble_id: bubble.id as u32,
        npc_obj_id: c.npc.obj_id,
    };
    crate::systems::skills::broadcast(c.server, player, &msg, true);
    crate::systems::skills::broadcast(c.server, player, &snd, true);
    if selected.skill_id > 0 {
        crate::systems::skills::skill::unit_use_skill(
            c.server,
            &c.obj,
            selected.skill_id as u32,
            player,
        );
    }
    tracing::debug!(
        "NPC {npc_template_id} sent {event_name}:{weight} #{} to {}",
        bubble.id,
        p.obj_id
    );
}

/// `TalkBehavior.DetermineEventType(player)`.
fn determine_event_type(c: &AiCtx<'_>, player: &WorldObject) -> Option<(String, f32)> {
    let npc_id = c.npc.data().unit.template_id as i32;
    let lvl = player.as_player().map_or(0, |p| p.data().unit.level);
    let data = &c.server.game_data.systems.ai.ai;
    // 1. Enemy check
    let relation = crate::systems::skills::relation::relation_to(
        c.server,
        &crate::systems::skills::geometry::Target::Obj(c.obj.clone()),
        &crate::systems::skills::geometry::Target::Obj(player.clone()),
    );
    use crate::systems::skills::types::RelationState as R;
    let friendly = matches!(relation, R::Friendly | R::Neutral);
    if can_attack(c.server, &c.obj, player)
        && !friendly
        && !data.events(npc_id, "OnEnemySeen").is_empty()
    {
        return Some(("OnEnemySeen".into(), 500.0));
    }
    // 2. All records for NPC
    let mut records: Vec<(&str, f32)> = Vec::new();
    for name in ["OnClientGreeting", "OnFriendNearSeen"] {
        for ev in data.events(npc_id, name) {
            records.push((name, ev.weight));
        }
    }
    // 3. Determine maximum allowed weight based on level
    let max_weight = match lvl {
        l if l < 15 => 500.0,
        l if l < 30 => 300.0,
        l if l < 45 => 200.0,
        _ => 60.0,
    };
    // 4. Get the entry with the highest weight not exceeding maxWeight (first of equals)
    let mut best: Option<(&str, f32)> = None;
    for r in records.into_iter().filter(|r| r.1 <= max_weight) {
        if best.is_none_or(|b| r.1 > b.1) {
            best = Some(r);
        }
    }
    Some(best.map_or((String::new(), 0.0), |(n, w)| (n.to_string(), w)))
}

/// `TalkBehavior.SpawnSpyglassIfNeeded()`.
fn spawn_spyglass_if_needed(c: &mut AiCtx<'_>) {
    const VEHICLE_NICK_ID: i32 = 22;
    const SPYGLASS_ID: u32 = 6129;
    const SPYGLASS_DIST: f32 = 1.5;
    if template_scalars(c).npc_nickname_id != VEHICLE_NICK_ID {
        return;
    }
    let t = c.obj.transform();
    // pos = Transform.CloneDetached(); pos.Local.AddDistanceToFront(1.5)
    let (fx, fy) = crate::systems::skills::controllers::add_distance_to_front(
        SPYGLASS_DIST,
        t.x,
        t.y,
        t.rot_z + std::f32::consts::FRAC_PI_2,
    );
    // yaw = CalculateAngleFrom(pos, Owner.Transform) (degrees, used as is)
    let yaw = crate::systems::skills::controllers::calculate_angle_from(
        Vec3::new(fx, fy, t.z),
        Vec3::new(t.x, t.y, t.z),
    ) as f32;
    super::hooks::spawn_spyglass(c.server, &c.obj, SPYGLASS_ID, fx, fy, t.z, yaw);
}

// ===========================================================================================
// Archer
// ===========================================================================================

fn archer_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    {
        let s = c.st(k);
        s.archer_phase = ArcherPhase::Base;
        s.make_a_gap_count = 0;
    }
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    set_in_battle(c.server, &c.obj, true);
    super::npc_events::on_combat_started(c);
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

fn archer_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::ArcherRoaming));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::Archer { combat_skills } = &params.kind else {
        return;
    };
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if c.st(k).archer_phase == ArcherPhase::NeedMakeAGap {
        let idle = c.ai.idle_position;
        let me = owner_pos(c);
        let abuser = current_target(c).map_or(Vec3::default(), |t| npc::world_pos(c.server, &t));
        let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
        let move_flags = npc::real_movement_flags(move_speed);
        move_speed *= delta_seconds(delta_ms);
        npc_move_towards(c, idle, move_speed as f32, move_flags, 1.0);
        let dist = calculate_distance(me, idle, true);
        let dist2 = calculate_distance(me, abuser, true);
        if dist < 1.0 || dist2 > params.prefered_combat_dist {
            npc::stop_movement(c.server, &c.npc);
            let s = c.st(k);
            s.make_a_gap_count += 1;
            s.archer_phase = ArcherPhase::TryingRangedDefSkill;
        } else {
            return;
        }
    } else if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    c.st(k).max_weapon_range = params.prefered_combat_dist;
    let target_dist = match current_target(c) {
        Some(t) => npc::distance_to(c.server, &c.obj, &t, false),
        None => 0.0,
    };
    let list = archer_request_available_skills(c, k, &params, combat_skills, target_dist);
    let selected = archer_pick_skill(c, &list);
    let Some(skill) = ai_skills::new_skill(c, selected) else {
        return;
    };
    let casting = skill.template.casting_time as f32;
    let target = current_target(c);
    use_skill(c, k, skill, target.as_ref(), casting);
}

/// `ArcherAttackBehavior.OnUseSkillDone()`.
fn archer_on_use_skill_done(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let s = c.st(k);
    s.archer_phase = match s.archer_phase {
        ArcherPhase::TryingMeleeSkill => ArcherPhase::UsedMeleeSkill,
        ArcherPhase::TryingRangedDefSkill => ArcherPhase::UsedRangedDefSkill,
        ArcherPhase::TryingMakeAGapSkill | ArcherPhase::NeedMakeAGap => ArcherPhase::NeedMakeAGap,
        _ => ArcherPhase::Base,
    };
}

/// `ArcherAttackBehavior.RequestAvailableSkills(aiParams, trgDist)` (OnRequestSkillInfo).
fn archer_request_available_skills(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    params: &AiParams,
    base: &[super::params::ArcherCombatSkill],
    trg_dist: f32,
) -> Vec<u32> {
    let in_melee = trg_dist <= params.melee_attack_range;
    let mut list = Vec::new();
    let collect =
        |c: &mut AiCtx<'_>, f: fn(&super::params::ArcherCombatSkill) -> &Vec<u32>| -> Vec<u32> {
            let mut out = Vec::new();
            for acs in base {
                for id in f(acs) {
                    if !ai_skills::on_cooldown(c, *id) {
                        out.push(*id);
                    }
                }
            }
            out
        };
    match c.st(k).archer_phase {
        ArcherPhase::UsedMeleeSkill => {
            let need_make_a_gap =
                in_melee && c.st(k).make_a_gap_count < params.max_make_a_gap_count;
            if need_make_a_gap {
                list = collect(c, |a| &a.make_a_gap);
                c.st(k).archer_phase = ArcherPhase::TryingMakeAGapSkill;
            } else {
                c.st(k).archer_phase = ArcherPhase::Base;
                // call again with phase change ("base")
                return archer_request_available_skills(c, k, params, base, trg_dist);
            }
        }
        ArcherPhase::UsedRangedDefSkill => list = collect(c, |a| &a.ranged_strong),
        _ => {}
    }
    if list.is_empty() {
        if in_melee {
            list = collect(c, |a| &a.melee);
            c.st(k).archer_phase = ArcherPhase::TryingMeleeSkill;
        } else {
            list = collect(c, |a| &a.ranged_def);
            c.st(k).archer_phase = ArcherPhase::TryingRangedDefSkill;
        }
    }
    archer_on_use_skill_done(c, k);
    list
}

/// `ArcherAttackBehavior.PickSkill(skills)`.
fn archer_pick_skill(c: &mut AiCtx<'_>, skills: &[u32]) -> u32 {
    if !skills.is_empty() {
        return skills[rand::thread_rng().gen_range(0..skills.len())];
    }
    let base = template_scalars(c).base_skill_id as u32;
    if !ai_skills::on_cooldown(c, base) {
        return base;
    }
    0 // no melee Skill
}

// ===========================================================================================
// BigMonster
// ===========================================================================================

fn big_monster_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    set_in_battle(c.server, &c.obj, true);
    super::npc_events::on_combat_started(c);
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

fn big_monster_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::BigMonsterRoaming));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::BigMonster { combat_skills } = &params.kind else {
        return;
    };
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    c.st(k).strafe_during_delay = false;
    let target = current_target(c);
    let target_dist = target
        .as_ref()
        .map_or(0.0, |t| npc::distance_to(c.server, &c.obj, t, false));
    // RequestAvailableSkills
    let hr = health_ratio(c);
    let available: Vec<super::params::BigMonsterCombatSkill> = combat_skills
        .iter()
        .filter(|s| s.health_range_min <= hr && hr <= s.health_range_max)
        .filter(|s| !ai_skills::on_cooldown(c, s.skill_type))
        .filter(|s| {
            c.server
                .game_data
                .systems
                .skills
                .skill(s.skill_type)
                .is_some_and(|tpl| {
                    (target_dist >= tpl.min_range as f32 && target_dist <= tpl.max_range as f32)
                        || ai_skills::is_self_target(tpl)
                })
        })
        .cloned()
        .collect();
    // PickSkill
    let t = template_scalars(c);
    let selected = if !available.is_empty() {
        Some(available[rand::thread_rng().gen_range(0..available.len())].clone())
    } else if !ai_skills::on_cooldown(c, t.base_skill_id as u32) {
        Some(super::params::BigMonsterCombatSkill {
            skill_type: t.base_skill_id as u32,
            skill_delay: t.base_skill_delay,
            strafe_during_delay: t.base_skill_strafe,
            ..Default::default()
        })
    } else {
        None
    };
    let Some(selected) = selected else {
        // If skill list is empty, get Base skill
        if let Some(target) = target {
            pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
        }
        return;
    };
    let Some(skill) = ai_skills::new_skill(c, selected.skill_type) else {
        return;
    };
    use_skill(c, k, skill, target.as_ref(), selected.skill_delay);
    c.st(k).strafe_during_delay = selected.strafe_during_delay;
}

// ===========================================================================================
// Flytrap
// ===========================================================================================

fn flytrap_alert_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::ALERT);
    broadcast_posture(c, false);
    super::npc_events::in_alert(c);
    c.st(k).enter = true;
}

fn flytrap_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    super::npc_events::on_combat_started(c);
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

fn flytrap_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::Flytrap));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    if !matches!(params.kind, AiParamsKind::Flytrap { .. }) {
        return;
    }
    c.st(k).ai_params = Some(params.clone());
    if !flytrap_update_target(c) {
        c.on_no_aggro_target();
        return;
    }
    let Some(target) = current_target(c) else {
        return;
    };
    // if (Ai.Owner.Gimmick?.CurrentTarget != null) MoveInRange(Ai.Owner.Gimmick.CurrentTarget, delta)
    if let Some(g) = super::gimmick::npc_gimmick(c.server, &c.obj) {
        let gt = g.gimmick.data().unit.current_target;
        if gt != 0 {
            if let Some(t) = c.server.world.object(gt) {
                super::gimmick::flytrap_move_in_range(c, &g, &t, delta_ms);
            }
        }
    }
    set_in_battle(c.server, &c.obj, true);
    let target_dist = npc::distance_to(c.server, &c.obj, &target, false);
    pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
    flytrap_update(c, k);
}

/// `FlytrapAttackBehavior.UpdateTarget()`.
fn flytrap_update_target(c: &mut AiCtx<'_>) -> bool {
    for abuser in sorted_abusers(c).into_iter().flatten() {
        let ap = npc::world_pos(c.server, &abuser);
        npc::look_towards(c.server, &c.npc, ap, 4);
        if c.ai.already_targeted {
            return true;
        }
        if npc::unit_is_visible(c.server, &c.obj, &abuser) && !npc::is_dead(&abuser) {
            npc::set_current_aggro_target(&c.npc, Some(&abuser));
            set_target(c, Some(&abuser));
            update_aggro_help(c, &abuser, 200.0);
            if geo_data_mode(c) {
                find_path(c, &abuser, "new target");
            }
            return true;
        }
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
    }
    match current_target(c) {
        Some(t) if t.is_unit() => {
            let hp = t.with_unit(|u| u.unit().hp).unwrap_or(0);
            if hp <= 0 || npc::is_dead(&t) {
                set_target(c, None);
            }
        }
        _ => set_target(c, None),
    }
    false
}

/// `FlytrapAttackBehavior.Update()`.
fn flytrap_update(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let Some(abuser) = current_target(c) else {
        return;
    };
    let Some(params) = c.st(k).ai_params.clone() else {
        return;
    };
    let AiParamsKind::Flytrap {
        attack_end_distance,
        ..
    } = params.kind
    else {
        return;
    };
    let abuser_pos = npc::world_pos(c.server, &abuser);
    let current = owner_pos(c);
    let idle = c.ai.idle_position;
    let dist_sq = |a: Vec3, b: Vec3| {
        let dx = f64::from(a.x) - f64::from(b.x);
        let dy = f64::from(a.y) - f64::from(b.y);
        (dx * dx + dy * dy) as f32
    };
    // Check out of idle pos
    let teleport =
        c.ai.param
            .as_ref()
            .is_some_and(|p| p.always_teleport_on_return);
    if teleport && dist_sq(current, idle) > 3.0 * 3.0 {
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
        c.on_no_aggro_target();
        return;
    }
    // Check that some target was gone out from attack end distance
    if dist_sq(abuser_pos, idle) > attack_end_distance * attack_end_distance {
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
        c.on_no_aggro_target();
    }
}

// ===========================================================================================
// WildBoar
// ===========================================================================================

fn wild_boar_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    c.ai.param = template_ai_params(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    super::npc_events::on_combat_started(c);
    set_in_battle(c.server, &c.obj, true);
    c.st(k).enter = true;
}

fn wild_boar_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::WildBoarRoaming));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::WildBoar {
        on_combat_start_skills,
        on_spurt_skills,
    } = &params.kind
    else {
        return;
    };
    {
        let (hp, max) = {
            let n = c.npc.data();
            (n.unit.hp, n.unit.max_hp)
        };
        c.st(k).curr_health = hp as f32 / max as f32 * 100.0;
    }
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    if !c.st(k).combat_start_skill {
        // On Combat Start Skill. Execute once
        let start = on_combat_start_skills.first().copied().unwrap_or(0);
        if start != 0 {
            npc::stop_movement(c.server, &c.npc);
            if let Some(skill) = ai_skills::new_skill(c, start) {
                let target = current_target(c);
                use_skill(c, k, skill, target.as_ref(), 0.0);
            }
            c.st(k).combat_start_skill = true;
        }
    }
    // Spurt or base?
    let Some(target) = current_target(c) else {
        return;
    };
    let target_dist = npc::distance_to(c.server, &c.obj, &target, false);
    if on_spurt_skills.is_empty() {
        pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
        return;
    }
    for skill_data in on_spurt_skills {
        if c.st(k).curr_health < skill_data.health_condition as f32 {
            let Some(skill) = ai_skills::new_skill(c, skill_data.skill_type) else {
                continue;
            };
            if target_dist >= skill.template.min_range as f32
                && target_dist <= skill.template.max_range as f32
            {
                set_weapon_range(c, k, &skill);
                let result = use_skill(c, k, skill, Some(&target), 0.0);
                if result == skill_result::COOLDOWN_TIME {
                    pick_skill_and_use_it(
                        c,
                        k,
                        SkillUseConditionKind::InCombat,
                        &target,
                        target_dist,
                    );
                }
            }
        } else {
            pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn npc_faces_its_attack_target_before_angle_validation_and_keeps_self_skill_heading() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::data::GameData;
        use crate::models::npc::Npc;
        use crate::systems::skills::data::SkillTemplate;
        use crate::systems::skills::state::SkillRun;
        use crate::test_support::test_server_with;
        use crate::world::Entity;

        for target_type in [5, 0] {
            let server = Arc::new(test_server_with(GameData::empty()));
            server.set_self();
            crate::systems::skills::bind(&server);
            let mut caster = Npc::new(1);
            caster.unit.hp = 1000;
            caster.unit.transform.set_xyz(100.0, 100.0, 100.0);
            let caster = Entity::new(0x300, caster);
            let mut target = Npc::new(2);
            target.unit.hp = 1000;
            target.unit.transform.set_xyz(105.0, 100.0, 100.0);
            let target = WorldObject::from(Entity::new(0x301, target));
            let world = server.world.world(0).unwrap();
            world.spawn(&caster, &server.view_ctx());
            world.spawn(&target, &server.view_ctx());
            let mut ai = NpcAi::new(AiKind::AlmightyNpc, &caster);
            let mut context = AiCtx {
                server: &server,
                npc: caster.clone(),
                obj: WorldObject::from(&caster),
                ai: &mut ai,
            };
            let skill = SkillRun::new(
                Arc::new(SkillTemplate {
                    id: 100,
                    target_type,
                    target_angle: 90,
                    max_range: 30,
                    ..Default::default()
                }),
                1,
            );
            assert_eq!(
                use_skill(
                    &mut context,
                    BehaviorKind::AlmightyAttack,
                    skill,
                    Some(&target),
                    0.0
                ),
                skill_result::SUCCESS
            );
            let yaw = caster.data().unit.transform.rot_z;
            if target_type == 5 {
                assert!((yaw + std::f32::consts::FRAC_PI_2).abs() < 0.001);
            } else {
                assert_eq!(yaw, 0.0);
            }
        }
    }

    #[test]
    fn skill_list_health_and_time_windows() {
        // no ranges: always
        assert!(health_and_time_ok(50, 0.0, 0, 0, 0.0, 0.0));
        // health window (min exclusive, max inclusive)
        assert!(!health_and_time_ok(40, 0.0, 40, 90, 0.0, 0.0));
        assert!(health_and_time_ok(90, 0.0, 40, 90, 0.0, 0.0));
        // time window [5, 0] = from 5 s on
        assert!(!health_and_time_ok(50, 4.0, 0, 100, 5.0, 0.0));
        assert!(health_and_time_ok(50, 6.0, 0, 100, 5.0, 0.0));
        // time window [5, 140]
        assert!(health_and_time_ok(50, 140.0, 0, 100, 5.0, 140.0));
        assert!(!health_and_time_ok(50, 141.0, 0, 100, 5.0, 140.0));
        // negative start: never (C# falls through every branch)
        assert!(!health_and_time_ok(50, 1.0, 0, 0, -1.0, 0.0));
    }

    #[test]
    fn delta_uses_the_millisecond_component() {
        assert_eq!(delta_seconds(120), 0.12);
        assert_eq!(delta_seconds(1120), 0.12);
    }

    /// `update_aggro_help` walks the linked NPCs in region order. An AI-less NPC used to end the
    /// loop (the C# NullReferenceException), so the NPCs behind it never helped and the error
    /// repeated on every tick. It must be skipped with one report, and the next NPC must help.
    #[tokio::test]
    async fn aggro_help_skips_an_ai_less_npc_and_still_helps_the_next() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::data::GameData;
        use crate::models::npc::Npc;
        use crate::systems::npcs::npc::data::NpcTemplate;
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
        use crate::world::Entity;

        const CASTER: u32 = 0x2E1;
        const NO_AI: u32 = 0x2E2;
        const HELPER: u32 = 0x2E3;
        const ABUSER: u32 = 0x2E4;

        let mut data = GameData::empty();
        for id in [7000u32, 7001, 7002] {
            data.systems.npcs.npc.templates.insert(
                id,
                NpcTemplate {
                    id,
                    aggression: true,
                    attack_start_range_scale: 1.0,
                    accept_aggro_link: true,
                    aggro_link_help_dist: 30.0,
                    ..NpcTemplate::default()
                },
            );
        }
        let server = Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::skills::bind(&server);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Tank", 98.0, 100.0),
            ABUSER,
        );
        let world = server.world.world(0).unwrap();

        let mut caster = Npc::new(7000);
        caster.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let caster = Entity::new(CASTER, caster);
        world.spawn(&caster, &server.view_ctx());
        // First in the region list on purpose.
        let mut ai_less = Npc::new(7002);
        ai_less.unit.transform.set_xyz(101.0, 100.0, 100.0);
        let ai_less = Entity::new(NO_AI, ai_less);
        world.spawn(&ai_less, &server.view_ctx());
        assert!(!super::super::manager::has_ai(&server, NO_AI));
        let mut helper = Npc::new(7001);
        helper.unit.transform.set_xyz(102.0, 100.0, 100.0);
        let helper = Entity::new(HELPER, helper);
        server.systems.ai.ais.add_ai(&helper, AiKind::HoldPosition);
        world.spawn(&helper, &server.view_ctx());

        let abuser = WorldObject::from(&player);
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, &caster);
        let mut context = AiCtx {
            server: &server,
            npc: caster.clone(),
            obj: WorldObject::from(&caster),
            ai: &mut ai,
        };
        let lines = warnings(|| {
            update_aggro_help(&mut context, &abuser, 200.0);
            // A second tick: the AI-less NPC is reported once, and the helper keeps its aggro.
            update_aggro_help(&mut context, &abuser, 200.0);
        });

        assert!(
            helper
                .data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&player.obj_id),
            "the NPC after the AI-less one must still receive aggro help"
        );
        assert!(
            !caster
                .data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&player.obj_id),
            "the caster does not add itself"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("has no AI") && l.contains(&NO_AI.to_string()))
                .count(),
            1,
            "the AI-less NPC must be reported once, got {lines:?}"
        );
    }

    /// An NPC template that refuses template-driven aggro help (the `update_aggro_help` rules).
    fn link_only_template(
        id: u32,
        accept_aggro_link: bool,
        aggression: bool,
        aggro_link_help_dist: f32,
    ) -> crate::systems::npcs::npc::data::NpcTemplate {
        crate::systems::npcs::npc::data::NpcTemplate {
            id,
            aggression,
            attack_start_range_scale: 1.0,
            accept_aggro_link,
            aggro_link_help_dist,
            ..Default::default()
        }
    }

    /// Spawns an NPC with a `HoldPosition` AI at `(x, 100, 100)` so it can receive aggro help.
    fn spawn_link_npc(
        server: &Arc<crate::server::GameServer>,
        obj_id: u32,
        template_id: u32,
        x: f32,
    ) -> crate::world::NpcRef {
        use super::super::framework::AiKind;
        use crate::models::npc::Npc;
        use crate::world::Entity;

        let mut npc = Npc::new(template_id);
        npc.unit.transform.set_xyz(x, 100.0, 100.0);
        let npc = Entity::new(obj_id, npc);
        server.systems.ai.ais.add_ai(&npc, AiKind::HoldPosition);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        npc
    }

    /// `aggro_links` group membership makes a member help another member of the same group even
    /// when its own template rules (`accept_aggro_link` false, non-aggressive with rule 0)
    /// would not; a non-member with the same template rules keeps the old behaviour.
    #[tokio::test]
    async fn aggro_link_members_help_each_other_despite_template_rules() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::data::GameData;
        use crate::models::npc::Npc;
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
        use crate::world::Entity;

        const LEADER: u32 = 0x320;
        const NO_ACCEPT: u32 = 0x321;
        const NO_AGGRESSION: u32 = 0x322;
        const NON_MEMBER: u32 = 0x323;
        const ABUSER: u32 = 0x324;

        let mut data = GameData::empty();
        data.systems
            .npcs
            .npc
            .templates
            .insert(7000, link_only_template(7000, true, true, 30.0));
        data.systems
            .npcs
            .npc
            .templates
            .insert(7001, link_only_template(7001, false, true, 30.0));
        data.systems
            .npcs
            .npc
            .templates
            .insert(7002, link_only_template(7002, true, false, 30.0));
        data.systems
            .npcs
            .npc
            .templates
            .insert(7003, link_only_template(7003, false, true, 30.0));
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups
            .insert(500, "herd".into());
        for id in [7000u32, 7001, 7002] {
            data.systems
                .npcs
                .npc
                .aggro_links
                .groups_by_npc
                .insert(id, vec![500]);
        }
        let server = Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::skills::bind(&server);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Tank", 98.0, 100.0),
            ABUSER,
        );

        // The leader is the NPC that just got the abuser as its target.
        let mut leader = Npc::new(7000);
        leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let leader = Entity::new(LEADER, leader);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&leader, &server.view_ctx());
        let no_accept = spawn_link_npc(&server, NO_ACCEPT, 7001, 105.0);
        let no_aggression = spawn_link_npc(&server, NO_AGGRESSION, 7002, 110.0);
        let non_member = spawn_link_npc(&server, NON_MEMBER, 7003, 115.0);

        let abuser = WorldObject::from(&player);
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, &leader);
        let mut context = AiCtx {
            server: &server,
            npc: leader.clone(),
            obj: WorldObject::from(&leader),
            ai: &mut ai,
        };
        update_aggro_help(&mut context, &abuser, 200.0);

        let has_aggro = |npc: &crate::world::NpcRef| {
            npc.data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&player.obj_id)
        };
        assert!(
            has_aggro(&no_accept),
            "a linked member must help even with accept_aggro_link=false"
        );
        assert!(
            has_aggro(&no_aggression),
            "a linked member must help even when its template rule 0 needs aggression"
        );
        assert!(
            !has_aggro(&non_member),
            "a non-member with accept_aggro_link=false must keep refusing"
        );
        assert!(
            !has_aggro(&leader),
            "the linked NPC must not add aggro to itself"
        );
    }

    /// The link only bypasses the template rules; the helper's own `aggro_link_help_dist` still
    /// bounds the distance.
    #[tokio::test]
    async fn aggro_link_help_respects_the_helpers_distance() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::data::GameData;
        use crate::models::npc::Npc;
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
        use crate::world::Entity;

        const LEADER: u32 = 0x330;
        const NEAR: u32 = 0x331;
        const FAR: u32 = 0x332;
        const ABUSER: u32 = 0x333;

        let mut data = GameData::empty();
        for id in [7100u32, 7101, 7102] {
            data.systems
                .npcs
                .npc
                .templates
                .insert(id, link_only_template(id, false, false, 30.0));
        }
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups
            .insert(501, "guard".into());
        for id in [7100u32, 7101, 7102] {
            data.systems
                .npcs
                .npc
                .aggro_links
                .groups_by_npc
                .insert(id, vec![501]);
        }
        let server = Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::skills::bind(&server);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Tank", 98.0, 100.0),
            ABUSER,
        );

        let mut leader = Npc::new(7100);
        leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let leader = Entity::new(LEADER, leader);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&leader, &server.view_ctx());
        let near = spawn_link_npc(&server, NEAR, 7101, 120.0);
        let far = spawn_link_npc(&server, FAR, 7102, 160.0);

        let abuser = WorldObject::from(&player);
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, &leader);
        let mut context = AiCtx {
            server: &server,
            npc: leader.clone(),
            obj: WorldObject::from(&leader),
            ai: &mut ai,
        };
        update_aggro_help(&mut context, &abuser, 200.0);

        let has_aggro = |npc: &crate::world::NpcRef| {
            npc.data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&player.obj_id)
        };
        assert!(
            has_aggro(&near),
            "a member within the helper distance helps"
        );
        assert!(
            !has_aggro(&far),
            "a member beyond aggro_link_help_dist must not help"
        );
    }

    /// An NPC in two `aggro_links` groups helps a member of either group.
    #[tokio::test]
    async fn aggro_link_member_of_two_groups_helps_members_of_both() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::data::GameData;
        use crate::models::npc::Npc;
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
        use crate::world::Entity;

        const LEADER_A: u32 = 0x340;
        const LEADER_B: u32 = 0x341;
        const HELPER: u32 = 0x342;
        const ABUSER_A: u32 = 0x343;
        const ABUSER_B: u32 = 0x344;

        let mut data = GameData::empty();
        for id in [7200u32, 7201, 7202] {
            data.systems
                .npcs
                .npc
                .templates
                .insert(id, link_only_template(id, false, false, 50.0));
        }
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups
            .insert(500, "herd".into());
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups
            .insert(501, "guard".into());
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups_by_npc
            .insert(7200, vec![500]);
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups_by_npc
            .insert(7201, vec![501]);
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups_by_npc
            .insert(7202, vec![500, 501]);

        let server = Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::skills::bind(&server);
        let client_a = TestClient::new(&server, 1, 1001);
        let client_b = TestClient::new(&server, 2, 1002);
        let player_a = spawn_player(
            &server,
            &client_a,
            test_character(1, "TankA", 98.0, 100.0),
            ABUSER_A,
        );
        let player_b = spawn_player(
            &server,
            &client_b,
            test_character(2, "TankB", 98.0, 100.0),
            ABUSER_B,
        );

        let mut leader_a = Npc::new(7200);
        leader_a.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let leader_a = Entity::new(LEADER_A, leader_a);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&leader_a, &server.view_ctx());
        let mut leader_b = Npc::new(7201);
        leader_b.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let leader_b = Entity::new(LEADER_B, leader_b);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&leader_b, &server.view_ctx());
        let helper = spawn_link_npc(&server, HELPER, 7202, 120.0);

        let mut ai_a = NpcAi::new(AiKind::AlmightyNpc, &leader_a);
        let mut context_a = AiCtx {
            server: &server,
            npc: leader_a.clone(),
            obj: WorldObject::from(&leader_a),
            ai: &mut ai_a,
        };
        update_aggro_help(&mut context_a, &WorldObject::from(&player_a), 200.0);
        let mut ai_b = NpcAi::new(AiKind::AlmightyNpc, &leader_b);
        let mut context_b = AiCtx {
            server: &server,
            npc: leader_b.clone(),
            obj: WorldObject::from(&leader_b),
            ai: &mut ai_b,
        };
        update_aggro_help(&mut context_b, &WorldObject::from(&player_b), 200.0);

        let table = helper.data().unit.skill_state.aggro_table.clone();
        assert!(
            table.contains_key(&player_a.obj_id),
            "the helper must help its group 500 member"
        );
        assert!(
            table.contains_key(&player_b.obj_id),
            "the helper must help its group 501 member"
        );
    }

    /// The link-group path must not relax the template sight gate for non-members. The C#
    /// `UpdateAggroHelp` special-rule branch needs `aggro_link_sight_check` enabled (Behavior.cs,
    /// `a2ab80abd^`), so a rule-4 non-member with the flag off does not help; the same template
    /// with the flag on still does (control). Only the link path may ignore the flag.
    #[tokio::test]
    async fn aggro_link_does_not_relax_the_template_sight_gate_for_non_members() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::data::GameData;
        use crate::models::npc::Npc;
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
        use crate::world::Entity;

        const LEADER: u32 = 0x350;
        const NO_SIGHT: u32 = 0x351;
        const WITH_SIGHT: u32 = 0x352;
        const ABUSER: u32 = 0x353;

        let mut data = GameData::empty();
        data.systems
            .npcs
            .npc
            .templates
            .insert(7300, link_only_template(7300, true, true, 30.0));
        data.systems.npcs.npc.templates.insert(
            7301,
            crate::systems::npcs::npc::data::NpcTemplate {
                aggro_link_special_rule_id: 4,
                aggro_link_sight_check: false,
                ..link_only_template(7301, true, true, 30.0)
            },
        );
        data.systems.npcs.npc.templates.insert(
            7302,
            crate::systems::npcs::npc::data::NpcTemplate {
                aggro_link_special_rule_id: 4,
                aggro_link_sight_check: true,
                ..link_only_template(7302, true, true, 30.0)
            },
        );
        let server = Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::skills::bind(&server);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Tank", 98.0, 100.0),
            ABUSER,
        );

        let mut leader = Npc::new(7300);
        leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let leader = Entity::new(LEADER, leader);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&leader, &server.view_ctx());
        let no_sight = spawn_link_npc(&server, NO_SIGHT, 7301, 105.0);
        let with_sight = spawn_link_npc(&server, WITH_SIGHT, 7302, 110.0);

        let abuser = WorldObject::from(&player);
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, &leader);
        let mut context = AiCtx {
            server: &server,
            npc: leader.clone(),
            obj: WorldObject::from(&leader),
            ai: &mut ai,
        };
        update_aggro_help(&mut context, &abuser, 200.0);

        let has_aggro = |npc: &crate::world::NpcRef| {
            npc.data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&player.obj_id)
        };
        assert!(
            !has_aggro(&no_sight),
            "a non-member with aggro_link_sight_check off must keep refusing special-rule help"
        );
        assert!(
            has_aggro(&with_sight),
            "a non-member with the sight check on still helps through rule 4"
        );
    }

    // ---- `aggro_link_special_guard` city-guard help -------------------------------------------

    const GUARD_FACTION: u32 = 162;
    const NATION_FACTION: u32 = 101;
    const PIRATE_FACTION: u32 = 161;
    const MONSTER_FACTION: u32 = 115;
    const GUARD_TEMPLATE: u32 = 7400;
    const PLAIN_TEMPLATE: u32 = 7401;
    const MONSTER_TEMPLATE: u32 = 7402;

    /// Guard fixture: `nation_guard_help` is the `system_factions.guard_help` switch of the
    /// victim's faction; the guard faction is hostile to the pirate and monster factions (as in
    /// the shipped `system_faction_relations`, 162 vs 161 / 115 = hostile). The guard template
    /// mirrors the city guards (faction 162, rule 4 everyone, sight check, 6 m help distance,
    /// `ignore_npc_attacker`).
    fn guard_data(nation_guard_help: bool) -> crate::data::GameData {
        use crate::data::world_data::{FactionRelation, SystemFaction};
        let mut data = crate::data::GameData::empty();
        for (id, guard_help) in [
            (NATION_FACTION, nation_guard_help),
            (GUARD_FACTION, true),
            (PIRATE_FACTION, true),
            (MONSTER_FACTION, false),
        ] {
            data.factions.add(SystemFaction {
                id,
                guard_help,
                ..SystemFaction::default()
            });
        }
        for (a, b) in [
            (GUARD_FACTION, PIRATE_FACTION),
            (GUARD_FACTION, MONSTER_FACTION),
            (PIRATE_FACTION, NATION_FACTION),
            (MONSTER_FACTION, NATION_FACTION),
        ] {
            data.factions.relations.push(FactionRelation {
                id: a,
                id2: b,
                state: crate::systems::skills::types::RelationState::Hostile as u8,
                exp_time: 0,
            });
        }
        data.systems.npcs.npc.templates.insert(
            GUARD_TEMPLATE,
            crate::systems::npcs::npc::data::NpcTemplate {
                id: GUARD_TEMPLATE,
                faction_id: GUARD_FACTION,
                aggression: true,
                accept_aggro_link: true,
                aggro_link_special_guard: true,
                aggro_link_special_ignore_npc_attacker: true,
                aggro_link_special_rule_id: 4,
                aggro_link_sight_check: true,
                aggro_link_help_dist: 6.0,
                attack_start_range_scale: 1.0,
                ..Default::default()
            },
        );
        // The same shape without `aggro_link_special_guard` (the control NPC).
        data.systems.npcs.npc.templates.insert(
            PLAIN_TEMPLATE,
            crate::systems::npcs::npc::data::NpcTemplate {
                id: PLAIN_TEMPLATE,
                faction_id: GUARD_FACTION,
                aggression: true,
                accept_aggro_link: true,
                aggro_link_special_rule_id: 4,
                aggro_link_sight_check: true,
                aggro_link_help_dist: 6.0,
                attack_start_range_scale: 1.0,
                ..Default::default()
            },
        );
        // The shipped data has special guards without the 0x40 flag (38 of 107), so an NPC
        // attacker can pull some of them; keep the cheap data-level skip out of these tests.
        data.systems.npcs.npc.special_guards_allow_npc_attacker = true;
        data.systems.npcs.npc.templates.insert(
            MONSTER_TEMPLATE,
            crate::systems::npcs::npc::data::NpcTemplate {
                id: MONSTER_TEMPLATE,
                faction_id: MONSTER_FACTION,
                aggression: true,
                attack_start_range_scale: 1.0,
                ..Default::default()
            },
        );
        data
    }

    /// Applies one fixed 10-damage `DamageEffect` from `caster` to `target` (the real damage
    /// pipeline, which is what triggers the guard help).
    fn apply_hit(server: &GameServer, caster: &WorldObject, target: &WorldObject) {
        use crate::systems::skills::effects::{EffectArgs, EffectSource};
        use crate::systems::skills::geometry::Target;
        use crate::systems::skills::types::{
            CastAction, SkillCastTarget, SkillCaster, SkillObject,
        };
        let args = EffectArgs {
            caster: Some(caster.clone()),
            caster_caster: SkillCaster::Unit(caster.obj_id()),
            target: Target::Obj(target.clone()),
            target_caster: SkillCastTarget::Unit(target.obj_id()),
            cast: CastAction::Skill {
                skill_id: 1,
                tl_id: 0,
            },
            source: EffectSource::default(),
            skill_object: &SkillObject::None,
        };
        let effect = crate::systems::skills::tests::damage_effect(1, 10);
        crate::systems::skills::effects::combat::damage(server, &effect, &args, None);
    }

    fn guard_fixture(
        nation_guard_help: bool,
    ) -> (
        Arc<GameServer>,
        crate::world::PlayerRef,
        crate::world::PlayerRef,
        crate::world::NpcRef,
    ) {
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

        let server = Arc::new(test_server_with(guard_data(nation_guard_help)));
        server.set_self();
        crate::systems::skills::bind(&server);
        let victim_client = TestClient::new(&server, 1, 1001);
        let attacker_client = TestClient::new(&server, 2, 1002);
        let mut victim_char = test_character(1, "Victim", 100.0, 100.0);
        victim_char.unit.hp = 1000;
        victim_char.unit.max_hp = 1000;
        victim_char.unit.faction_id = NATION_FACTION;
        let victim = spawn_player(&server, &victim_client, victim_char, 0x401);
        let mut attacker_char = test_character(2, "Attacker", 102.0, 100.0);
        attacker_char.unit.hp = 1000;
        attacker_char.unit.max_hp = 1000;
        attacker_char.unit.faction_id = PIRATE_FACTION;
        let attacker = spawn_player(&server, &attacker_client, attacker_char, 0x402);
        let guard = spawn_link_npc(&server, 0x403, GUARD_TEMPLATE, 103.0);
        guard.data().unit.faction_id = GUARD_FACTION;
        (server, victim, attacker, guard)
    }

    fn has_aggro_on(npc: &crate::world::NpcRef, obj_id: u32) -> bool {
        npc.data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&obj_id)
    }

    /// A piracy attack on a protected-faction player near a city guard pulls the guard onto the
    /// attacker (10.x `sub_39321C80`, Z:823528-823950). Driven through the damage effect.
    #[tokio::test]
    async fn a_special_guard_helps_a_player_attacked_by_a_pirate() {
        let (server, victim, attacker, guard) = guard_fixture(true);
        apply_hit(
            &server,
            &WorldObject::from(&attacker),
            &WorldObject::from(&victim),
        );
        assert!(
            has_aggro_on(&guard, attacker.obj_id),
            "the guard must join the fight against the pirate attacker"
        );
        assert!(
            !has_aggro_on(&guard, victim.obj_id),
            "the guard must not aggro the victim it protects"
        );
    }

    /// `aggro_link_special_ignore_npc_attacker` (0x40): a monster attacking a player does not
    /// pull the city guard, which keeps attacking monsters it sees on its own (unchanged).
    #[tokio::test]
    async fn an_ignoring_guard_is_not_pulled_by_a_monster_attacking_a_player() {
        use crate::models::npc::Npc;
        use crate::world::Entity;
        let (server, victim, _attacker, guard) = guard_fixture(true);
        let mut mob = Npc::new(MONSTER_TEMPLATE);
        mob.unit.transform.set_xyz(102.0, 100.0, 100.0);
        mob.unit.faction_id = MONSTER_FACTION;
        let mob = Entity::new(0x404, mob);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&mob, &server.view_ctx());
        apply_hit(
            &server,
            &WorldObject::from(&mob),
            &WorldObject::from(&victim),
        );
        assert!(
            !has_aggro_on(&guard, mob.obj_id),
            "a guard with ignore_npc_attacker must ignore a monster attacker"
        );
    }

    /// A duel between the victim and the attacker is not helped (10.x `sub_39322ED0`,
    /// Z:823876).
    #[tokio::test]
    async fn a_duel_does_not_pull_the_guard() {
        let (server, victim, attacker, guard) = guard_fixture(true);
        server
            .systems
            .social
            .duels
            .duel_request(&server, &attacker, victim.character_id);
        assert!(
            server
                .systems
                .social
                .duels
                .is_in_duel_with(attacker.character_id, victim.character_id),
            "fixture: the duel request must register both sides"
        );
        apply_hit(
            &server,
            &WorldObject::from(&attacker),
            &WorldObject::from(&victim),
        );
        assert!(
            !has_aggro_on(&guard, attacker.obj_id),
            "a duel must not pull the city guard"
        );
    }

    /// The victim's `system_factions.guard_help` is the switch (10.x Z:823905-823909).
    #[tokio::test]
    async fn a_faction_without_guard_help_does_not_pull_the_guard() {
        let (server, victim, attacker, guard) = guard_fixture(false);
        apply_hit(
            &server,
            &WorldObject::from(&attacker),
            &WorldObject::from(&victim),
        );
        assert!(
            !has_aggro_on(&guard, attacker.obj_id),
            "a victim faction with guard_help=false must not pull the guard"
        );
    }

    /// Only `aggro_link_special_guard` templates answer the player-victim scan; a plain NPC in
    /// the same spot stays out of the fight (the control for the four tests above).
    #[tokio::test]
    async fn a_non_special_npc_is_not_pulled_by_the_guard_scan() {
        let (server, victim, attacker, _guard) = guard_fixture(true);
        let plain = spawn_link_npc(&server, 0x405, PLAIN_TEMPLATE, 103.0);
        plain.data().unit.faction_id = GUARD_FACTION;
        apply_hit(
            &server,
            &WorldObject::from(&attacker),
            &WorldObject::from(&victim),
        );
        assert!(
            !has_aggro_on(&plain, attacker.obj_id),
            "a template without aggro_link_special_guard must not join"
        );
    }

    /// NPC victims keep the existing C# help path: a guard attacked by a monster still pulls a
    /// nearby fellow guard (the player-victim guard gate must not override it).
    #[tokio::test]
    async fn a_guard_attacked_by_a_monster_pulls_a_nearby_fellow_guard() {
        use super::super::framework::{AiKind, NpcAi};
        use crate::models::npc::Npc;
        use crate::test_support::{test_server_with, TestClient};
        use crate::world::Entity;

        let server = Arc::new(test_server_with(guard_data(true)));
        server.set_self();
        crate::systems::skills::bind(&server);
        let _client = TestClient::new(&server, 1, 1001);

        let mut victim = Npc::new(GUARD_TEMPLATE);
        victim.unit.transform.set_xyz(100.0, 100.0, 100.0);
        victim.unit.faction_id = GUARD_FACTION;
        let victim = Entity::new(0x406, victim);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&victim, &server.view_ctx());
        let fellow = spawn_link_npc(&server, 0x407, GUARD_TEMPLATE, 103.0);
        fellow.data().unit.faction_id = GUARD_FACTION;
        let mut mob = Npc::new(MONSTER_TEMPLATE);
        mob.unit.transform.set_xyz(101.0, 100.0, 100.0);
        mob.unit.faction_id = MONSTER_FACTION;
        let mob = Entity::new(0x408, mob);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&mob, &server.view_ctx());

        let abuser = WorldObject::from(&mob);
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, &victim);
        let mut context = AiCtx {
            server: &server,
            npc: victim.clone(),
            obj: WorldObject::from(&victim),
            ai: &mut ai,
        };
        update_aggro_help(&mut context, &abuser, 200.0);

        assert!(
            has_aggro_on(&fellow, mob.obj_id),
            "a fellow guard must still answer a guard attacked by a monster"
        );
    }

    /// The player-victim scan is rate-limited per `(victim, abuser)` pair: a second hit within
    /// the interval does not rescan, an expired stamp lets the next hit pick up a newly arrived
    /// guard.
    #[tokio::test]
    async fn guard_help_scans_are_rate_limited_per_pair() {
        let (server, victim, attacker, guard) = guard_fixture(true);
        let abuser = WorldObject::from(&attacker);
        let target = WorldObject::from(&victim);
        apply_hit(&server, &abuser, &target);
        assert!(has_aggro_on(&guard, attacker.obj_id), "first scan helps");

        // A guard spawned after the first scan is not reached by an immediate second hit.
        let late = spawn_link_npc(&server, 0x409, GUARD_TEMPLATE, 104.0);
        late.data().unit.faction_id = GUARD_FACTION;
        apply_hit(&server, &abuser, &target);
        assert!(
            !has_aggro_on(&late, attacker.obj_id),
            "a second hit within one second must not rescan"
        );

        // Expire the pair's stamp (what one second of fighting does) and hit again.
        server.systems.ai.guard_help_scans.lock().unwrap().insert(
            (victim.obj_id, attacker.obj_id),
            crate::world::now_ms() - GUARD_HELP_SCAN_INTERVAL_MS,
        );
        apply_hit(&server, &abuser, &target);
        assert!(
            has_aggro_on(&late, attacker.obj_id),
            "an expired stamp must allow the next scan to help the late guard"
        );
    }

    /// The duel exception resolves pets / slaves to their owner: a duellist's mate attacking
    /// the other duellist must not pull the guard.
    #[tokio::test]
    async fn a_duellists_pet_attacker_does_not_pull_the_guard() {
        use crate::models::mate::Mate;
        use crate::world::Entity;

        let (server, victim, attacker, guard) = guard_fixture(true);
        server
            .systems
            .social
            .duels
            .duel_request(&server, &attacker, victim.character_id);
        let mut mate = Mate::new(0x409, 1);
        mate.owner_obj_id = attacker.obj_id;
        mate.unit.faction_id = PIRATE_FACTION;
        mate.unit.hp = 1000;
        mate.unit.max_hp = 1000;
        mate.unit.transform.set_xyz(102.0, 100.0, 100.0);
        let mate = Entity::new(0x40a, mate);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&mate, &server.view_ctx());

        apply_hit(
            &server,
            &WorldObject::from(&mate),
            &WorldObject::from(&victim),
        );
        assert!(
            !has_aggro_on(&guard, mate.obj_id),
            "the duel pair's pet attacker must not pull the guard"
        );
    }

    /// Captures the formatted WARN lines of the current thread.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn warnings(f: impl FnOnce()) -> Vec<String> {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
#[path = "fish_movement_tests.rs"]
mod fish_movement_tests;

#[cfg(test)]
#[path = "chase_tests.rs"]
mod chase_tests;

#[cfg(test)]
#[path = "walk_tests.rs"]
mod walk_tests;
