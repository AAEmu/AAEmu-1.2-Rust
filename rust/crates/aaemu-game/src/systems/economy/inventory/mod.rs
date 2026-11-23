//! Port of `Models/Game/Char/Inventory` (item moves, consumes, expands, coffers) and the money /
//! point / labor members of `Character` (`ChangeMoney`, `AddMoney`, `SubtractMoney`,
//! `ChangeGamePoints`, `ChangeLabor`, `SendErrorMessage`, `ItemUse`).
//!
//! Every item operation runs inside an [`ItemTxn`]: it takes the economy's operation lock (so
//! the multi-step C# operations cannot interleave), collects the effects in its [`ItemEnv`]
//! and applies item-manager persistence effects before releasing that lock. It flushes
//! notifications and callbacks through [`super::fx::flush`] afterwards. Lock order inside
//! a transaction: character data (one at a time, short) and
//! container locks (leaf; two at most through [`lock_pair`]). Never start a transaction while
//! holding a character or container lock.

use std::sync::{Arc, MutexGuard};

use chrono::Utc;

use crate::data::items::{TemplateKind, EQUIPMENT_ITEM_SLOT_TYPE_BACKPACK};
use crate::data::ItemData;
use crate::models::container::{
    equip_slot, slot_type, ContainerRef, ItemContainer, ItemEnv, ItemRules,
};
use crate::models::item::{Item, ItemClass};
use crate::models::item_task::{task_type, ItemFx, ItemFxList, ItemTask};
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

#[cfg(doc)]
use super::fx;
use super::item_packets;
use super::item_packets::SCInvenExpandedPacket;

mod backpack;
mod character;

pub use self::backpack::{
    can_replace_glider_in_backpack_slot, is_auto_equip_trade_pack, reequip_previous_glider,
    take_off_backpack, try_add_new_item, try_equip_new_backpack,
};
pub(crate) use self::character::item_use_zone_allowed;
pub use self::character::{
    actability_exp_multiplier, actability_labor_cost_multiplier, actability_time_multiplier,
    add_money, change_game_points, change_labor, change_money, change_money_at,
    equip_actability_refused, game_point_kind, item_use, meets_item_actability,
    refuse_item_actability, send_error_message, send_error_message_to, subtract_money,
};

/// `EquipmentItemSlotType` values the two-handed checks use.
mod gear_slot_type {
    pub const TWO_HANDED: u32 = 16;
}

/// `GamePointKind`.
impl ItemRules for GameServer {
    /// `SummonSlave.CanDestroy()` through the summon hooks; every other class can be destroyed.
    fn can_destroy(&self, item: &Item) -> bool {
        if item.class != ItemClass::SummonSlave {
            return true;
        }
        match self.systems.economy.hooks.summons.get() {
            Some(h) => h.can_destroy_summon_slave(self, item),
            None => true,
        }
    }
}

/// An item operation in progress (see the module docs).
pub struct ItemTxn<'a> {
    pub server: &'a GameServer,
    guard: Option<super::OpsGuard<'a>>,
    pub env: ItemEnv<'a>,
}

impl<'a> ItemTxn<'a> {
    /// Starts an operation: takes the economy's operation lock.
    pub fn begin(server: &'a GameServer) -> Self {
        let guard = server.systems.economy.ops_lock();
        Self {
            server,
            guard: Some(guard),
            env: server_env(server),
        }
    }

    /// An operation without the operation lock, for callers that already hold a character
    /// lock (the quest "locked hooks"): starting a normal transaction there could deadlock with
    /// an operation that holds the lock and waits for that character. Hand the effects to
    /// [`ItemTxn::into_fx`] and flush them once the character lock is released.
    /// This path deliberately bypasses both the ops mutex and the outer publication scope.
    /// It is not protected against concurrent world publication; callers must keep its quest
    /// mutations confined to their already-locked character and must never acquire the outer
    /// scope while holding that character lock.
    pub fn unlocked(server: &'a GameServer) -> Self {
        Self {
            server,
            guard: None,
            env: server_env(server),
        }
    }

    /// Ends the operation after applying persistence effects. Notifications and callbacks are
    /// returned for [`super::fx::flush`] once every character and container lock is released.
    pub fn into_fx(mut self) -> ItemFxList {
        let mut fx = self.env.take_fx();
        super::fx::flush_persistence(self.server, &mut fx);
        fx
    }

    pub fn data(&self) -> &'a ItemData {
        &self.server.game_data.items
    }

    /// Queue for the character packets sent during this operation.
    pub fn out(&mut self) -> Out<'_> {
        Out::Queue(self.server, &mut self.env.fx)
    }

    /// Ends the operation now (same as dropping it).
    pub fn commit(self) {}
}

impl Drop for ItemTxn<'_> {
    fn drop(&mut self) {
        let mut fx = self.env.take_fx();
        super::fx::flush_persistence(self.server, &mut fx);
        // An unlocked transaction (quest locked hooks) may run under a character lock: only a
        // transaction that held the operation lock is a safe point for the queued achievements.
        let locked = self.guard.take().is_some();
        super::fx::flush_with(self.server, fx, locked);
        if locked {
            super::flush_deferred_glider_reequips(self.server);
            crate::systems::flush_achievements(self.server);
        }
    }
}

/// An [`ItemEnv`] for `server` (without the operation lock; tests and loaders).
pub fn server_env(server: &GameServer) -> ItemEnv<'_> {
    ItemEnv {
        data: &server.game_data.items,
        items: &server.items,
        item_ids: &server.ids.item,
        world_id: server.config.id,
        default_world_template_id: default_world_template_id(server),
        now: Utc::now().timestamp(),
        rules: server,
        fx: ItemFxList::default(),
    }
}

/// `WorldManager.DefaultWorldTemplateId` (the first loaded world, `main_world` = 0).
pub fn default_world_template_id(server: &GameServer) -> u8 {
    server
        .game_data
        .worlds
        .get_world_template_by_name("main_world")
        .map_or(0, |w| w.id as u8)
}

