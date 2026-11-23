use std::sync::Arc;

use crate::models::container::{slot_type, ContainerRef};
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory;
use super::super::inventory::{Inv, ItemTxn};
use super::BaseMail;

// ---------------------------------------------------------------------------------------------
// Item helpers (attachments are item ids)
// ---------------------------------------------------------------------------------------------

/// Runs `f` on the item object `id` wherever it is (a persistent container or the loose items).
pub fn mutate_item(server: &GameServer, id: u64, f: impl FnOnce(&mut Item)) -> bool {
    if let Some(c) = server.items.holding_container(id) {
        let mut g = c.lock().expect("container poisoned");
        if let Some(i) = g.get_item_by_id_mut(id) {
            f(i);
            return true;
        }
    }
    if let Some(mut i) = server.items.take_loose(id) {
        f(&mut i);
        server.items.add_loose(i);
        return true;
    }
    false
}

/// `BaseMail.RenumberSlots()`: every attachment gets `SlotType.Mail` and its index.
pub fn renumber_slots(server: &GameServer, mail: &BaseMail) {
    for (i, id) in mail.body.attachments.iter().enumerate() {
        mutate_item(server, *id, |it| {
            it.slot_type = slot_type::MAIL;
            it.slot = i as i32;
            it.dirty = true;
        });
    }
}

/// `character.Inventory.MailAttachments`.
pub fn mail_container(player: &PlayerRef) -> Option<ContainerRef> {
    Inv::of(player).get(slot_type::MAIL)
}

/// `container.AddOrMoveExistingItem(task, item, preferredSlot)` for an item that may be in any
/// container (its `_holdingContainer`) or loose. Returns the C# result.
pub fn add_or_move_existing_item(
    txn: &mut ItemTxn<'_>,
    target: &ContainerRef,
    task: u8,
    item_id: u64,
    preferred_slot: i32,
) -> bool {
    let server = txn.server;
    let data = &server.game_data.items;
    match server.items.holding_container(item_id) {
        Some(src) if Arc::ptr_eq(&src, target) => target
            .lock()
            .expect("container poisoned")
            .relocate_item(task, item_id, preferred_slot, data, &mut txn.env.fx),
        Some(src) => {
            let (mut t, s) = inventory::lock_pair(target, &src);
            let Some(mut s) = s else { return false };
            t.move_item_from(&mut s, task, item_id, preferred_slot, data, &mut txn.env.fx)
        }
        None => {
            let Some(item) = server.items.take_loose(item_id) else {
                // C#: `item` is null -> NullReferenceException.
                tracing::error!("AddOrMoveExistingItem: item {item_id} no longer exists");
                return false;
            };
            let res = target.lock().expect("container poisoned").add_item(
                task,
                item,
                preferred_slot,
                data,
                &mut txn.env.fx,
            );
            match res {
                Ok(r) => r,
                Err(item) => {
                    server.items.add_loose(item);
                    false
                }
            }
        }
    }
}

/// `DeleteMail(BaseMail mail, bool trashItems)`.
pub fn delete_mail_trash(txn: &mut ItemTxn<'_>, mail_id: i64, trash_items: bool) -> bool {
    let server = txn.server;
    if trash_items {
        let attachments = server
            .systems
            .economy
            .mail
            .get_mail_by_id(mail_id)
            .map(|m| m.body.attachments)
            .unwrap_or_default();
        for (i, item_id) in attachments.iter().enumerate().rev() {
            match server.items.holding_container(*item_id) {
                Some(c) => {
                    c.lock().expect("container poisoned").remove_item_task(
                        task_type::INVALID,
                        *item_id,
                        true,
                        &mut txn.env,
                    );
                }
                None => tracing::error!(
                    "Failed to remove mail attachment [{i}] from {mail_id}: item has no container"
                ),
            }
        }
    }
    server.systems.economy.mail.delete_mail(server, mail_id)
}
