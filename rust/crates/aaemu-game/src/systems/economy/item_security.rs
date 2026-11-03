//! Item security (`CSItemSecurePacket`, `CSItemUnsecurePacket`,
//! `CSEquipmentsSecurePacket`, `CSEquipmentsUnsecurePacket`).
//!
//! The 1.2 client sends `CSItemSecure`/`CSItemUnsecure` for one bag item and
//! `CSEquipmentsSecure`/`CSEquipmentsUnsecure` for the whole equipment set
//! (`FUN_394ea660`/`FUN_394ea730`, `FUN_394e5dd0`/`FUN_394e5ea0`). Locking sets
//! `ItemFlag.Secure`; unlocking sets `unsecure_time = now + content_configs[43] * 60` and keeps
//! the flag, so the item stays locked until the delay has elapsed. `X2Item.GetSecurityUnlockDelayTime`
//! reads `content_configs` id 43 (`FUN_395720e0(0x2b)`); the 1.2 compact DB value is 4320
//! minutes (72 hours).
//!
//! Only items whose `item_categories.secure` is set and that are not listed in
//! `item_secure_exceptions` can be locked (`FUN_394df4a0`, `IsSecurityLockable`).
//!
//! The task/flag combination the client expects for lock / unlock / expiry is inferred from the
//! 1.2 client's `ItemUpdateSecurity` userdata fields (`isUnsecureExcess`, `isUnSecureSet`,
//! `isUnpack`, `unSecureDateTime`) and the task-type names (`ITEM_LOCK`, `ITEM_UNLOCK`,
//! `ITEM_UNLOCK_EXCESS`); no 1.2 capture of the server replies is available yet
//! (capture-pending, case item-lock-flow).

use chrono::Utc;

use crate::models::container::slot_type;
use crate::models::item::{Item, ITEM_FLAG_SECURE, ITEM_FLAG_UNPACKED};
use crate::models::item_task::{task_type, ItemTask};
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::error_message;
use super::inventory::{self, Inv};
use super::item_packets::SCItemTaskSuccessPacket;

/// `ItemFlag.Secure` as the client's `Item.HasFlag`.
pub fn is_locked(item: &Item) -> bool {
    item.has_flag(ITEM_FLAG_SECURE)
}

/// `item_categories.secure` + `item_secure_exceptions` (`FUN_394df4a0`).
pub fn is_lockable(server: &GameServer, item: &Item) -> bool {
    server
        .game_data
        .items
        .template(item.template_id)
        .is_some_and(|t| server.game_data.systems.economy.items.is_secure_lockable(t))
}

/// Refuses an operation on a locked item with the client's `item_secure_condition` error (664).
pub fn refuse_if_locked(player: &PlayerRef, item: &Item) -> bool {
    if !is_locked(item) {
        return false;
    }
    inventory::send_error_message(player, error_message::ITEM_SECURE_CONDITION, 0, true);
    true
}

/// Refuses a container move/split/merge/swap that touches a locked item with 664. The 1.2
/// client's drag validation returns `item_secure_condition` for either side (`FUN_390455d0`).
pub fn refuse_move_if_locked(
    server: &GameServer,
    player: &PlayerRef,
    from_item_id: u64,
    to_item_id: u64,
) -> bool {
    let locked = [from_item_id, to_item_id]
        .into_iter()
        .filter(|id| *id != 0)
        .any(|id| server.items.item_by_id(id).is_some_and(|i| is_locked(&i)));
    if locked {
        inventory::send_error_message(player, error_message::ITEM_SECURE_CONDITION, 0, true);
    }
    locked
}

fn task(item: &Item, is_unsecure_excess: bool, is_unsecure_set: bool) -> ItemTask {
    ItemTask::update_security(
        item,
        item.flags,
        is_unsecure_excess,
        is_unsecure_set,
        item.has_flag(ITEM_FLAG_UNPACKED),
    )
}

fn send_tasks(player: &PlayerRef, action: u8, tasks: Vec<ItemTask>) {
    if tasks.is_empty() {
        return;
    }
    player.send(SCItemTaskSuccessPacket {
        action,
        tasks,
        force_remove: Vec::new(),
    });
}

/// `content_configs[43]` is minutes (`X2Item.GetSecurityUnlockDelayTime`; the dialogs divide it
/// by 60 to print hours), so the deadline is `value * 60` seconds.
fn unlock_delay_secs(server: &GameServer) -> i64 {
    server
        .game_data
        .systems
        .economy
        .items
        .security_unlock_delay_minutes
        .max(0)
        * 60
}

/// `CSItemSecurePacket`: lock one bag item.
pub fn secure_item(
    server: &GameServer,
    player: &PlayerRef,
    st: u8,
    slot: u8,
    item_id: u64,
) -> bool {
    if st != slot_type::INVENTORY {
        inventory::send_error_message(player, error_message::ITEM_SECURE_CONDITION, 0, true);
        return false;
    }
    let inv = Inv::of(player);
    let bag = inv.bag();
    let task = {
        let mut c = bag.lock().expect("container poisoned");
        let owned = c.owner_id == player.character_id;
        match c.get_item_by_id_mut(item_id) {
            Some(item)
                if owned
                    && item.slot_type == st
                    && item.slot == i32::from(slot)
                    && is_lockable(server, item)
                    && !(is_locked(item) && item.unsecure_time == 0) =>
            {
                // A pending unlock is cancelled by locking again (client state 3 -> 2).
                item.flags |= ITEM_FLAG_SECURE;
                item.unsecure_time = 0;
                item.dirty = true;
                Some(task(item, false, false))
            }
            Some(item) if item.slot_type != st || item.slot != i32::from(slot) => {
                tracing::warn!(
                    "ItemSecure: slot mismatch for item {item_id} (requested {st}-{slot})"
                );
                None
            }
            _ => None,
        }
    };
    let Some(task) = task else {
        inventory::send_error_message(player, error_message::ITEM_SECURE_CONDITION, 0, true);
        return false;
    };
    send_tasks(player, task_type::ITEM_LOCK, vec![task]);
    true
}