/// Locks two containers (once when they are the same), in address order.
pub fn lock_pair<'a>(
    a: &'a ContainerRef,
    b: &'a ContainerRef,
) -> (
    MutexGuard<'a, ItemContainer>,
    Option<MutexGuard<'a, ItemContainer>>,
) {
    if Arc::ptr_eq(a, b) {
        return (a.lock().expect("container poisoned"), None);
    }
    if Arc::as_ptr(a) < Arc::as_ptr(b) {
        let ga = a.lock().expect("container poisoned");
        let gb = b.lock().expect("container poisoned");
        (ga, Some(gb))
    } else {
        let gb = b.lock().expect("container poisoned");
        let ga = a.lock().expect("container poisoned");
        (ga, Some(gb))
    }
}

/// The player's `Inventory._itemContainers` (handles copied out of the character lock).
#[derive(Debug, Clone)]
pub struct Inv {
    pub character_id: u32,
    pub containers: Vec<(u8, ContainerRef)>,
}

impl Inv {
    pub fn of(player: &PlayerRef) -> Self {
        let c = player.data();
        Self {
            character_id: c.id,
            containers: c.inventory.containers.clone(),
        }
    }

    /// `_itemContainers.GetValueOrDefault(type)`.
    pub fn get(&self, st: u8) -> Option<ContainerRef> {
        self.containers
            .iter()
            .find(|(t, _)| *t == st)
            .map(|(_, c)| c.clone())
    }

    pub fn equipment(&self) -> ContainerRef {
        self.get(slot_type::EQUIPMENT).expect("equipment container")
    }

    pub fn bag(&self) -> ContainerRef {
        self.get(slot_type::INVENTORY).expect("bag container")
    }

    pub fn warehouse(&self) -> ContainerRef {
        self.get(slot_type::BANK).expect("bank container")
    }

    /// `GetItemById` (Equipment, Inventory and Bank only).
    pub fn item_by_id(&self, id: u64) -> Option<Item> {
        for (st, c) in &self.containers {
            if matches!(
                *st,
                slot_type::EQUIPMENT | slot_type::INVENTORY | slot_type::BANK
            ) {
                if let Some(i) = c.lock().expect("container poisoned").get_item_by_id(id) {
                    return Some(i.clone());
                }
            }
        }
        None
    }

    /// The container (Equipment / Inventory / Bank) that holds `id`.
    pub fn holder_of(&self, id: u64) -> Option<ContainerRef> {
        for (st, c) in &self.containers {
            if matches!(
                *st,
                slot_type::EQUIPMENT | slot_type::INVENTORY | slot_type::BANK
            ) && c
                .lock()
                .expect("container poisoned")
                .get_item_by_id(id)
                .is_some()
            {
                return Some(c.clone());
            }
        }
        None
    }

    /// Any of the player's containers that holds `id` (`item._holdingContainer`).
    pub fn holder_of_any(&self, id: u64) -> Option<ContainerRef> {
        self.containers
            .iter()
            .find(|(_, c)| {
                c.lock()
                    .expect("container poisoned")
                    .get_item_by_id(id)
                    .is_some()
            })
            .map(|(_, c)| c.clone())
    }

    /// `GetEquippedBySlot(slot)`.
    pub fn equipped(&self, slot: i32) -> Option<Item> {
        self.equipment()
            .lock()
            .expect("container poisoned")
            .get_item_by_slot(slot)
            .cloned()
    }

    /// `GetItem(type, slot)` (Equipment / Inventory / Bank; other types are TODO in C#).
    pub fn item_at(&self, st: u8, slot: u8) -> Option<Item> {
        match st {
            slot_type::EQUIPMENT | slot_type::INVENTORY | slot_type::BANK => self
                .get(st)?
                .lock()
                .expect("container poisoned")
                .get_item_by_slot(i32::from(slot))
                .cloned(),
            _ => None,
        }
    }

    /// `FreeSlotCount(type)`.
    pub fn free_slot_count(&self, st: u8) -> i32 {
        self.get(st).map_or(0, |c| {
            c.lock().expect("container poisoned").free_slot_count()
        })
    }

    /// `GetAllItemsByTemplate(inContainerTypes, templateId, gradeToCheck)`: (item ids, units).
    pub fn all_items_by_template(
        &self,
        types: Option<&[u8]>,
        template_id: u32,
        grade: i32,
    ) -> (Vec<u64>, i32) {
        let default = [slot_type::INVENTORY, slot_type::EQUIPMENT, slot_type::BANK];
        let types = match types {
            Some(t) if !t.is_empty() => t,
            _ => &default,
        };
        let mut ids = Vec::new();
        let mut units = 0;
        for st in types {
            if let Some(c) = self.get(*st) {
                let (i, u) = c
                    .lock()
                    .expect("container poisoned")
                    .items_by_template(template_id, grade);
                ids.extend(i);
                units += u;
            }
        }
        (ids, units)
    }

    /// [`all_items_by_template`](Self::all_items_by_template) without secured stacks.
    pub fn all_items_by_template_unlocked(
        &self,
        types: Option<&[u8]>,
        template_id: u32,
        grade: i32,
    ) -> (Vec<u64>, i32) {
        let default = [slot_type::INVENTORY, slot_type::EQUIPMENT, slot_type::BANK];
        let types = match types {
            Some(t) if !t.is_empty() => t,
            _ => &default,
        };
        let mut ids = Vec::new();
        let mut units = 0;
        for st in types {
            if let Some(c) = self.get(*st) {
                let (i, u) = c
                    .lock()
                    .expect("container poisoned")
                    .items_by_template_unlocked(template_id, grade);
                ids.extend(i);
                units += u;
            }
        }
        (ids, units)
    }

    /// `GetItemsCount(templateId, grade)` (Inventory + Equipment + Bank).
    pub fn items_count(&self, template_id: u32, grade: i32) -> i32 {
        self.all_items_by_template(None, template_id, grade).1
    }

    /// [`items_count`](Self::items_count) without secured stacks: a locked item cannot pay.
    pub fn items_count_unlocked(&self, template_id: u32, grade: i32) -> i32 {
        self.all_items_by_template_unlocked(None, template_id, grade)
            .1
    }

    /// `GetItemsCount(slotType, templateId, grade)`.
    pub fn items_count_in(&self, st: u8, template_id: u32, grade: i32) -> i32 {
        self.all_items_by_template(Some(&[st]), template_id, grade)
            .1
    }

