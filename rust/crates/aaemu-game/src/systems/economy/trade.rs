//! Port of `Core/Managers/TradeManager.cs` (player to player trade) and its packets:
//! `CSCanStartTradePacket`, `CSCannotStartTradePacket`, `CSStartTradePacket`,
//! `CSCancelTradePacket`, `CSPutupTradeItemPacket`, `CSTakedownTradeItemPacket`,
//! `CSPutupTradeMoneyPacket`, `CSTradeLockPacket`, `CSTradeOkPacket` and `SCCanStartTrade`,
//! `SCCannotStartTrade`, `SCTradeStarted`, `SCTradeCanceled`, `SCTradeItemPutup`,
//! `SCOtherTradeItemPutup`, `SCTradeItemTookdown`, `SCOtherTradeItemTookdown`,
//! `SCTradeMoneyPutup`, `SCOtherTradeMoneyPutup`, `SCTradeLockUpdate`, `SCTradeOkUpdate`,
//! `SCTradeMade`.
//!
//! The trade table lives behind one mutex ([`TradeManager`]); it is never held while an
//! [`ItemTxn`] is started (the finished trade is taken out of the table first) and callers must
//! not hold a character or container lock when calling in.
//!
//! Departing players cancel their trades and pending invitations before world removal, so a
//! reconnect can start a fresh trade without first clearing stale object ids.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aaemu_commons::{Marshal, PacketStream};

use crate::error::Result;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemFx, ItemTask};
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::{cs, sc};
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::inventory::{lock_pair, Inv, ItemTxn};
use super::item_handlers::{active, sync_client_packet};

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

/// `TradeTemplate`. Items are kept by id (the C# keeps the `Item` objects).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TradeTemplate {
    pub id: u32,
    pub owner_obj_id: u32,
    pub target_obj_id: u32,
    pub lock_owner: bool,
    pub lock_target: bool,
    pub ok_owner: bool,
    pub ok_target: bool,
    /// `(item id, put-up amount)`: only this amount changes hands (a larger stack is split).
    pub owner_items: Vec<(u64, i32)>,
    pub target_items: Vec<(u64, i32)>,
    pub owner_money_putup: i32,
    pub target_money_putup: i32,
}

/// `TradeManager._trades` (insertion order, like the C# dictionary without removals).
#[derive(Debug, Default)]
pub struct TradeManager {
    trades: Mutex<Vec<TradeTemplate>>,
    /// (requester, recipient) -> expiry; a reply must match a server-issued invitation.
    invitations: Mutex<HashMap<(u32, u32), i64>>,
}

type Trades<'a> = MutexGuard<'a, Vec<TradeTemplate>>;

impl TradeManager {
    fn lock(&self) -> Trades<'_> {
        self.trades.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The running trades (copies, for tests / GM tools).
    pub fn trades(&self) -> Vec<TradeTemplate> {
        self.lock().clone()
    }

    fn invite(&self, owner: u32, target: u32, now: i64) {
        let mut invitations = self.invitations.lock().unwrap_or_else(|e| e.into_inner());
        invitations.retain(|_, expiry| *expiry > now);
        // One outstanding invitation per recipient matches the client's single dialog.
        invitations.retain(|(_, recipient), _| *recipient != target);
        invitations.insert((owner, target), now + 60_000);
    }

    fn take_invitation(&self, owner: u32, target: u32, now: i64) -> bool {
        self.invitations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(owner, target))
            .is_some_and(|expiry| expiry > now)
    }
}

fn manager(server: &GameServer) -> &TradeManager {
    &server.systems.economy.trade
}

/// `GetTradeId(objId)`.
fn get_trade_id(trades: &Trades<'_>, obj_id: u32) -> u32 {
    trades
        .iter()
        .find(|t| t.owner_obj_id == obj_id || t.target_obj_id == obj_id)
        .map_or(0, |t| t.id)
}

fn trade_mut<'a>(trades: &'a mut Trades<'_>, id: u32) -> Option<&'a mut TradeTemplate> {
    trades.iter_mut().find(|t| t.id == id)
}

fn character(server: &GameServer, obj_id: u32) -> Option<PlayerRef> {
    server.world.character_by_obj_id(obj_id)
}

fn name_of(p: &PlayerRef) -> String {
    p.data().unit.name.clone()
}

/// `IsTrading(objId)`: cancels a running trade of `obj_id` (reason 0) and reports it.
fn is_trading(server: &GameServer, trades: &mut Trades<'_>, obj_id: u32) -> bool {
    let trade_id = get_trade_id(trades, obj_id);
    if trade_id == 0 {
        return false;
    }
    cancel_locked(server, trades, obj_id, 0, trade_id); // TODO (C#) - reason?
    true
}

/// `CancelTrade(objId, reason, tradeId)` with the table locked.
fn cancel_locked(
    server: &GameServer,
    trades: &mut Trades<'_>,
    obj_id: u32,
    reason: i32,
    trade_id: u32,
) {
    // TODO (C#) - All reasons.
    let trade_id = if trade_id == 0 {
        get_trade_id(trades, obj_id)
    } else {
        trade_id
    };
    if trade_id == 0 {
        if let Some(p) = character(server, obj_id) {
            p.send(SCTradeCanceledPacket {
                reason,
                caused_by_me: true,
            });
        }
        return;
    }
    let Some(idx) = trades.iter().position(|t| t.id == trade_id) else {
        // C#: `_trades[tradeId]` throws KeyNotFoundException.
        return;
    };
    let t = trades.remove(idx);
    server.ids.trade.release_id(t.id);
    send_canceled(server, &t, obj_id, reason);
}

