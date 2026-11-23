//! The economy subsystem's implementations of the other subsystems' hooks (installed by
//! [`install`] from `System::start`; tests call it too):
//!
//! | hook | implementation |
//! |---|---|
//! | skills `ItemSkillHooks` | [`EconomyForSkills`]: gear stats, holdables, item counts / consumes, the item `SpecialEffect`s (`item_effects::*`), the item effects (`GainLootPackItem`, `ImprintUcc`, `CleanupUcc`, `PutDownBackpack`, `RecoverExp`, `CraftEffect` collect / craft), the gear buffs of `UpdateGearBonuses` |
//! | npcs `EconomyBridge` | [`EconomyForNpcs`]: NPC loot (`LootManager.GenerateLoot`), doodad func items, coffers |
//! | quests `QuestHooks` (item / money / mail part) | [`EconomyForQuests`] (`server.systems.quests.economy_hooks`) |
//! | housing `HousingEconomyHooks` | [`EconomyForHousing`] |
//! | vehicles `VehicleItemHooks` | [`EconomyForVehicles`] (`EndCraft`) |
//! | economy `MailHousingHooks`, `CraftHooks`, `ItemEffectHooks` | [`EconomyComposite`]: direct calls into housing / npcs / skills / quests |
//! | services `WebApiEconomyHooks`, `CHARACTER_ITEM_OPS`, `SheetMusicInventory`, `BUYBACK_WIPE` | [`EconomyForServices`] |

use std::sync::Arc;

use crate::models::container::{slot_type, ContainerRef, ItemContainer};
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemTask};
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::account_attributes;
use super::auction;
use super::character_slots;
use super::craft;
use super::craft_learning;
use super::fishing;
use super::fx;
use super::inventory::{Inv, ItemTxn};
use super::item_packets::SCItemTaskSuccessPacket;
use super::loot;
use super::looting;
use super::mail;
use super::mail_kinds;
use super::procs;
use super::renew;
use super::repair;
use super::shop_limits;

mod composite;
mod housing;
mod npcs;
mod quests;
mod services;
mod skills;

pub use self::composite::EconomyComposite;
pub use self::housing::EconomyForHousing;
pub use self::npcs::EconomyForNpcs;
pub use self::quests::EconomyForQuests;
pub use self::services::{EconomyForServices, EconomySheetMusic};
pub use self::skills::EconomyForSkills;

#[cfg(test)]
use crate::systems::skills::ext::ItemSkillHooks;

/// `Item.Dye` category.
const CATEGORY_DYE: i32 = 33;

/// Locks a container. In a function that owns an [`ItemTxn`], never leave the guard as a
/// temporary of the tail expression: it would be dropped after the transaction, whose drop runs
/// the quest hooks that lock the same container again (self-deadlock). Bind the result to a local
/// first (`let result = lock(..).consume_item(.., &mut txn.env); result`).
fn lock(c: &ContainerRef) -> std::sync::MutexGuard<'_, ItemContainer> {
    c.lock().expect("container poisoned")
}

