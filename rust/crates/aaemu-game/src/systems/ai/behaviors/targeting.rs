use crate::world::WorldObject;

use super::super::combat::clear_aggro_of_unit;
use super::super::framework::AiCtx;

use super::chase::{find_path, geo_data_mode, reset_chase_path};
use super::help::update_aggro_help;
use super::npc;

/// `Ai.Owner.SetTarget(other)`.
pub fn set_target(c: &mut AiCtx<'_>, other: Option<&WorldObject>) {
    npc::set_target(c.server, &c.npc, other);
    c.ai.already_targeted = other.is_some();
    if other.is_none() {
        reset_chase_path(c);
    }
}

/// `Ai.Owner.CurrentTarget` (as a world object).
pub(super) fn current_target(c: &AiCtx<'_>) -> Option<WorldObject> {
    let id = c.npc.data().unit.current_target;
    if id == 0 {
        return None;
    }
    if id == c.npc.obj_id {
        return Some(c.obj.clone());
    }
    c.server.world.object(id)
}

/// `BaseCombatBehavior.UpdateTarget()`: the abuser with the most aggro becomes the target.
pub(super) fn update_target(c: &mut AiCtx<'_>) -> bool {
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
pub(super) fn sorted_abusers(c: &AiCtx<'_>) -> Vec<Option<WorldObject>> {
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