/// The packets of `CancelTrade` once the entry is removed. A missing partner makes the C#
/// throw (NullReferenceException in the log line) after the removal: nothing is sent.
fn send_canceled(server: &GameServer, t: &TradeTemplate, obj_id: u32, reason: i32) {
    let (Some(owner), Some(target)) = (
        character(server, t.owner_obj_id),
        character(server, t.target_obj_id),
    ) else {
        return;
    };
    tracing::info!(
        "Trade Id:{} between {}({}) - {}({}) is canceled.",
        t.id,
        name_of(&owner),
        owner.obj_id,
        name_of(&target),
        target.obj_id
    );
    let caused_by_me = owner.obj_id == obj_id;
    owner.send(SCTradeCanceledPacket {
        reason,
        caused_by_me,
    });
    target.send(SCTradeCanceledPacket {
        reason,
        caused_by_me: !caused_by_me,
    });
}

/// `UnlockTrade(owner, target, tradeId)`.
fn unlock_trade(t: &mut TradeTemplate, owner: &PlayerRef, target: &PlayerRef) {
    if !t.lock_owner && !t.lock_target {
        return;
    }
    t.lock_owner = false;
    t.lock_target = false;
    t.ok_owner = false;
    t.ok_target = false;
    owner.send(SCTradeLockUpdatePacket {
        my_lock: false,
        other_lock: false,
    });
    target.send(SCTradeLockUpdatePacket {
        my_lock: false,
        other_lock: false,
    });
    tracing::info!("Trade Id:{} Lockers opened and Ok undone.", t.id);
}

/// The two partners of a trade; `None` where the C# would throw on a missing character.
fn partners(server: &GameServer, t: &TradeTemplate) -> Option<(PlayerRef, PlayerRef)> {
    Some((
        character(server, t.owner_obj_id)?,
        character(server, t.target_obj_id)?,
    ))
}

/// `TradeManager.CanStartTrade(owner, target)`.
pub fn can_start_trade(server: &GameServer, owner: &PlayerRef, target: &PlayerRef) {
    if owner.obj_id == target.obj_id {
        return;
    }
    let mut trades = manager(server).lock();
    if is_trading(server, &mut trades, owner.obj_id)
        || is_trading(server, &mut trades, target.obj_id)
    {
        return;
    }
    // TODO (C#) - Check faction and others
    tracing::info!(
        "{}({}) is trying to trade with {}({}).",
        name_of(owner),
        owner.obj_id,
        name_of(target),
        target.obj_id
    );
    manager(server).invite(owner.obj_id, target.obj_id, crate::world::now_ms());
    target.send(SCCanStartTradePacket {
        obj_id: owner.obj_id,
    });
}

/// `TradeManager.StartTrade(owner, target)`.
pub fn start_trade(server: &GameServer, owner: &PlayerRef, target: &PlayerRef) {
    let mut trades = manager(server).lock();
    if is_trading(server, &mut trades, owner.obj_id)
        || is_trading(server, &mut trades, target.obj_id)
    {
        return;
    }
    let next_id = match server.ids.trade.get_next_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };
    trades.push(TradeTemplate {
        id: next_id,
        owner_obj_id: owner.obj_id,
        target_obj_id: target.obj_id,
        ..TradeTemplate::default()
    });
    tracing::info!(
        "Trade Id:{next_id} started between {}({}) - {}({}).",
        name_of(owner),
        owner.obj_id,
        name_of(target),
        target.obj_id
    );
    owner.send(SCTradeStartedPacket {
        obj_id: target.obj_id,
    });
    target.send(SCTradeStartedPacket {
        obj_id: owner.obj_id,
    });
}

/// `TradeManager.CancelTrade(objId, reason, tradeId = 0)`.
pub fn cancel_trade(server: &GameServer, obj_id: u32, reason: i32, trade_id: u32) {
    let mut trades = manager(server).lock();
    cancel_locked(server, &mut trades, obj_id, reason, trade_id);
}

/// Ends only this departing player's trade and invitations. Repeated lifecycle calls are
/// harmless; an old player handle must not cancel a new session reusing its object id.
pub fn on_player_departed(server: &GameServer, player: &PlayerRef) {
    if character(server, player.obj_id).is_some_and(|current| !Arc::ptr_eq(&current, player)) {
        return;
    }
    let mgr = manager(server);
    let mut trades = mgr.lock();
    mgr.invitations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(owner, target), _| *owner != player.obj_id && *target != player.obj_id);
    loop {
        let id = get_trade_id(&trades, player.obj_id);
        if id == 0 {
            break;
        }
        cancel_locked(server, &mut trades, player.obj_id, 0, id);
    }
}