    /// [`items_count_in`](Self::items_count_in) without secured stacks.
    pub fn items_count_in_unlocked(&self, st: u8, template_id: u32, grade: i32) -> i32 {
        self.all_items_by_template_unlocked(Some(&[st]), template_id, grade)
            .1
    }

    /// `CheckItems(slotType, templateId, count)`: locked stacks cannot satisfy a requirement.
    pub fn check_items(&self, st: u8, template_id: u32, count: i32) -> bool {
        let total = self.get(st).map_or(0, |c| {
            c.lock()
                .expect("container poisoned")
                .items_by_template_unlocked(template_id, -1)
                .1
        });
        total >= count
    }
}

/// `Inventory.ConsumeItem(containersToCheck, taskType, templateId, amount, preferredItem)`.
pub fn consume_item(
    txn: &mut ItemTxn<'_>,
    inv: &Inv,
    containers: Option<&[u8]>,
    task: u8,
    template_id: u32,
    mut amount: i32,
    preferred: Option<u64>,
) -> i32 {
    let default = [slot_type::INVENTORY, slot_type::EQUIPMENT, slot_type::BANK];
    let list = match containers {
        Some(c) if !c.is_empty() => c,
        _ => &default,
    };
    let mut res = 0;
    for st in list {
        if let Some(c) = inv.get(*st) {
            let used = c.lock().expect("container poisoned").consume_item(
                task,
                template_id,
                amount,
                preferred,
                &mut txn.env,
            );
            res += used;
            amount -= used;
        }
    }
    res
}

/// `SplitOrMoveItem`'s `SwapAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum SwapAction {
    DoNothing,
    DoSwap,
    DoSplit,
    DoMerge,
    DoMoveAllToEmpty,
    DoEquipInEmptySlot,
}

/// The client request of a `SplitOrMoveItem` call, for diagnostics.
#[derive(Debug, Clone, Copy)]
struct MoveRequest {
    task: u8,
    from_item_id: u64,
    from_type: u8,
    from_slot: u8,
    to_item_id: u64,
    to_type: u8,
    to_slot: u8,
    count: i32,
}

/// The "phantom item" message: the original C# text first (so log greps still match), then the
/// branch, the character and the whole request. `known_elsewhere` is `None` when the
/// server-wide lookup was not possible.
fn phantom_item_message(
    branch: &str,
    missing_id: u64,
    owner_id: u32,
    owner_name: &str,
    req: &MoveRequest,
    known_elsewhere: Option<bool>,
) -> String {
    let known = match known_elsewhere {
        Some(true) => "yes",
        Some(false) => "no",
        None => "n/a",
    };
    format!(
        "SplitOrMoveItem - ItemId {missing_id} no longer exists, possibly a phantom item ({branch}): character {owner_id} ({owner_name}), task {}, from item {} {}:{}, to item {} {}:{}, count {}, known elsewhere on server: {known}.",
        req.task,
        req.from_item_id,
        req.from_type,
        req.from_slot,
        req.to_item_id,
        req.to_type,
        req.to_slot,
        req.count
    )
}

/// Logs a request that names an item the player no longer holds. This is normally client
/// staleness (the item was consumed, merged or moved before the request arrived), so it is a
/// warning; the C# logged it with `Logger.Error`. `lookup_server` enables the server-wide
/// `ItemManager::item_by_id` lookup, which locks every container (not allowed while holding one).
fn log_phantom_item(
    server: &GameServer,
    inv: &Inv,
    branch: &str,
    missing_id: u64,
    req: &MoveRequest,
    lookup_server: bool,
) {
    let name = server
        .names
        .get_character_name(inv.character_id)
        .unwrap_or_default();
    let known = lookup_server.then(|| server.items.item_by_id(missing_id).is_some());
    tracing::warn!(
        "{}",
        phantom_item_message(branch, missing_id, inv.character_id, &name, req, known)
    );
}

/// What a rejected `SplitOrMoveItemEx` knows about the two items and the source container, for
/// diagnostics.
#[derive(Debug, Clone, Copy)]
struct MoveFacts<'a> {
    /// Count after the "no count provided" default was applied.
    effective_count: i32,
    from_item: Option<&'a Item>,
    target_item: Option<&'a Item>,
    /// The source template's `max_count` (0 = unknown template).
    max_count: i32,
    /// Source container id and slot type.
    src_container: (u64, u8),
}

fn describe_item(label: &str, item: Option<&Item>) -> String {
    match item {
        Some(i) => format!(
            "{label} template {} count {} at {}:{}",
            i.template_id, i.count, i.slot_type, i.slot
        ),
        None => format!("{label} none"),
    }
}

/// The rejection message of a `SplitOrMoveItem` validation: the original C# text first (so log
/// greps still match), then the reason details, the character, the whole request and the item
/// facts.
fn move_rejection_message(
    prefix: &str,
    detail: &str,
    owner_id: u32,
    owner_name: &str,
    req: &MoveRequest,
    facts: &MoveFacts<'_>,
) -> String {
    format!(
        "{prefix} ({detail}): character {owner_id} ({owner_name}), task {}, from item {} {}:{}, to item {} {}:{}, requested count {}, effective count {}, source container {} (slot type {}), {}, {}, source max_count {}.",
        req.task,
        req.from_item_id,
        req.from_type,
        req.from_slot,
        req.to_item_id,
        req.to_type,
        req.to_slot,
        req.count,
        facts.effective_count,
        facts.src_container.0,
        facts.src_container.1,
        describe_item("source item", facts.from_item),
        describe_item("target item", facts.target_item),
        facts.max_count
    )
}

/// Logs a client request that `SplitOrMoveItemEx` rejected. These are client-request
/// validations (normally a stale client view), so they are warnings; the C# logged them with
/// `Logger.Error`.
fn log_move_rejection(
    server: &GameServer,
    inv: &Inv,
    prefix: &str,
    detail: &str,
    req: &MoveRequest,
    facts: &MoveFacts<'_>,
) {
    let name = server
        .names
        .get_character_name(inv.character_id)
        .unwrap_or_default();
    tracing::warn!(
        "{}",
        move_rejection_message(prefix, detail, inv.character_id, &name, req, facts)
    );
}

