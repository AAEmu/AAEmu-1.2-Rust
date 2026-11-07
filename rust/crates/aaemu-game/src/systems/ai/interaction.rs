//! `ai_command_sets.can_interact` and the two NPC interaction packets (WP-5b).
//!
//! The 1.2 data marks 23 of the 325 command sets `can_interact = 't'` and 302 `'f'`, but the C#
//! loaded the column and never read it. The rule here is INFERRED (see `PORTING.md`): while a
//! command set with `can_interact = 'f'` runs, the NPC's client interaction marker is off, the
//! interaction window a player still has open on it is closed, and `CSStartInteraction` is
//! refused. `SCNpcInteractionStatusChanged`/`SCNpcInteractionEndedByZone` layouts are evidenced
//! from the 1.2 readers `FUN_397b4b00` / `FUN_397b4aa0`; sending them for this state is not.

use crate::systems::npcs::npc::packets::{
    SCNpcInteractionEndedByZonePacket, SCNpcInteractionStatusChangedPacket,
};
use crate::world::WorldInstance;

use super::framework::AiCtx;

/// Applies the `can_interact` of the command set about to run: a locked (`'f'`) set makes the
/// NPC non-interactable, a later command of another (`'t'`) set unlocks it again. Unknown set
/// ids keep the NPC interactable, exactly like an NPC without a set.
pub(crate) fn apply_command_set(c: &mut AiCtx<'_>, cmd_set_id: u32) {
    if !c.server.config.interaction.command_set_lock {
        return; // INFERRED rule disabled (Interaction.CommandSetLock, default off)
    }
    let can_interact = c
        .server
        .game_data
        .systems
        .ai
        .ai
        .command_sets
        .get(&cmd_set_id)
        .is_none_or(|s| s.can_interact);
    match (can_interact, c.ai.locked_command_set) {
        (true, None) => {}
        (false, Some(id)) if id == cmd_set_id => {}
        (false, _) => {
            c.ai.locked_command_set = Some(cmd_set_id);
            set_interactable(c, false);
        }
        (true, Some(_)) => release(c),
    }
}

/// Ends the lock once the command set is over (idle / follow end / death / despawn) and marks
/// the NPC interactable again.
pub(crate) fn release(c: &mut AiCtx<'_>) {
    if c.ai.locked_command_set.take().is_some() {
        set_interactable(c, true);
    }
}

/// `SCNpcInteractionEndedByZone` for every player whose interaction window is open on the NPC
/// (the server tracks it as `Unit.CurrentInteractionObject`); nothing when none is open.
pub(crate) fn end_open_interactions(c: &AiCtx<'_>, npc_obj_id: u32) {
    let Some(world) = c.server.world.world(c.obj.transform().instance_id) else {
        return;
    };
    end_open_interactions_in(&world, npc_obj_id);
}

/// [`end_open_interactions`] for the delete path, which has no `AiCtx` (the NPC is still in the
/// world when `NpcManager::delete` calls the AI hook).
pub(crate) fn end_open_interactions_for(server: &crate::server::GameServer, npc_obj_id: u32) {
    let Some(obj) = server.world.object(npc_obj_id) else {
        return;
    };
    let Some(world) = server.world.world_of(&obj) else {
        return;
    };
    end_open_interactions_in(&world, npc_obj_id);
}

fn set_interactable(c: &mut AiCtx<'_>, interactable: bool) {
    let changed = {
        let mut n = c.npc.data();
        let changed = n.interactable != interactable;
        n.interactable = interactable;
        changed
    };
    if !changed {
        return;
    }
    let npc_obj_id = c.npc.obj_id;
    let Some(world) = c.server.world.world(c.obj.transform().instance_id) else {
        return;
    };
    crate::world::broadcast(
        &world,
        &c.obj,
        &SCNpcInteractionStatusChangedPacket {
            npc_obj_id,
            interactable,
        },
        false,
    );
    if !interactable {
        end_open_interactions_in(&world, npc_obj_id);
    }
}

/// Closes the window of every player whose `CurrentInteractionObject` is this NPC. The client's
/// `FUN_391d9000` closes the window but does not tell the server, so the tracked id is cleared
/// here as well.
fn end_open_interactions_in(world: &WorldInstance, npc_obj_id: u32) {
    for p in world.all_characters() {
        let open = {
            let mut ch = p.data();
            if ch.unit.current_interaction_object == npc_obj_id {
                ch.unit.current_interaction_object = 0;
                true
            } else {
                false
            }
        };
        if open {
            p.send(SCNpcInteractionEndedByZonePacket { npc_obj_id });
        }
    }
}