/// `TradeManager.AddItem(character, slotType, slot, amount)`. Only `amount` units are traded:
/// a larger stack is split when the trade is made (the C# moved the whole item).
pub fn add_item(server: &GameServer, character: &PlayerRef, st: u8, slot: u8, amount: i32) {
    let mut trades = manager(server).lock();
    let trade_id = get_trade_id(&trades, character.obj_id);
    let item = Inv::of(character).item_at(st, slot);
    if trade_id == 0 {
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id); // TODO (C#) - Reason
        return;
    }
    // C#: `item.Count` throws on an empty slot.
    let Some(item) = item else { return };
    if amount <= 0 || item.count < amount {
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id); // TODO (C#) - Reason
        return;
    }
    // Only unbound bag items may be put up (as for mail and auction).
    if item.slot_type != slot_type::INVENTORY
        || item.flags & crate::models::item::ITEM_FLAG_SOUL_BOUND != 0
    {
        tracing::warn!(
            "Trade Id:{trade_id} {} put up a bound item or a non-bag slot ({st}-{slot}), refused.",
            name_of(character)
        );
        if item.flags & crate::models::item::ITEM_FLAG_SOUL_BOUND != 0 {
            crate::systems::social::util::send_error(
                character,
                super::error_message::TRADE_SOUL_BOUND_ITEM,
            );
        }
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id);
        return;
    }
    let Some(t) = trade_mut(&mut trades, trade_id) else {
        return;
    };
    let Some((owner, target)) = partners(server, t) else {
        return;
    };
    if t.owner_obj_id == character.obj_id {
        tracing::info!(
            "Trade Id:{trade_id} {}({}) added item ({st}-{slot}) Amount: {amount}.",
            name_of(&owner),
            owner.obj_id
        );
        t.owner_items.push((item.id, amount));
        owner.send(SCTradeItemPutupPacket {
            slot_type: st,
            slot,
            amount,
        });
        target.send(SCOtherTradeItemPutupPacket {
            item: shown_putup(&item, amount),
        });
    } else {
        tracing::info!(
            "Trade Id:{trade_id} {}({}) added item ({st}-{slot}) Amount: {amount}.",
            name_of(&target),
            target.obj_id
        );
        t.target_items.push((item.id, amount));
        owner.send(SCOtherTradeItemPutupPacket {
            item: shown_putup(&item, amount),
        });
        target.send(SCTradeItemPutupPacket {
            slot_type: st,
            slot,
            amount,
        });
    }
    // If trade was Locked, unlock both
    unlock_trade(t, &owner, &target);
}

/// `TradeManager.AddMoney(character, moneyAmount)`.
pub fn add_money(server: &GameServer, character: &PlayerRef, money_amount: i32) {
    let mut trades = manager(server).lock();
    let trade_id = get_trade_id(&trades, character.obj_id);
    let money = character.data().money;
    // A negative amount would be stored and then skipped when the trade is made; reject it.
    if trade_id == 0 || money_amount < 0 || money < i64::from(money_amount) {
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id); // TODO (C#) - Reason
        return;
    }
    let Some(t) = trade_mut(&mut trades, trade_id) else {
        return;
    };
    let Some((owner, target)) = partners(server, t) else {
        return;
    };
    if t.owner_obj_id == character.obj_id {
        tracing::info!(
            "Trade Id:{trade_id} {}({}) changed Money: {money_amount}.",
            name_of(&owner),
            owner.obj_id
        );
        t.owner_money_putup = money_amount;
        owner.send(SCTradeMoneyPutupPacket { money_amount });
        target.send(SCOtherTradeMoneyPutupPacket { money_amount });
    } else {
        tracing::info!(
            "Trade Id:{trade_id} {}({}) changed Money: {money_amount}.",
            name_of(&target),
            target.obj_id
        );
        t.target_money_putup = money_amount;
        owner.send(SCOtherTradeMoneyPutupPacket { money_amount });
        target.send(SCTradeMoneyPutupPacket { money_amount });
    }
    // If trade was Locked, unlock both
    unlock_trade(t, &owner, &target);
}

/// The item snapshot the peer sees for a put-up: only the offered units.
fn shown_putup(item: &Item, amount: i32) -> Item {
    let mut shown = item.clone();
    shown.count = amount;
    shown
}

/// `List.Remove(item)` of `RemoveItem`: takes the first put-up of `item_id` out of the offer and
/// returns the offered amount. The C# cleared the whole list when it held at most one entry
/// (whatever item was taken down); only the requested item leaves the offer here.
fn take_down(list: &mut Vec<(u64, i32)>, item_id: u64) -> Option<i32> {
    let i = list.iter().position(|&(id, _)| id == item_id)?;
    Some(list.remove(i).1)
}