/// `Inventory.SplitOrMoveItem(taskType, fromItemId, fromType, fromSlot, toItemId, toType,
/// toSlot, count)`.
#[allow(clippy::too_many_arguments)]
pub fn split_or_move_item(
    txn: &mut ItemTxn<'_>,
    inv: &Inv,
    task: u8,
    from_item_id: u64,
    from_type: u8,
    from_slot: u8,
    to_item_id: u64,
    to_type: u8,
    to_slot: u8,
    count: i32,
) -> bool {
    let holder = |id: u64| {
        if id == 0 {
            return None;
        }
        inv.holder_of_any(id)
    };
    let from_holder = holder(from_item_id);
    if matches!(from_type, slot_type::MAIL | slot_type::AUCTION)
        || matches!(to_type, slot_type::MAIL | slot_type::AUCTION)
        || from_holder.as_ref().is_some_and(|c| {
            matches!(
                c.lock().expect("container poisoned").slot_type,
                slot_type::MAIL | slot_type::AUCTION
            )
        })
    {
        // Escrow is released through the mail/auction operations, not generic item moves.
        return false;
    }
    if from_holder.is_none() && from_item_id != 0 {
        let req = MoveRequest {
            task,
            from_item_id,
            from_type,
            from_slot,
            to_item_id,
            to_type,
            to_slot,
            count,
        };
        log_phantom_item(txn.server, inv, "from-holder", from_item_id, &req, true);
        return false;
    }
    let to_holder = holder(to_item_id);
    if to_holder.as_ref().is_some_and(|c| {
        matches!(
            c.lock().expect("container poisoned").slot_type,
            slot_type::MAIL | slot_type::AUCTION
        )
    }) {
        return false;
    }
    if to_holder.is_none() && to_item_id != 0 {
        let req = MoveRequest {
            task,
            from_item_id,
            from_type,
            from_slot,
            to_item_id,
            to_type,
            to_slot,
            count,
        };
        log_phantom_item(txn.server, inv, "to-holder", to_item_id, &req, true);
        return false;
    }
    let src = from_holder.or_else(|| inv.get(from_type));
    let dst = to_holder.or_else(|| inv.get(to_type));
    split_or_move_item_ex(
        txn,
        inv,
        task,
        src,
        dst,
        from_item_id,
        from_type,
        from_slot,
        to_item_id,
        to_type,
        to_slot,
        count,
    )
}

/// Finds an item by id in one of two (locked) containers.
fn find_in<'b>(a: &'b ItemContainer, b: Option<&'b ItemContainer>, id: u64) -> Option<&'b Item> {
    if id == 0 {
        return None;
    }
    a.get_item_by_id(id)
        .or_else(|| b.and_then(|b| b.get_item_by_id(id)))
}

fn is_two_handed(data: &ItemData, item: Option<&Item>) -> bool {
    item.and_then(|i| data.template(i.template_id))
        .is_some_and(|t| matches!(&t.kind, TemplateKind::Weapon(e) if e.slot_type_id == gear_slot_type::TWO_HANDED))
}

/// Whether `from` may be merged into the stack `target` (same stackable template, and the same
/// grade, binding / secure / unpack flags and expiry / timers: the merge keeps the target's
/// values, so a differing stack would change grade or binding for free). Anything else falls
/// through to a swap.
fn stacks_merge(from: &Item, target: &Item, max_count: i32) -> bool {
    max_count > 1
        && from.template_id == target.template_id
        && from.grade == target.grade
        && from.flags == target.flags
        && from.expiration_time == target.expiration_time
        && from.unpack_time == target.unpack_time
        && from.unsecure_time == target.unsecure_time
}

/// A slot of a non-equipment container must lie inside its size (`size < 0` = unbounded);
/// equipment containers range-check in `ItemContainer::can_accept`.
fn slot_in_range(c: &ItemContainer, slot: u8) -> bool {
    c.is_equipment() || c.size() < 0 || i32::from(slot) < c.size()
}

