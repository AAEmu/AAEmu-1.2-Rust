//! `Models/Game/AI/v2/Framework/Behavior` (skill picking, aggression / alert checks, aggro
//! help), `Behaviors/BaseCombatBehavior` (`MoveInRange`, `UpdateTarget`, `ShouldReturn`,
//! `RefreshSkillQueue`, ...) and every concrete behavior of `Behaviors/**` (Common, Archer,
//! BigMonster, Flytrap, WildBoar).
//!
//! Each C# behavior method is a function taking the [`AiCtx`] and the behavior's
//! [`BehaviorKind`] (its instance); the instance fields live in [`BehaviorSlot`] and are only
//! borrowed for single statements, so nested `Ai.GoToX()` calls behave like the C# ones.

use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::instances::encounters::hooks as encounter_hooks;
use crate::systems::skills::buffs;

use self::archer::{archer_attack_enter, archer_attack_tick};
use self::big_monster::{big_monster_attack_enter, big_monster_attack_tick};
use self::chase::reset_chase_path;
use self::combat::{
    almighty_attack_enter, almighty_attack_exit, almighty_attack_tick, attack_enter, attack_tick,
};
use self::command_sets::run_command_set_tick;
use self::flytrap::{flytrap_alert_enter, flytrap_attack_enter, flytrap_attack_tick};
use self::idle::{hold_position_enter, hold_position_tick, idle_enter, idle_tick};
use self::paths::{follow_path_enter, follow_path_tick, follow_unit_tick};
use self::perception::{alert_enter, alert_tick};
use self::return_state::{return_state_enter, return_state_tick};
use self::roaming::roaming_tick;
use self::support::{dead_enter, despawning_enter, posture_packet, spawning_enter, spawning_tick};
use self::talk::{talk_enter, talk_tick};
use self::wild_boar::{wild_boar_attack_enter, wild_boar_attack_tick};
use super::framework::{AiCtx, BehaviorKind};
use super::skills as ai_skills;
use super::{
    collision, geodata, gimmick, group, hooks, interaction, manager, npc, npc_events, packets,
    params, profile,
};

use BehaviorKind as B;

mod archer;
mod big_monster;
mod chase;
mod combat;
mod command_sets;
mod flytrap;
mod help;
mod idle;
mod ledge;
mod paths;
mod perception;
mod return_state;
mod roaming;
mod skill_use;
mod support;
mod talk;
mod targeting;
mod wild_boar;

pub use self::help::{update_aggro_help, update_guard_help};
pub use self::perception::{check_aggression, check_alert};
pub use self::roaming::calc_next_roaming_position;
pub(crate) use self::skill_use::is_using_skill;
pub use self::skill_use::{on_skill_ended, pick_skill_and_use_it, use_skill};
pub use self::support::npc_move_towards;
pub(crate) use self::support::{base_move_speed, delta_seconds, owner_pos};
pub use self::targeting::set_target;

#[cfg(test)]
use self::big_monster::ranged_base_attack;
#[cfg(test)]
use self::chase::{find_path, move_in_range};
#[cfg(test)]
use self::help::GUARD_HELP_SCAN_INTERVAL_MS;
#[cfg(test)]
use self::ledge::{ledge_wait_over, ChaseStep};
#[cfg(test)]
use self::roaming::{choose_roaming_position, wall_ahead};
#[cfg(test)]
use self::skill_use::health_and_time_ok;
#[cfg(test)]
use crate::server::GameServer;
#[cfg(test)]
use crate::systems::skills::controllers::calculate_distance;
#[cfg(test)]
use crate::systems::skills::types::skill_result;
#[cfg(test)]
use crate::world::{now_ms, WorldObject};
#[cfg(test)]
use std::sync::Arc;

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
    if encounter_hooks::for_zone(c.obj.transform().zone_id).ai_tick(c, k, delta_ms) {
        return;
    }
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

#[cfg(test)]
mod chase_tests;
#[cfg(test)]
mod fish_movement_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod walk_tests;