/// `TradeManager.RemoveItem(character, slotType, slot)`.
pub fn remove_item(server: &GameServer, character: &PlayerRef, st: u8, slot: u8) {
    let mut trades = manager(server).lock();
    let trade_id = get_trade_id(&trades, character.obj_id);
    let item = Inv::of(character).item_at(st, slot);
    let Some(item) = item.filter(|_| trade_id != 0) else {
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id); // TODO (C#) - Reason
        return;
    };
    let Some(t) = trade_mut(&mut trades, trade_id) else {
        return;
    };
    let Some((owner, target)) = partners(server, t) else {
        return;
    };
    if t.owner_obj_id == character.obj_id {
        tracing::info!(
            "Trade Id:{trade_id} {}({}) tookdown item ({st}-{slot}).",
            name_of(&owner),
            owner.obj_id
        );
        let shown = shown_putup(
            &item,
            take_down(&mut t.owner_items, item.id).unwrap_or(item.count),
        );
        owner.send(SCTradeItemTookdownPacket {
            slot_type: st,
            slot,
        });
        target.send(SCOtherTradeItemTookdownPacket { item: shown });
    } else {
        tracing::info!(
            "Trade Id:{trade_id} {}({}) tookdown item ({st}-{slot}).",
            name_of(&target),
            target.obj_id
        );
        let shown = shown_putup(
            &item,
            take_down(&mut t.target_items, item.id).unwrap_or(item.count),
        );
        owner.send(SCOtherTradeItemTookdownPacket { item: shown });
        target.send(SCTradeItemTookdownPacket {
            slot_type: st,
            slot,
        });
    }
    // If trade was Locked, unlock both
    unlock_trade(t, &owner, &target);
}

/// `TradeManager.LockTrade(character, lock)`.
pub fn lock_trade(server: &GameServer, character: &PlayerRef, lock: bool) {
    let mut trades = manager(server).lock();
    let trade_id = get_trade_id(&trades, character.obj_id);
    if trade_id == 0 {
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id); // TODO (C#) - Reason
        return;
    }
    let Some(t) = trade_mut(&mut trades, trade_id) else {
        return;
    };
    let is_owner = t.owner_obj_id == character.obj_id;
    // Check if already locked
    if is_owner && t.lock_owner && lock {
        return;
    }
    if !is_owner && t.lock_target && lock {
        return;
    }
    let Some((owner, target)) = partners(server, t) else {
        return;
    };
    if !lock {
        t.lock_owner = false;
        t.lock_target = false;
        // Unlocking undoes both confirmations (like `UnlockTrade`); a stale OK would otherwise
        // let the next lock of both sides finish a trade nobody re-confirmed.
        t.ok_owner = false;
        t.ok_target = false;
        tracing::info!(
            "Trade Id:{trade_id} {}({}) - {}({}) unlocked trade.",
            name_of(&owner),
            owner.obj_id,
            name_of(&target),
            target.obj_id
        );
    } else if is_owner {
        t.lock_owner = true;
        tracing::info!(
            "Trade Id:{trade_id} {}({}) locked trade.",
            name_of(&owner),
            owner.obj_id
        );
    } else {
        t.lock_target = true;
        tracing::info!(
            "Trade Id:{trade_id} {}({}) locked trade.",
            name_of(&target),
            target.obj_id
        );
    }
    owner.send(SCTradeLockUpdatePacket {
        my_lock: t.lock_owner,
        other_lock: t.lock_target,
    });
    target.send(SCTradeLockUpdatePacket {
        my_lock: t.lock_target,
        other_lock: t.lock_owner,
    });
}

/// `TradeManager.OkTrade(character)`.
pub fn ok_trade(server: &GameServer, character: &PlayerRef) {
    let mut trades = manager(server).lock();
    let trade_id = get_trade_id(&trades, character.obj_id);
    if trade_id == 0 {
        cancel_locked(server, &mut trades, character.obj_id, 0, trade_id); // TODO (C#) - Reason
        return;
    }
    let Some(t) = trade_mut(&mut trades, trade_id) else {
        return;
    };
    let is_owner = t.owner_obj_id == character.obj_id;
    // Both sides must have locked before either can confirm.
    if !(t.lock_owner && t.lock_target) {
        return;
    }
    let Some((owner, target)) = partners(server, t) else {
        return;
    };
    if is_owner {
        t.ok_owner = true;
        tracing::info!(
            "Trade Id:{trade_id} {}({}) ok trade.",
            name_of(&owner),
            owner.obj_id
        );
    } else {
        t.ok_target = true;
        tracing::info!(
            "Trade Id:{trade_id} {}({}) ok trade.",
            name_of(&target),
            target.obj_id
        );
    }
    // Send ok status
    owner.send(SCTradeOkUpdatePacket {
        my_ok: t.ok_owner,
        other_ok: t.ok_target,
    });
    target.send(SCTradeOkUpdatePacket {
        my_ok: t.ok_target,
        other_ok: t.ok_owner,
    });

    // If both ok finish trade
    if !(t.ok_owner && t.ok_target) {
        return;
    }
    // Check inventory space. The C# cancels and then throws (KeyNotFoundException) on the
    // removed entry in the next check / FinishTrade: one cancel, no trade.
    let (owner_items, target_items) = (t.owner_items.len(), t.target_items.len());
    if (Inv::of(&owner).free_slot_count(slot_type::INVENTORY) as i64) < target_items as i64 {
        cancel_locked(server, &mut trades, owner.obj_id, 0, trade_id);
        return;
    }
    if (Inv::of(&target).free_slot_count(slot_type::INVENTORY) as i64) < owner_items as i64 {
        cancel_locked(server, &mut trades, target.obj_id, 0, trade_id);
        return;
    }
    // Finish trade (the entry leaves the table before the item operation starts)
    let Some(idx) = trades.iter().position(|t| t.id == trade_id) else {
        return;
    };
    let t = trades.remove(idx);
    server.ids.trade.release_id(t.id);
    drop(trades);
    finish_trade(server, &owner, &target, t);
}