/// `Inventory.SplitOrMoveItemEx`. Source / target items are looked up in the two containers
/// (the C# `GetItemByItemId` searches every item; an item that is in neither container is
/// treated as a phantom item here).
#[allow(clippy::too_many_arguments)]
pub fn split_or_move_item_ex(
    txn: &mut ItemTxn<'_>,
    inv: &Inv,
    task: u8,
    src: Option<ContainerRef>,
    dst: Option<ContainerRef>,
    from_item_id: u64,
    from_type: u8,
    from_slot: u8,
    to_item_id: u64,
    to_type: u8,
    to_slot: u8,
    mut count: i32,
) -> bool {
    tracing::trace!(
        "SplitOrMoveItem({from_item_id} {from_type}:{from_slot} => {to_item_id} {to_type}:{to_slot} - {count})"
    );
    let data = txn.data();
    let (Some(src), Some(dst)) = (src, dst) else {
        // C#: NullReferenceException on the missing container.
        tracing::error!(
            "SplitOrMoveItem - missing source or target container: character {}, task {task}, from item {from_item_id} {from_type}:{from_slot}, to item {to_item_id} {to_type}:{to_slot}, count {count}.",
            inv.character_id
        );
        return false;
    };

    // ---- Decide (read-only, under both container locks) ----
    let requested_count = count;
    let (action, from_item, target_item, src_type, dst_type) = {
        let (s, d) = lock_pair(&src, &dst);
        let dref = d.as_deref().unwrap_or(&s);
        let from_item = find_in(&s, d.as_deref(), from_item_id).cloned();
        if from_item.is_none() && from_item_id != 0 {
            let req = MoveRequest {
                task,
                from_item_id,
                from_type,
                from_slot,
                to_item_id,
                to_type,
                to_slot,
                count,
            };
            // The container locks are held here, so the server-wide lookup is skipped.
            log_phantom_item(txn.server, inv, "ex-decide", from_item_id, &req, false);
            return false;
        }
        if !slot_in_range(&s, from_slot) || !slot_in_range(dref, to_slot) {
            tracing::warn!(
                "SplitOrMoveItem - slot out of range: from {from_type}:{from_slot} => {to_type}:{to_slot}, owner {}.",
                inv.character_id
            );
            return false;
        }
        let mut action = SwapAction::DoNothing;
        let mut target_item = find_in(&s, d.as_deref(), to_item_id).cloned();
        // If no count provided, and we have a source item, use that item's total count instead
        if count <= 0 {
            if let Some(f) = &from_item {
                count = f.count;
            }
        }
        if target_item.is_none() {
            target_item = dref.get_item_by_slot(i32::from(to_slot)).cloned();
        }
        // A locked item cannot be split, merged, moved or swapped (the 1.2 client refuses every
        // drag that touches it with `item_secure_condition`; `FUN_390455d0`, `FUN_39045850`).
        // The packet handlers report 664 before reaching this core guard.
        if from_item
            .as_ref()
            .is_some_and(super::item_security::is_locked)
            || target_item
                .as_ref()
                .is_some_and(super::item_security::is_locked)
        {
            return false;
        }
        // Check if containers can accept the items
        if let Some(f) = &from_item {
            if !dref.can_accept(f, data.template(f.template_id), i32::from(to_slot)) {
                // C# logged this at Error; a client-driven rejection is a warning here.
                tracing::warn!(
                    "SplitOrMoveItem - fromItemId {from_item_id} is not welcome in this container {} ({}). Request: from {from_type}:{from_slot} => toItemId {to_item_id} {to_type}:{to_slot}, owner {}.",
                    dref.slot_type,
                    dref.container_id,
                    inv.character_id
                );
                return false;
            }
        }
        if let Some(t) = &target_item {
            if !s.can_accept(t, data.template(t.template_id), i32::from(from_slot)) {
                tracing::warn!(
                    "SplitOrMoveItem - toItemId {to_item_id} is not welcome in this container {} ({}). Request: fromItemId {from_item_id} {from_type}:{from_slot} => {to_type}:{to_slot}, owner {}.",
                    s.slot_type,
                    s.container_id,
                    inv.character_id
                );
                return false;
            }
        }
        // Equipping into an empty slot: the client sends FROM empty equipment slot => TO item
        if from_item_id == 0
            && from_type == slot_type::EQUIPMENT
            && to_type != slot_type::EQUIPMENT
            && target_item.is_some()
        {
            action = SwapAction::DoEquipInEmptySlot;
        }
        // Equipping into an empty mount gear slot
        if from_item_id == 0
            && from_type == slot_type::EQUIPMENT_MATE
            && to_type != slot_type::EQUIPMENT_MATE
            && target_item.is_some()
        {
            action = SwapAction::DoEquipInEmptySlot;
        }
        let equip_empty = action == SwapAction::DoEquipInEmptySlot;
        // The request as the client sent it (`count` may have been defaulted above).
        let req = MoveRequest {
            task,
            from_item_id,
            from_type,
            from_slot,
            to_item_id,
            to_type,
            to_slot,
            count: requested_count,
        };
        let facts = MoveFacts {
            effective_count: count,
            from_item: from_item.as_ref(),
            target_item: target_item.as_ref(),
            max_count: from_item
                .as_ref()
                .and_then(|f| data.template(f.template_id))
                .map_or(0, |t| t.max_count),
            src_container: (s.container_id, s.slot_type),
        };
        if !equip_empty && from_item.is_none() {
            log_move_rejection(
                txn.server,
                inv,
                "SplitOrMoveItem didn't provide a source itemId",
                "no source item and not an empty-slot equip",
                &req,
                &facts,
            );
            return false;
        }
        if !equip_empty && s.slot_type != from_type {
            log_move_rejection(
                txn.server,
                inv,
                "SplitOrMoveItem Source Item Container did not match what the client asked",
                &format!(
                    "client from_type {from_type} but the source item is in container {} of slot type {}",
                    s.container_id, s.slot_type
                ),
                &req,
                &facts,
            );
            return false;
        }
        if !equip_empty && from_item.as_ref().map(|f| f.slot) != Some(i32::from(from_slot)) {
            log_move_rejection(
                txn.server,
                inv,
                "SplitOrMoveItem Source Item slot did not match what the client asked",
                &format!(
                    "client from_slot {from_slot} but the source item is at slot {:?}",
                    from_item.as_ref().map(|f| f.slot)
                ),
                &req,
                &facts,
            );
            return false;
        }
        if !equip_empty && from_item.as_ref().is_some_and(|f| count > f.count) {
            log_move_rejection(
                txn.server,
                inv,
                "SplitOrMoveItem Source Item has less item count than is requested to be moved",
                &format!(
                    "requested {count} but the source stack holds {}",
                    from_item.as_ref().map_or(0, |f| f.count)
                ),
                &req,
                &facts,
            );
            return false;
        }
        if let Some(t) = &target_item {
            if t.slot_type != to_type {
                log_move_rejection(
                    txn.server,
                    inv,
                    "SplitOrMoveItem Target Item Type does not match",
                    &format!(
                        "client to_type {to_type} but the target item has slot type {}",
                        t.slot_type
                    ),
                    &req,
                    &facts,
                );
                return false;
            }
            if t.slot != i32::from(to_slot) {
                log_move_rejection(
                    txn.server,
                    inv,
                    "SplitOrMoveItem Target Item Slot does not match",
                    &format!(
                        "client to_slot {to_slot} but the target item is at slot {}",
                        t.slot
                    ),
                    &req,
                    &facts,
                );
                return false;
            }
            if let Some(f) = &from_item {
                let max = data.template(f.template_id).map_or(0, |t| t.max_count);
                if !equip_empty && stacks_merge(f, t, max) && t.count + count > max {
                    log_move_rejection(
                        txn.server,
                        inv,
                        "SplitOrMoveItem Target Item stack does not have enough room to take source",
                        &format!(
                            "target count {} + moved count {count} = {} exceeds template {} max_count {max}; source template {} target template {}",
                            t.count,
                            t.count + count,
                            t.template_id,
                            f.template_id,
                            t.template_id
                        ),
                        &req,
                        &facts,
                    );
                    return false;
                }
            }
        }
        if !equip_empty {
            let f = from_item.as_ref().expect("checked above");
            action = match &target_item {
                None if f.count > count => SwapAction::DoSplit,
                None if f.count == count => SwapAction::DoMoveAllToEmpty,
                Some(t)
                    if stacks_merge(
                        f,
                        t,
                        data.template(t.template_id).map_or(0, |x| x.max_count),
                    ) =>
                {
                    SwapAction::DoMerge
                }
                _ => SwapAction::DoSwap,
            };
            // `fromItem.Count < count` with no target item falls through to doSwap in C#
            // (impossible after the count check above).
        }
        (action, from_item, target_item, s.slot_type, dref.slot_type)
    };

    // The 1.2 client refuses the equip below the item's actability requirement before the move
    // is sent (`FUN_394e41f0` -> `FUN_394e1a90`, error 508); the same check guards every other
    // caller that can put an item into the equipment container. The packet handlers answer 508
    // themselves; this is the silent last line of defence.
    let incoming = if src_type == slot_type::EQUIPMENT
        && dst_type != slot_type::EQUIPMENT
        && matches!(action, SwapAction::DoEquipInEmptySlot | SwapAction::DoSwap)
    {
        // Equipping into an empty slot (`DoEquipInEmptySlot`) or swapping an occupied bag
        // slot: the target item moves into the equipment slot the source leaves.
        target_item.as_ref()
    } else if dst_type == slot_type::EQUIPMENT && src_type != slot_type::EQUIPMENT {
        from_item.as_ref()
    } else {
        None
    };
    if let (Some(item), Some(player)) =
        (incoming, txn.server.world.character_by_id(inv.character_id))
    {
        if data
            .template(item.template_id)
            .is_some_and(|t| !meets_item_actability(txn.server, &player, t))
        {
            return false;
        }
    }

    // ---- Two-handed weapon handling ----
    let mut un_equip_offhand = false;
    let mut un_equip_mainhand = false;
    let mut main_hand = None;
    let mut off_hand = None;
    if action == SwapAction::DoSwap || action == SwapAction::DoEquipInEmptySlot {
        main_hand = inv.equipped(equip_slot::MAINHAND);
        off_hand = inv.equipped(equip_slot::OFFHAND);
        let is_from_2h = is_two_handed(data, from_item.as_ref());
        let is_to_2h = is_two_handed(data, target_item.as_ref());
        let is_main_2h = is_two_handed(data, main_hand.as_ref());
        if is_to_2h
            && src_type == slot_type::EQUIPMENT
            && i32::from(from_slot) == equip_slot::MAINHAND
        {
            un_equip_offhand = true;
        }
        if is_main_2h
            && src_type == slot_type::EQUIPMENT
            && i32::from(from_slot) == equip_slot::OFFHAND
        {
            un_equip_mainhand = true;
        }
        if is_from_2h
            && dst_type == slot_type::EQUIPMENT
            && i32::from(to_slot) == equip_slot::MAINHAND
        {
            un_equip_offhand = true;
        }
        if is_main_2h
            && dst_type == slot_type::EQUIPMENT
            && i32::from(to_slot) == equip_slot::OFFHAND
        {
            un_equip_mainhand = true;
        }
    }
    let bag = inv.bag();
    let equipment = inv.equipment();
    if un_equip_offhand {
        if let Some(off) = &off_hand {
            if bag.lock().expect("container poisoned").free_slot_count() < 1 {
                return false;
            }
            let (mut b, e) = lock_pair(&bag, &equipment);
            let Some(mut e) = e else { return false };
            if !b.move_item_from(&mut e, task, off.id, -1, data, &mut txn.env.fx) {
                return false;
            }
        }
    }
    if un_equip_mainhand {
        if bag.lock().expect("container poisoned").free_slot_count() < 1 {
            return false;
        }
        // C#: `Bag.AddOrMoveExistingItem(taskType, mainHandWeapon)` (mainHandWeapon is non-null
        // here, isMain2H implies it).
        let Some(main) = &main_hand else { return false };
        let (mut b, e) = lock_pair(&bag, &equipment);
        let Some(mut e) = e else { return false };
        if !b.move_item_from(&mut e, task, main.id, -1, data, &mut txn.env.fx) {
            return false;
        }
    }

    // ---- Execute ----
    let mut item_tasks = Vec::new();
    let same = Arc::ptr_eq(&src, &dst);
    let (from_after, target_after) = {
        let (mut s, mut d) = lock_pair(&src, &dst);
        match action {
            SwapAction::DoEquipInEmptySlot => {
                let t = target_item.as_ref().expect("equip target");
                item_tasks.push(ItemTask::ItemMove {
                    from_slot_type: from_type,
                    from_slot,
                    from_item_id,
                    to_slot_type: to_type,
                    to_slot,
                    to_item_id: t.id,
                });
                let src_type = s.slot_type;
                if let Some(d) = d.as_deref_mut() {
                    if let Some(mut it) = d.remove_item(t.id) {
                        it.slot_type = src_type;
                        it.slot = i32::from(from_slot);
                        it.dirty = true;
                        s.items.push(it);
                        s.update_free_slot_count();
                    }
                } else if let Some(it) = s.get_item_by_id_mut(t.id) {
                    it.slot_type = src_type;
                    it.slot = i32::from(from_slot);
                    it.dirty = true;
                }
            }
            SwapAction::DoSplit => {
                let f = from_item.as_ref().expect("split source");
                let Some(fi) = s.get_item_by_id_mut(f.id) else {
                    return false;
                };
                let Some(mut ni) = txn.env.create(f.template_id, count, f.grade) else {
                    tracing::error!("SplitOrMoveItem - failed to create the split item");
                    return false;
                };
                fi.count -= count;
                fi.dirty = true;
                item_tasks.push(ItemTask::count_update(fi, -count));
                let target = d.as_deref_mut().unwrap_or(&mut s);
                ni.slot_type = target.slot_type;
                ni.slot = i32::from(to_slot);
                ni.owner_id = u64::from(target.owner_id);
                ni.dirty = true;
                item_tasks.push(ItemTask::add(&ni));
                target.items.push(ni);
                target.update_free_slot_count();
            }
            SwapAction::DoMoveAllToEmpty => {
                let f = from_item.as_ref().expect("move source");
                item_tasks.push(ItemTask::ItemMove {
                    from_slot_type: from_type,
                    from_slot,
                    from_item_id,
                    to_slot_type: to_type,
                    to_slot,
                    to_item_id,
                });
                if let Some(d) = d.as_deref_mut() {
                    if let Some(mut it) = s.remove_item(f.id) {
                        it.slot_type = d.slot_type;
                        it.slot = i32::from(to_slot);
                        it.dirty = true;
                        d.items.push(it);
                        d.update_free_slot_count();
                    }
                } else if let Some(it) = s.get_item_by_id_mut(f.id) {
                    it.slot = i32::from(to_slot);
                    it.dirty = true;
                }
            }
            SwapAction::DoMerge => {
                let f = from_item.as_ref().expect("merge source");
                let t = target_item.as_ref().expect("merge target");
                let max = data.template(t.template_id).map_or(0, |x| x.max_count);
                let to_add = count.min(max - t.count);
                if to_add < count {
                    tracing::trace!(
                        "SplitOrMoveItem supplied more than target can take, changed {count} to {to_add}"
                    );
                }
                let target_c = d.as_deref_mut().unwrap_or(&mut s);
                if let Some(ti) = target_c.get_item_by_id_mut(t.id) {
                    ti.count += to_add;
                    ti.dirty = true;
                }
                let mut from_left = 0;
                if let Some(fi) = s.get_item_by_id_mut(f.id) {
                    fi.count -= to_add;
                    fi.dirty = true;
                    from_left = fi.count;
                    if from_left > 0 {
                        item_tasks.push(ItemTask::count_update(fi, -to_add));
                    } else {
                        item_tasks.push(ItemTask::remove(fi));
                    }
                }
                if from_left <= 0 {
                    s.remove_item_task(task_type::INVALID, f.id, true, &mut txn.env);
                }
                let target_c = d.as_deref().unwrap_or(&s);
                if let Some(ti) = target_c.get_item_by_id(t.id) {
                    item_tasks.push(ItemTask::count_update(ti, to_add));
                }
            }
            SwapAction::DoSwap => {
                let f = from_item.as_ref().expect("swap source");
                let t = target_item.as_ref().expect("swap target");
                item_tasks.push(ItemTask::ItemMove {
                    from_slot_type: from_type,
                    from_slot,
                    from_item_id,
                    to_slot_type: to_type,
                    to_slot,
                    to_item_id: t.id,
                });
                let s_type = s.slot_type;
                match d.as_deref_mut() {
                    Some(d) => {
                        let d_type = d.slot_type;
                        if let Some(mut fi) = s.remove_item(f.id) {
                            fi.slot_type = d_type;
                            fi.slot = i32::from(to_slot);
                            fi.dirty = true;
                            d.items.push(fi);
                            d.update_free_slot_count();
                        }
                        if let Some(mut ti) = d.remove_item(t.id) {
                            ti.slot_type = s_type;
                            ti.slot = i32::from(from_slot);
                            ti.dirty = true;
                            s.items.push(ti);
                            s.update_free_slot_count();
                        }
                    }
                    None => {
                        if let Some(fi) = s.get_item_by_id_mut(f.id) {
                            fi.slot = i32::from(to_slot);
                            fi.dirty = true;
                        }
                        if let Some(ti) = s.get_item_by_id_mut(t.id) {
                            ti.slot = i32::from(from_slot);
                            ti.dirty = true;
                        }
                    }
                }
            }
            SwapAction::DoNothing => {
                // Should be impossible to get here
                tracing::error!("SplitOrMoveItem swap action not implemented {action:?}");
            }
        }

        // Force-assign item owners for safety: an item belongs to the container that holds it
        // after the move (the C# assigned the *source* owner to the moved source item and the
        // *target* owner to the target item wherever they ended up).
        let s_owner = s.owner_id;
        let d_owner = d.as_deref().map_or(s_owner, |d| d.owner_id);
        let mut from_after = None;
        let mut target_after = None;
        if let Some(f) = &from_item {
            if let Some(i) = s.get_item_by_id_mut(f.id) {
                i.owner_id = u64::from(s_owner);
                i.dirty = true;
                from_after = Some(i.clone());
            } else if let Some(i) = d.as_deref_mut().and_then(|h| h.get_item_by_id_mut(f.id)) {
                i.owner_id = u64::from(d_owner);
                i.dirty = true;
                from_after = Some(i.clone());
            } else {
                // Merged away: the event still carries the (now empty) item.
                from_after = Some(f.clone());
            }
        }
        if let Some(t) = &target_item {
            if let Some(i) = s.get_item_by_id_mut(t.id) {
                i.owner_id = u64::from(s_owner);
                i.dirty = true;
                target_after = Some(i.clone());
            } else if let Some(i) = d.as_deref_mut().and_then(|h| h.get_item_by_id_mut(t.id)) {
                i.owner_id = u64::from(d_owner);
                i.dirty = true;
                target_after = Some(i.clone());
            }
        }
        (from_after, target_after)
    };

    // Send Item manipulation packet
    if task != task_type::INVALID && !item_tasks.is_empty() {
        txn.env.fx.tasks(inv.character_id, task, item_tasks.clone());
    }

    // Send ItemContainer events
    {
        let (mut s, mut d) = lock_pair(&src, &dst);
        if !same {
            let dd = d.as_deref().expect("different containers");
            if let Some(f) = &from_after {
                s.notify_leave(f, from_slot, &mut txn.env.fx);
                dd.notify_enter(f, &mut txn.env.fx);
            }
            if let Some(t) = &target_after {
                // The target item left its old slot in the target container (`fromSlot` is the
                // slot it moved to in the source container, not its previous slot).
                dd.notify_leave(t, to_slot, &mut txn.env.fx);
                s.notify_enter(t, &mut txn.env.fx);
            }
        }
        s.apply_bind_rules_task(task, data, &mut txn.env.fx);
        if let Some(d) = d.as_deref_mut() {
            d.apply_bind_rules_task(task, data, &mut txn.env.fx);
        }
    }
    !item_tasks.is_empty()
}