/// `CSItemUnsecurePacket`: start the unlock delay of one bag item.
pub fn unsecure_item(
    server: &GameServer,
    player: &PlayerRef,
    st: u8,
    slot: u8,
    item_id: u64,
) -> bool {
    if st != slot_type::INVENTORY {
        inventory::send_error_message(player, error_message::ITEM_SECURE_CONDITION, 0, true);
        return false;
    }
    let inv = Inv::of(player);
    let delay = unlock_delay_secs(server);
    let bag = inv.bag();
    let task = {
        let mut c = bag.lock().expect("container poisoned");
        let owned = c.owner_id == player.character_id;
        match c.get_item_by_id_mut(item_id) {
            Some(item)
                if owned
                    && item.slot_type == st
                    && item.slot == i32::from(slot)
                    && is_locked(item)
                    && item.unsecure_time == 0 =>
            {
                // The item stays secured; `UpdateItemTimers` clears the flag when the delay
                // elapses.
                item.unsecure_time = Utc::now().timestamp() + delay;
                item.dirty = true;
                Some(task(item, false, true))
            }
            Some(item) if item.slot_type != st || item.slot != i32::from(slot) => {
                tracing::warn!(
                    "ItemUnsecure: slot mismatch for item {item_id} (requested {st}-{slot})"
                );
                None
            }
            _ => None,
        }
    };
    let Some(task) = task else {
        inventory::send_error_message(player, error_message::ITEM_SECURE_CONDITION, 0, true);
        return false;
    };
    send_tasks(player, task_type::ITEM_UNLOCK, vec![task]);
    true
}

/// `CSEquipmentsSecurePacket`: lock every equipped item. The 1.2 client's equipment lock checks
/// every equipment slot including slot 26 (backpack/trade pack; `FUN_394e5dd0`, slot table
/// `DAT_399e5db0` plus the special `0x1a` entry), so the back slot is locked with the set.
pub fn secure_equipments(_server: &GameServer, player: &PlayerRef) -> bool {
    let inv = Inv::of(player);
    let equipment = inv.equipment();
    let mut tasks = Vec::new();
    {
        let mut c = equipment.lock().expect("container poisoned");
        if c.owner_id != player.character_id {
            return false;
        }
        for item in c.items.iter_mut().filter(|i| i.id != 0) {
            if is_locked(item) && item.unsecure_time == 0 {
                continue;
            }
            item.flags |= ITEM_FLAG_SECURE;
            item.unsecure_time = 0;
            item.dirty = true;
            tasks.push(task(item, false, false));
        }
    }
    if tasks.is_empty() {
        inventory::send_error_message(player, error_message::ALL_EQUIPMENTS_ARE_SECURED, 0, true);
        return false;
    }
    send_tasks(player, task_type::ITEM_LOCK, tasks);
    true
}

/// `CSEquipmentsUnsecurePacket`: start the unlock delay of every locked equipped item.
pub fn unsecure_equipments(server: &GameServer, player: &PlayerRef) -> bool {
    let inv = Inv::of(player);
    let delay = unlock_delay_secs(server);
    let now = Utc::now().timestamp();
    let equipment = inv.equipment();
    let mut tasks = Vec::new();
    {
        let mut c = equipment.lock().expect("container poisoned");
        if c.owner_id != player.character_id {
            return false;
        }
        for item in c.items.iter_mut().filter(|i| i.id != 0) {
            if !is_locked(item) || item.unsecure_time != 0 {
                continue;
            }
            item.unsecure_time = now + delay;
            item.dirty = true;
            tasks.push(task(item, false, true));
        }
    }
    if tasks.is_empty() {
        inventory::send_error_message(player, error_message::ALL_EQUIPMENTS_ARE_UNSECURED, 0, true);
        return false;
    }
    send_tasks(player, task_type::ITEM_UNLOCK, tasks);
    true
}

/// Clears the secure flag of an item whose unlock delay has elapsed, returning the update task.
/// The caller holds the item's container lock and records it in the item effects.
pub fn finalize_expired(item: &mut Item, now: i64) -> Option<ItemTask> {
    if !is_locked(item) || item.unsecure_time == 0 || item.unsecure_time > now {
        return None;
    }
    item.flags &= !ITEM_FLAG_SECURE;
    item.unsecure_time = 0;
    item.dirty = true;
    Some(task(item, true, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_keeps_a_pending_unlock_locked_until_the_deadline() {
        let mut item = Item::new(1);
        item.id = 7;
        item.flags = ITEM_FLAG_SECURE;
        item.unsecure_time = 100;
        assert!(finalize_expired(&mut item, 99).is_none());
        assert!(is_locked(&item));
        let task = finalize_expired(&mut item, 100).expect("expired");
        assert!(!is_locked(&item));
        assert_eq!(item.unsecure_time, 0);
        assert!(matches!(
            task,
            ItemTask::ItemUpdateSecurity {
                is_unsecure_excess: true,
                is_unsecure_set: false,
                ..
            }
        ));
    }
}