/// `TradeManager.FinishTrade(owner, target, tradeId)`; `trade` was already taken out of the
/// table.
pub fn finish_trade(
    server: &GameServer,
    owner: &PlayerRef,
    target: &PlayerRef,
    trade: TradeTemplate,
) {
    let trade_id = trade.id;
    let owner_put = i64::from(trade.owner_money_putup);
    let target_put = i64::from(trade.target_money_putup);
    // Negative put-ups were accepted and then silently skipped at the finish; reject them.
    if owner_put < 0 || target_put < 0 {
        send_canceled(server, &trade, owner.obj_id, 0); // Reason?
        tracing::error!(
            "Trade Id:{trade_id} has a negative money put-up ({} / {}), possible exploit or modified client!",
            trade.owner_money_putup,
            trade.target_money_putup
        );
        return;
    }
    // Validate Money (custom client protection) and apply the transfers with overflow checks.
    let owner_money = owner.data().money;
    if owner_put > owner_money {
        send_canceled(server, &trade, owner.obj_id, 0); // Reason?
        let c = owner.data();
        tracing::error!(
            "{} ({}) is putting up more money for trade than have {} > {}, possible exploit or modified client!",
            c.unit.name,
            c.id,
            trade.owner_money_putup,
            c.money
        );
        return;
    }
    let target_money = target.data().money;
    if target_put > target_money {
        send_canceled(server, &trade, target.obj_id, 0); // Reason?
        let c = target.data();
        tracing::error!(
            "{} ({}) is putting up more money for trade than have {} > {}, possible exploit or modified client!",
            c.unit.name,
            c.id,
            trade.target_money_putup,
            c.money
        );
        return;
    }
    // Money after the transfer, checked before anything is changed.
    if owner_money
        .checked_sub(owner_put)
        .and_then(|money| money.checked_add(target_put))
        .is_none()
    {
        send_canceled(server, &trade, owner.obj_id, 0);
        tracing::error!("Trade Id:{trade_id} money overflow, possible exploit or modified client!");
        return;
    }
    if target_money
        .checked_sub(target_put)
        .and_then(|money| money.checked_add(owner_put))
        .is_none()
    {
        send_canceled(server, &trade, target.obj_id, 0);
        tracing::error!("Trade Id:{trade_id} money overflow, possible exploit or modified client!");
        return;
    }

    // The item operation lock keeps the item containers still from the preflight until the
    // items are moved.
    let mut txn = ItemTxn::begin(server);
    // Preflight every offered item before anything is changed: it must still exist in the
    // giver's containers with (at least) the offered amount and the receiver must have room
    // for it. A stack that shrank or moved after the lock would otherwise leave a trade where
    // one side paid and got nothing back.
    for (giver, receiver, items) in [
        (owner, target, &trade.owner_items),
        (target, owner, &trade.target_items),
    ] {
        if let Err(why) = preflight_items(&txn, giver, receiver, items) {
            send_canceled(server, &trade, giver.obj_id, 0);
            tracing::error!(
                "Trade Id:{trade_id} aborted, {} ({}): {why}, possible exploit or modified client!",
                name_of(giver),
                giver.character_id
            );
            return;
        }
    }

    let mut has_errors = 0;
    let mut tasks_owner = Vec::new();
    let mut tasks_target = Vec::new();

    // Money moves as deltas, each under a single character guard (no absolute write from an
    // earlier read).
    if !apply_money_delta(owner, target_put - owner_put) {
        send_canceled(server, &trade, owner.obj_id, 0);
        tracing::error!("Trade Id:{trade_id} money changed during the trade, aborted!");
        return;
    }
    if !apply_money_delta(target, owner_put - target_put) {
        apply_money_delta(owner, owner_put - target_put);
        send_canceled(server, &trade, target.obj_id, 0);
        tracing::error!("Trade Id:{trade_id} money changed during the trade, aborted!");
        return;
    }
    // Handle Money from Owner
    if trade.owner_money_putup > 0 {
        tasks_owner.push(ItemTask::MoneyChange(-trade.owner_money_putup));
        tasks_target.push(ItemTask::MoneyChange(trade.owner_money_putup));
    }
    // Handle Money from Target
    if trade.target_money_putup > 0 {
        tasks_owner.push(ItemTask::MoneyChange(trade.target_money_putup));
        tasks_target.push(ItemTask::MoneyChange(-trade.target_money_putup));
    }

    // Handle Items from Owner
    for &(item_id, amount) in &trade.owner_items {
        match move_amount_to_bag(&mut txn, owner, target, item_id, amount) {
            Some((removed, added)) => {
                tasks_owner.push(removed);
                tasks_target.push(added);
            }
            None => has_errors += 1,
        }
    }
    // Handle Items from Target
    for &(item_id, amount) in &trade.target_items {
        match move_amount_to_bag(&mut txn, target, owner, item_id, amount) {
            Some((removed, added)) => {
                tasks_target.push(removed);
                tasks_owner.push(added);
            }
            None => has_errors += 1,
        }
    }

    // Trade complete, send item task packets
    txn.env.fx.send(
        owner.character_id,
        SCTradeMadePacket {
            action: task_type::TRADE,
            tasks: tasks_owner,
            force_remove: Vec::new(),
        },
    );
    txn.env.fx.send(
        target.character_id,
        SCTradeMadePacket {
            action: task_type::TRADE,
            tasks: tasks_target,
            force_remove: Vec::new(),
        },
    );
    drop(txn);
    let (on, oi) = {
        let c = owner.data();
        (c.unit.name.clone(), c.id)
    };
    let (tn, ti) = {
        let c = target.data();
        (c.unit.name.clone(), c.id)
    };
    tracing::info!(
        "Trade Id:{trade_id} finished. Owner {on} ({oi}) Items/Money: {}/{} <=> Target {tn} ({ti}) Items/Money: {}/{}",
        trade.owner_items.len(),
        trade.owner_money_putup,
        trade.target_items.len(),
        trade.target_money_putup
    );
    if has_errors > 0 {
        tracing::error!(
            "{has_errors}item(s) could not be trade for tradeId: {trade_id} between {on} ({oi}) and {tn} ({ti}), possible exploit or modified client!"
        );
    }
}