/// `Inventory.SwapCofferItems` / `SplitCofferItems` (`count` = 0 for a swap).
#[allow(clippy::too_many_arguments)]
pub fn coffer_items(
    txn: &mut ItemTxn<'_>,
    inv: &Inv,
    task: u8,
    count: i32,
    from_item_id: u64,
    to_item_id: u64,
    from_type: u8,
    from_slot: u8,
    to_type: u8,
    to_slot: u8,
    db_id: u64,
) -> bool {
    let server = txn.server;
    let Some(player) = server.world.character_by_id(inv.character_id) else {
        return false;
    };
    let Some(world) = server.world.world_of(&WorldObject::from(&player)) else {
        return false;
    };
    let Some(doodad) = world.all_doodads().into_iter().find(|d| {
        d.data()
            .coffer
            .as_ref()
            .is_some_and(|c| c.container_id == db_id && c.opened_by == Some(inv.character_id))
    }) else {
        return false;
    };
    if !crate::systems::npcs::doodad::coffer::allowed_to_interact(server, &doodad, &player)
        || world
            .world_pos_rot(&WorldObject::from(&player))
            .distance_to(&world.world_pos_rot(&WorldObject::from(&doodad)), false)
            > 5.0
    {
        return false;
    }
    let coffer = txn.server.items.container_by_id(db_id);
    let pick = |st: u8| -> Option<ContainerRef> {
        if st == slot_type::TRADE {
            coffer.clone()
        } else {
            inv.get(st)
        }
    };
    let (Some(src), Some(dst)) = (pick(from_type), pick(to_type)) else {
        tracing::error!("SwapCofferItems, not all of the targetted containers exist");
        return false;
    };
    split_or_move_item_ex(
        txn,
        inv,
        task,
        Some(src),
        Some(dst),
        from_item_id,
        from_type,
        from_slot,
        to_item_id,
        to_type,
        to_slot,
        count,
    )
}