/// Puts a taken/refunded item back where the server actually placed it. An equipment source
/// only returns to its own slot while that slot is free; otherwise it goes to the owner's bag
/// (never into another equipment slot, which would equip it in the wrong slot). A full
/// container leaves the item loose. The client task reports exactly the container and slot used.
fn put_restored_item(
    server: &GameServer,
    txn: &mut ItemTxn<'_>,
    container: &ContainerRef,
    preferred_slot: i32,
    mut item: Item,
    owner: u32,
) {
    item.dirty = true;
    let mut guard = lock(container);
    let equipment = guard.is_equipment();
    let slot = if equipment {
        // Equipment slots are not interchangeable: a different free slot is a different gear
        // slot, and `EquipEnter` would announce the item at the wrong one.
        if guard.get_item_by_slot(preferred_slot).is_none() {
            preferred_slot
        } else {
            -1
        }
    } else {
        guard.get_unused_slot(preferred_slot)
    };
    if slot >= 0 {
        item.slot = slot;
        item.slot_type = guard.slot_type;
        guard.items.insert(0, item.clone());
        guard.update_free_slot_count();
        guard.notify_enter(&item, &mut txn.env.fx);
        drop(guard);
        txn.env
            .fx
            .tasks(owner, task_type::SKILL_REAGENTS, vec![ItemTask::add(&item)]);
        return;
    }
    drop(guard);
    if equipment {
        if let Some(player) = server.world.character_by_id(owner) {
            let bag = Inv::of(&player).bag();
            let mut guard = lock(&bag);
            let slot = guard.get_unused_slot(-1);
            if slot >= 0 {
                item.slot = slot;
                item.slot_type = guard.slot_type;
                guard.items.insert(0, item.clone());
                guard.update_free_slot_count();
                drop(guard);
                txn.env
                    .fx
                    .tasks(owner, task_type::SKILL_REAGENTS, vec![ItemTask::add(&item)]);
                return;
            }
        }
    }
    // Not in any container: the server keeps it loose, so no slot-add is reported.
    tracing::error!(
        item_id = item.id,
        template_id = item.template_id,
        "cast source could not be placed back into a container"
    );
    server.items.add_loose(item);
}

fn task_packet(action: u8, tasks: Vec<ItemTask>) -> SCItemTaskSuccessPacket {
    SCItemTaskSuccessPacket {
        action,
        tasks,
        force_remove: Vec::new(),
    }
}

/// `Inventory.SystemContainer.AddOrMoveExistingItem(task, item)`.
fn move_to_system_container(
    server: &GameServer,
    player: &PlayerRef,
    task: u8,
    item_id: u64,
) -> bool {
    let mut txn = ItemTxn::begin(server);
    let Some(target) = Inv::of(player).get(slot_type::SYSTEM) else {
        tracing::error!("character {} has no system container", player.character_id);
        return false;
    };
    super::mail::add_or_move_existing_item(&mut txn, &target, task, item_id, -1)
}

// ---------------------------------------------------------------------------------------------
// vehicles: VehicleItemHooks
// ---------------------------------------------------------------------------------------------

/// `character.Craft.EndCraft()` for the shipyard branches of `CraftEffect`.
pub struct EconomyForVehicles;

impl crate::systems::vehicles::hooks::VehicleItemHooks for EconomyForVehicles {
    fn end_craft(&self, server: &GameServer, player: &PlayerRef) {
        super::craft::end_craft(server, player);
    }
}

// ---------------------------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------------------------

/// Installs the economy's implementations of the other subsystems' hooks and the composite
/// hooks of its own slots.
pub fn install(server: &GameServer) {
    let s = &server.systems;
    s.economy.loot.install_release_hook(&server.ids.object);
    s.skills.ext.items.install(Arc::new(EconomyForSkills));
    s.npcs.hooks.economy.install(Arc::new(EconomyForNpcs));
    s.quests.economy_hooks.install(Arc::new(EconomyForQuests));
    s.housing.hooks.install(Arc::new(EconomyForHousing));
    s.vehicles.hooks.items.install(Arc::new(EconomyForVehicles));
    let composite = Arc::new(EconomyComposite);
    s.economy.mail.hooks.housing.install(composite.clone());
    s.economy.craft.hooks.install(composite.clone());
    s.economy.item_effects.hooks.install(composite);
    s.services
        .webapi_economy_hooks
        .install(Arc::new(EconomyForServices));
    crate::systems::services::item_ops::CHARACTER_ITEM_OPS.install(Arc::new(EconomyForServices));
    let wipe: Arc<crate::systems::services::shutdown::BuyBackWipe> =
        Arc::new(|server: &GameServer| {
            // character.BuyBackItems.Wipe() for every online character
            for p in server.world.all_characters() {
                server
                    .systems
                    .economy
                    .store
                    .wipe_buyback(server, p.character_id);
            }
        });
    crate::systems::services::shutdown::BUYBACK_WIPE.install(wipe);
}

#[cfg(test)]
mod dye_tests;

#[cfg(test)]
mod cargo_flow_tests;

#[cfg(test)]
mod pack_expiry_tests;