/// Adds `delta` to the character's money under one guard; `false` (nothing changed) when the
/// result would be negative or overflow.
fn apply_money_delta(player: &PlayerRef, delta: i64) -> bool {
    let mut c = player.data();
    match c.money.checked_add(delta) {
        Some(money) if money >= 0 => {
            c.money = money;
            true
        }
        _ => false,
    }
}

/// Checks that all of `items` (item id, offered amount) can be moved from `giver` into
/// `receiver`'s bag: every item is still held by the giver with at least the offered amount
/// (several put-ups of one item add up) and the receiver's bag accepts each put-up in a free slot.
fn preflight_items(
    txn: &ItemTxn<'_>,
    giver: &PlayerRef,
    receiver: &PlayerRef,
    items: &[(u64, i32)],
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    let giver_inv = Inv::of(giver);
    let mut totals: HashMap<u64, i64> = HashMap::new();
    let mut offered = Vec::with_capacity(items.len());
    for &(item_id, amount) in items {
        if amount <= 0 {
            return Err(format!("item {item_id} is offered with amount {amount}"));
        }
        let item = giver_inv
            .holder_of_any(item_id)
            .and_then(|c| {
                c.lock()
                    .expect("container poisoned")
                    .get_item_by_id(item_id)
                    .cloned()
            })
            .ok_or_else(|| format!("item {item_id} is no longer in the owner's containers"))?;
        if item.slot_type != slot_type::INVENTORY
            || item.flags & crate::models::item::ITEM_FLAG_SOUL_BOUND != 0
        {
            return Err(format!("item {item_id} left the bag or became bound"));
        }
        let total = totals.entry(item_id).or_insert(0);
        *total += i64::from(amount);
        if *total > i64::from(item.count) {
            return Err(format!(
                "item {item_id} has {} left but {total} are offered",
                item.count
            ));
        }
        offered.push(shown_putup(&item, amount));
    }
    let receiver_inv = Inv::of(receiver);
    let bag = receiver_inv.bag();
    let bag = bag.lock().expect("container poisoned");
    if i64::from(bag.free_slot_count()) < offered.len() as i64 {
        return Err("the receiver's bag has too few free slots".to_string());
    }
    // Put-ups are placed with preferred slot -1 and never merge: each takes a free slot.
    for item in &offered {
        if bag.place(item, -1, txn.data()).is_none() {
            return Err(format!(
                "the receiver's bag does not accept item {}",
                item.id
            ));
        }
    }
    Ok(())
}

/// Moves `amount` units of `item_id` from `from` into `to`'s bag, splitting a larger stack
/// (the C# moved the whole item). Returns the giver's removal task and the receiver's add task
/// for `SCTradeMade`; the removal names the slot the item had in the giver's container. An item
/// that is gone or shorter than its put-up counts as an error.
fn move_amount_to_bag(
    txn: &mut ItemTxn<'_>,
    from: &PlayerRef,
    to: &PlayerRef,
    item_id: u64,
    amount: i32,
) -> Option<(ItemTask, ItemTask)> {
    if amount <= 0 {
        return None;
    }
    let src = Inv::of(from).holder_of_any(item_id)?;
    let bag = Inv::of(to).bag();
    let data = txn.data();
    let (mut s, b) = lock_pair(&src, &bag);
    let mut b = b?; // different players never share a container
    let source = s.get_item_by_id(item_id)?.clone();
    if source.count < amount {
        return None;
    }
    if amount < source.count {
        let split = txn.env.create(source.template_id, amount, source.grade)?;
        let split_id = split.id;
        if let Some(item) = s.get_item_by_id_mut(item_id) {
            item.count -= amount;
            item.dirty = true;
        }
        match b.add_item(task_type::INVALID, split, -1, data, &mut txn.env.fx) {
            Ok(true) => {
                // Preferred slot -1 never merges: the new stack sits in a free slot.
                let added = ItemTask::add(b.get_item_by_id(split_id)?);
                Some((ItemTask::count_update(&source, -amount), added))
            }
            _ => {
                if let Some(item) = s.get_item_by_id_mut(item_id) {
                    item.count += amount;
                    item.dirty = true;
                }
                txn.env.fx.push(ItemFx::ReleaseId(split_id));
                None
            }
        }
    } else {
        if !b.move_item_from(
            &mut s,
            task_type::INVALID,
            item_id,
            -1,
            data,
            &mut txn.env.fx,
        ) {
            return None;
        }
        let added = ItemTask::add(b.get_item_by_id(item_id)?);
        Some((ItemTask::remove(&source), added))
    }
}