/// `Inventory.ExpandSlot(slotType)`.
pub fn expand_slot(txn: &mut ItemTxn<'_>, player: &PlayerRef, inv: &Inv, st: u8) {
    let server = txn.server;
    let is_bank = st == slot_type::BANK;
    let (num_bank, num_inv, money) = {
        let c = player.data();
        (c.num_bank_slots, c.num_inventory_slots, c.money)
    };
    let step = (if is_bank {
        i32::from(num_bank)
    } else {
        i32::from(num_inv)
    } - 50)
        / 10;
    let Some(expands) = server.game_data.systems.economy.expands(step) else {
        // C#: `_expands[step]` throws KeyNotFoundException (the packet handler logs it).
        tracing::error!("ExpandSlot: no bag_expands for step {step}");
        return;
    };
    let Some(expand) = expands.iter().find(|e| e.is_bank == is_bank).copied() else {
        return;
    };
    if expand.price != 0 && money < i64::from(expand.price) {
        tracing::warn!("No Money for expand!");
        return;
    }
    if expand.item_id != 0
        && expand.item_count != 0
        && !inv.check_items(slot_type::INVENTORY, expand.item_id, expand.item_count)
    {
        let (char_id, name) = {
            let c = player.data();
            (
                c.id,
                server.names.get_character_name(c.id).unwrap_or_default(),
            )
        };
        tracing::warn!(
            "Item or Count not fount (ExpandSlot, economy): character {char_id} ({name}), slot type {st} ({}), step {step}, required item template {} x{}, held in inventory {}.",
            if is_bank { "bank" } else { "bag" },
            expand.item_id,
            expand.item_count,
            inv.items_count_in(slot_type::INVENTORY, expand.item_id, -1)
        );
        return;
    }
    if expand.price != 0 {
        // `tasks.Add(new MoneyChange(-expand.Price))` was collected but never sent in C#; the
        // client needs the change to update its displayed money.
        player.data().money -= i64::from(expand.price);
        txn.env.fx.tasks(
            player.character_id,
            if is_bank {
                task_type::EXPAND_BANK
            } else {
                task_type::EXPAND_BAG
            },
            vec![ItemTask::MoneyChange(-expand.price)],
        );
    }
    if expand.item_id != 0 && expand.item_count != 0 {
        inv.bag().lock().expect("container poisoned").consume_item(
            if is_bank {
                task_type::EXPAND_BANK
            } else {
                task_type::EXPAND_BAG
            },
            expand.item_id,
            expand.item_count,
            None,
            &mut txn.env,
        );
    }
    let new_size = 50 + 10 * (1 + step);
    {
        let mut c = player.data();
        if is_bank {
            c.num_bank_slots = new_size as i16;
        } else {
            c.num_inventory_slots = new_size as u8;
        }
    }
    let target = if is_bank { inv.warehouse() } else { inv.bag() };
    target
        .lock()
        .expect("container poisoned")
        .set_container_size(new_size);
    txn.env.fx.send(
        player.character_id,
        SCInvenExpandedPacket {
            slot_type: if is_bank {
                slot_type::BANK
            } else {
                slot_type::INVENTORY
            },
            num_slots: new_size as u8,
        },
    );
}

