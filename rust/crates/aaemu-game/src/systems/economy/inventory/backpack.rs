use crate::data::items::TemplateKind;
use crate::data::ItemData;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::{split_or_move_item, Inv, ItemTxn};

/// `ItemManager.IsAutoEquipTradePack`.
pub fn is_auto_equip_trade_pack(data: &ItemData, template_id: u32) -> bool {
    data.template(template_id)
        .is_some_and(|t| matches!(t.kind, TemplateKind::Backpack { .. }) && t.bind_type & 3 != 3)
}

/// `Inventory.TryAddNewItem(taskType, itemId, itemCount, gradeToAdd, crafterId)`.
pub fn try_add_new_item(
    txn: &mut ItemTxn<'_>,
    inv: &Inv,
    task: u8,
    template_id: u32,
    count: i32,
    grade: i32,
    crafter_id: u32,
) -> bool {
    if is_auto_equip_trade_pack(txn.data(), template_id) {
        return try_equip_new_backpack(txn, inv, task, template_id, count, grade, crafter_id);
    }
    inv.bag()
        .lock()
        .expect("container poisoned")
        .acquire_default_item(task, template_id, count, grade, crafter_id, &mut txn.env)
}

/// `Inventory.TryEquipNewBackPack`.
pub fn try_equip_new_backpack(
    txn: &mut ItemTxn<'_>,
    inv: &Inv,
    task: u8,
    template_id: u32,
    count: i32,
    grade: i32,
    crafter_id: u32,
) -> bool {
    // Validate before taking off an existing glider: a backpack cannot be stacked or
    // acquired into multiple equipment slots.
    if count != 1
        || !txn
            .data()
            .template(template_id)
            .is_some_and(|t| matches!(t.kind, TemplateKind::Backpack { .. }) && t.max_count > 0)
    {
        return false;
    }
    // Remove player backpack
    if take_off_backpack(txn, inv, task, true) {
        // Put tradepack in their backpack slot
        return inv
            .equipment()
            .lock()
            .expect("container poisoned")
            .acquire_default_item(task, template_id, count, grade, crafter_id, &mut txn.env);
    }
    false
}

fn backpack_type_of(data: &ItemData, item: &Item) -> Option<u32> {
    match data.template(item.template_id).map(|t| &t.kind) {
        Some(TemplateKind::Backpack { backpack_type }) => Some(*backpack_type),
        _ => None,
    }
}

/// `Inventory.CanReplaceGliderInBackpackSlot`.
pub fn can_replace_glider_in_backpack_slot(data: &ItemData, inv: &Inv) -> bool {
    let Some(backpack) = inv.equipped(equip_slot::BACKPACK) else {
        return true; // Nothing equipped, so we're good
    };
    // Check if a glider is equipped, and if we have at least 1 free space
    backpack_type_of(data, &backpack) == Some(crate::data::items::backpack_type::GLIDER)
        && inv.free_slot_count(slot_type::INVENTORY) > 0
}

/// `Inventory.TakeoffBackpack(taskType, glidersOnly)`. `previous_backpack` receives
/// `PreviousBackPackItemId` when a glider was taken off for a trade pack.
pub fn take_off_backpack(txn: &mut ItemTxn<'_>, inv: &Inv, task: u8, gliders_only: bool) -> bool {
    let Some(backpack) = inv.equipped(equip_slot::BACKPACK) else {
        return true;
    };
    // Check glider if needed
    if gliders_only {
        if let Some(bt) = backpack_type_of(txn.data(), &backpack) {
            if bt != crate::data::items::backpack_type::GLIDER {
                return false;
            }
        }
    }
    // Move to first available slot
    let bag = inv.bag();
    let free_slot = {
        let g = bag.lock().expect("container poisoned");
        if g.free_slot_count() <= 0 {
            return false;
        }
        g.get_unused_slot(-1)
    };
    if !split_or_move_item(
        txn,
        inv,
        task,
        backpack.id,
        backpack.slot_type,
        backpack.slot as u8,
        0,
        slot_type::INVENTORY,
        free_slot as u8,
        0,
    ) {
        return false;
    }
    if gliders_only {
        txn.server.systems.economy.set_previous_backpack(
            inv.character_id,
            backpack.id,
            backpack.template_id,
        );
    }
    true
}

/// Puts back the glider that an auto-equipped backpack (trade pack) replaced, once the pack has
/// left the back slot by any route (deviation from the C#, which only did this in
/// `PutDownBackpackEffect`; called centrally from [`super::fx::flush`] on the back slot's
/// `EquipLeave`, and on resurrection). The record is kept while the character is dead; in every
/// other case it is consumed: a glider that is not provably the one that was taken off, or that
/// cannot be equipped right now, is left in the bag. Returns whether the glider was equipped.
/// Must not be called with a character or container lock held or inside an [`ItemTxn`].
pub fn reequip_previous_glider(server: &GameServer, player: &PlayerRef) -> bool {
    let economy = &server.systems.economy;
    let character_id = player.character_id;
    let Some((item_id, template_id)) = economy.previous_backpack(character_id) else {
        return false;
    };
    // Dead: keep the record, `resurrect_character` retries.
    if player.data().unit.hp <= 0 {
        return false;
    }
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let equipment = inv.equipment();
    // Something is worn (e.g. a pack that is not dropped on death): keep the record for when it
    // leaves; `glider_reequip_candidates` drops it if the player equipped something else.
    if inv.equipped(equip_slot::BACKPACK).is_some() {
        return false;
    }
    // Only this character's own bag counts (not bank, mail, trade, system container, others).
    let glider = inv
        .bag()
        .lock()
        .expect("container poisoned")
        .get_item_by_id(item_id)
        .cloned();
    let valid = glider.as_ref().is_some_and(|g| {
        g.template_id == template_id
            && g.slot_type == slot_type::INVENTORY
            && backpack_type_of(txn.data(), g) == Some(crate::data::items::backpack_type::GLIDER)
    }) && {
        let eq = equipment.lock().expect("container poisoned");
        // The back slot is empty and the equipment container accepts the glider (level / class /
        // slot rules, as for the client's equip request).
        eq.get_item_by_slot(equip_slot::BACKPACK).is_none()
            && glider.as_ref().is_some_and(|g| {
                eq.place(g, equip_slot::BACKPACK, txn.data())
                    .is_some_and(|(slot, _)| slot == equip_slot::BACKPACK)
            })
    };
    // The record is consumed whatever the outcome.
    economy.clear_previous_backpack(character_id);
    let (true, Some(glider)) = (valid, glider) else {
        return false;
    };
    split_or_move_item(
        &mut txn,
        &inv,
        task_type::SWAP_ITEMS,
        glider.id,
        slot_type::INVENTORY,
        glider.slot as u8,
        0,
        slot_type::EQUIPMENT,
        equip_slot::BACKPACK as u8,
        0,
    )
}