// ---------------------------------------------------------------------------------------------
// S->C packets
// ---------------------------------------------------------------------------------------------

/// `SCCanStartTradePacket(objId)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCCanStartTradePacket {
    pub obj_id: u32,
}
packet_def!(
    SCCanStartTradePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCCanStartTradePacket
);
impl Marshal for SCCanStartTradePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id);
    }
}

/// `SCCannotStartTradePacket(objId, reason)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCCannotStartTradePacket {
    pub obj_id: u32,
    pub reason: i32,
}
packet_def!(
    SCCannotStartTradePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCCannotStartTradePacket
);
impl Marshal for SCCannotStartTradePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id).write_i32(self.reason);
    }
}

/// `SCTradeStartedPacket(objId)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeStartedPacket {
    pub obj_id: u32,
}
packet_def!(
    SCTradeStartedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeStartedPacket
);
impl Marshal for SCTradeStartedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id);
    }
}

/// `SCTradeCanceledPacket(reason, causedByMe)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeCanceledPacket {
    pub reason: i32,
    pub caused_by_me: bool,
}
packet_def!(
    SCTradeCanceledPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeCanceledPacket
);
impl Marshal for SCTradeCanceledPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.reason).write_bool(self.caused_by_me);
    }
}

/// `SCTradeItemPutupPacket(slotType, slot, amount)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeItemPutupPacket {
    pub slot_type: u8,
    pub slot: u8,
    pub amount: i32,
}
packet_def!(
    SCTradeItemPutupPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeItemPutupPacket
);
impl Marshal for SCTradeItemPutupPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.slot_type) // type
            .write_u8(self.slot) // index
            .write_i32(self.amount); // amount
    }
}

/// `SCOtherTradeItemPutupPacket(item)`.
#[derive(Debug, Clone, PartialEq)]
pub struct SCOtherTradeItemPutupPacket {
    pub item: Item,
}
packet_def!(
    SCOtherTradeItemPutupPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCOtherTradeItemPutupPacket
);
impl Marshal for SCOtherTradeItemPutupPacket {
    fn write(&self, s: &mut PacketStream) {
        self.item.write(s);
    }
}

/// `SCTradeItemTookdownPacket(slotType, slot)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeItemTookdownPacket {
    pub slot_type: u8,
    pub slot: u8,
}
packet_def!(
    SCTradeItemTookdownPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeItemTookdownPacket
);
impl Marshal for SCTradeItemTookdownPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.slot_type).write_u8(self.slot);
    }
}

/// `SCOtherTradeItemTookdownPacket(item)`.
#[derive(Debug, Clone, PartialEq)]
pub struct SCOtherTradeItemTookdownPacket {
    pub item: Item,
}
packet_def!(
    SCOtherTradeItemTookdownPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCOtherTradeItemTookdownPacket
);
impl Marshal for SCOtherTradeItemTookdownPacket {
    fn write(&self, s: &mut PacketStream) {
        self.item.write(s);
    }
}

/// `SCTradeMoneyPutupPacket(moneyAmount)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeMoneyPutupPacket {
    pub money_amount: i32,
}
packet_def!(
    SCTradeMoneyPutupPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeMoneyPutupPacket
);
impl Marshal for SCTradeMoneyPutupPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.money_amount);
    }
}

/// `SCOtherTradeMoneyPutupPacket(moneyAmount)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCOtherTradeMoneyPutupPacket {
    pub money_amount: i32,
}
packet_def!(
    SCOtherTradeMoneyPutupPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCOtherTradeMoneyPutupPacket
);
impl Marshal for SCOtherTradeMoneyPutupPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.money_amount);
    }
}

/// `SCTradeLockUpdatePacket(myLock, otherLock)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeLockUpdatePacket {
    pub my_lock: bool,
    pub other_lock: bool,
}
packet_def!(
    SCTradeLockUpdatePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeLockUpdatePacket
);
impl Marshal for SCTradeLockUpdatePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.my_lock).write_bool(self.other_lock);
    }
}

/// `SCTradeOkUpdatePacket(myOk, otherOk)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTradeOkUpdatePacket {
    pub my_ok: bool,
    pub other_ok: bool,
}
packet_def!(
    SCTradeOkUpdatePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeOkUpdatePacket
);
impl Marshal for SCTradeOkUpdatePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.my_ok).write_bool(self.other_ok);
    }
}

/// `SCTradeMadePacket(action, tasks, forceRemove)`.
#[derive(Debug, Clone, PartialEq)]
pub struct SCTradeMadePacket {
    pub action: u8,
    pub tasks: Vec<ItemTask>,
    pub force_remove: Vec<u64>,
}
packet_def!(
    SCTradeMadePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTradeMadePacket
);
impl Marshal for SCTradeMadePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.action);
        s.write_u8(self.tasks.len() as u8); // TODO (C#) max count 30
        for t in &self.tasks {
            t.write(s);
        }
        s.write_u8(self.force_remove.len() as u8); // TODO (C#) max count 30
        for r in &self.force_remove {
            s.write_u64(*r);
        }
        s.write_u32(0); // type(id)
    }
}