/// `Inventory.OnItemManuallyDestroyed(item, count)`.
pub fn on_item_manually_destroyed(server: &GameServer, player: &PlayerRef, item: &Item) {
    // item?.OnManuallyDestroyingItem()
    if item.class == ItemClass::SummonSlave {
        if let Some(h) = server.systems.economy.hooks.summons.get() {
            h.on_slave_item_destroyed(server, item);
        }
    }
    let loot_quest_id = server
        .game_data
        .items
        .template(item.template_id)
        .map_or(0, |t| t.loot_quest_id);
    if loot_quest_id > 0 {
        if let Some(h) = server.systems.economy.hooks.quests.get() {
            h.on_quest_item_manually_destroyed(server, player, item, loot_quest_id);
        }
    }
}

/// Where a character's own packets go: straight to its connection, or queued with the item
/// effects of the running [`ItemTxn`] (the C# sends everything inline, so the order of item
/// task packets and these packets must be kept). Both variants carry the server, because an
/// upward money change reports the `MY_GOLD` achievement through it (queued in the item effects
/// or in the achievement deferral, never under the character or operation lock).
pub enum Out<'a> {
    Direct(&'a GameServer),
    Queue(&'a GameServer, &'a mut ItemFxList),
}

impl Out<'_> {
    pub fn send(
        &mut self,
        player: &PlayerRef,
        packet: impl crate::net::packet::ServerPacket + 'static,
    ) {
        match self {
            Out::Direct(_) => player.send(packet),
            Out::Queue(_, fx) => fx.push(ItemFx::Packet {
                owner: player.character_id,
                packet: Box::new(packet),
            }),
        }
    }
}

/// `EquipmentItemSlotType.Backpack` re-export for callers.
pub const BACKPACK_SLOT_TYPE: u32 = EQUIPMENT_ITEM_SLOT_TYPE_BACKPACK;

#[cfg(test)]
mod backpack_acquire_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod phantom_message_tests;