// ---------------------------------------------------------------------------------------------
// C->S packets
// ---------------------------------------------------------------------------------------------

pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSStartTradePacket>(LEVEL_PLAIN, cs::CSStartTradePacket);
    reg.register::<CSCanStartTradePacket>(LEVEL_PLAIN, cs::CSCanStartTradePacket);
    reg.register::<CSCannotStartTradePacket>(LEVEL_PLAIN, cs::CSCannotStartTradePacket);
    reg.register::<CSCancelTradePacket>(LEVEL_PLAIN, cs::CSCancelTradePacket);
    reg.register::<CSPutupTradeItemPacket>(LEVEL_PLAIN, cs::CSPutupTradeItemPacket);
    reg.register::<CSPutupTradeMoneyPacket>(LEVEL_PLAIN, cs::CSPutupTradeMoneyPacket);
    reg.register::<CSTakedownTradeItemPacket>(LEVEL_PLAIN, cs::CSTakedownTradeItemPacket);
    reg.register::<CSTradeLockPacket>(LEVEL_PLAIN, cs::CSTradeLockPacket);
    reg.register::<CSTradeOkPacket>(LEVEL_PLAIN, cs::CSTradeOkPacket);
}

/// `CSCanStartTradePacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCanStartTradePacket {
    pub obj_id: u32,
}

impl CSCanStartTradePacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(target) = character(server, self.obj_id) else {
            return;
        };
        let Some(owner) = active(conn) else { return };
        can_start_trade(server, &owner, &target);
    }
}
sync_client_packet!(CSCanStartTradePacket);

/// `CSCannotStartTradePacket`: forward a decline only to its pending requester.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCannotStartTradePacket {
    pub obj_id: u32,
    pub reason: i32,
}

impl CSCannotStartTradePacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            reason: s.read_i32()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(target) = active(conn) else { return };
        if !manager(server).take_invitation(self.obj_id, target.obj_id, crate::world::now_ms()) {
            return;
        }
        if let Some(owner) = character(server, self.obj_id) {
            owner.send(SCCannotStartTradePacket {
                obj_id: target.obj_id,
                reason: self.reason,
            });
        }
    }
}
sync_client_packet!(CSCannotStartTradePacket);

/// `CSStartTradePacket`: the accepting player (`ActiveChar`) is the trade's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSStartTradePacket {
    pub obj_id: u32,
}

impl CSStartTradePacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(owner) = character(server, self.obj_id) else {
            return;
        };
        let Some(target) = active(conn) else { return };
        if !manager(server).take_invitation(owner.obj_id, target.obj_id, crate::world::now_ms()) {
            return;
        }
        start_trade(server, &owner, &target);
    }
}
sync_client_packet!(CSStartTradePacket);

/// `CSCancelTradePacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCancelTradePacket {
    pub reason: i32,
}

impl CSCancelTradePacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            reason: s.read_i32()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        tracing::warn!("CancelTrade, Reason: {}", self.reason);
        let Some(p) = active(conn) else { return };
        cancel_trade(server, p.obj_id, self.reason, 0);
    }
}
sync_client_packet!(CSCancelTradePacket);

/// `CSPutupTradeItemPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSPutupTradeItemPacket {
    pub slot_type: u8,
    pub slot: u8,
    pub amount: i32,
}

impl CSPutupTradeItemPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            slot_type: s.read_u8()?,
            slot: s.read_u8()?,
            amount: s.read_i32()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        add_item(server, &p, self.slot_type, self.slot, self.amount);
    }
}
sync_client_packet!(CSPutupTradeItemPacket);

/// `CSPutupTradeMoneyPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSPutupTradeMoneyPacket {
    pub money_amount: i32,
}

impl CSPutupTradeMoneyPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            money_amount: s.read_i32()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        add_money(server, &p, self.money_amount);
    }
}
sync_client_packet!(CSPutupTradeMoneyPacket);

/// `CSTakedownTradeItemPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSTakedownTradeItemPacket {
    pub slot_type: u8,
    pub slot: u8,
}

impl CSTakedownTradeItemPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            slot_type: s.read_u8()?,
            slot: s.read_u8()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        remove_item(server, &p, self.slot_type, self.slot);
    }
}
sync_client_packet!(CSTakedownTradeItemPacket);

/// `CSTradeLockPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSTradeLockPacket {
    pub lock: bool,
}

impl CSTradeLockPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            lock: s.read_bool()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        lock_trade(server, &p, self.lock);
    }
}
sync_client_packet!(CSTradeLockPacket);

/// `CSTradeOkPacket` (empty body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSTradeOkPacket;

impl CSTradeOkPacket {
    fn read_body(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        ok_trade(server, &p);
    }
}
sync_client_packet!(CSTradeOkPacket);

#[cfg(test)]
mod invitation_tests {
    use super::TradeManager;

    #[test]
    fn invitations_require_matching_peers_and_expire_at_the_deadline() {
        let manager = TradeManager::default();
        manager.invite(100, 101, 1000);
        assert!(!manager.take_invitation(100, 102, 1001));
        assert!(manager.take_invitation(100, 101, 60_999));
        assert!(!manager.take_invitation(100, 101, 60_999));
        manager.invite(100, 101, 1000);
        assert!(!manager.take_invitation(100, 101, 61_000));
    }
}
