use std::collections::HashMap;

use chrono::{NaiveDateTime, Utc};

use crate::db;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory::ItemTxn;
use super::super::{error_message, inventory};
use super::persistence::same_character_name;
use super::{
    currency, limit_type, manager, restrict_sale_type, AuditIcsSale, BuyTarget, CashShopDb,
    CommercialMailRequest, IcsSku, ReceiverLookup, SCBmPointPacket, SCICSBuyResultPacket,
    SCICSCashPointPacket, SCICSSyncGoodPacket,
};

// ---------------------------------------------------------------------------------------------
// CashShopBuyTask
// ---------------------------------------------------------------------------------------------

fn send_error(p: &PlayerRef, error: i16) {
    inventory::send_error_message(p, error, 0, true);
}

/// The generic checkout failure text: `INGAME_SHOP_BUY_GIFT_FAIL` (594) for a gift,
/// `INGAME_SHOP_BUY_FAIL` (590) otherwise. Neither id is in the client's error -> BFR reason
/// map (`FUN_39040000`, a switch on `id - 591`): both fall through to its default reason 1, the
/// generic failure result, so they differ only in the text of the error message the buyer sees.
fn buy_fail_error(is_gift: bool) -> i16 {
    if is_gift {
        error_message::INGAME_SHOP_BUY_GIFT_FAIL
    } else {
        error_message::INGAME_SHOP_BUY_FAIL
    }
}

pub(super) fn fail(buyer: &PlayerRef, error: i16, buy_mode: u8, target_name: &str) {
    send_error(buyer, error);
    buyer.send(SCICSBuyResultPacket {
        success: false,
        buy_mode,
        receiver_name: target_name.to_string(),
        charge_aa_point: 0,
    });
}

/// Resolve a gift receiver by name: the online world first (`WorldManager.character_by_name`),
/// else the `characters` table. Same-account receivers are checked by the caller (the buyer's
/// own account is the first case of that rule).
pub async fn resolve_receiver(server: &GameServer, name: &str) -> ReceiverLookup {
    if let Some(player) = server.world.character_by_name(name) {
        let (id, name, account_id, transfer_requested) = {
            let c = player.data();
            (
                c.id,
                c.unit.name.clone(),
                c.account_id,
                c.transfer_request_time != 0,
            )
        };
        if transfer_requested {
            return ReceiverLookup::TransferRequested;
        }
        return ReceiverLookup::Found(BuyTarget {
            id,
            name,
            account_id,
            player: Some(player),
        });
    }
    match manager(server)
        .store()
        .find_character_by_name(server, name)
        .await
    {
        // Stand-in stores (and a future SQL caller that does not filter) must not accept a
        // collation match with a different spelling either.
        Some(row) if !same_character_name(name, &row.name) => ReceiverLookup::NameNotFound,
        Some(row) if row.deleted || row.delete_requested => ReceiverLookup::DeleteRequested,
        Some(row) if row.transfer_requested => ReceiverLookup::TransferRequested,
        Some(row) => ReceiverLookup::Found(BuyTarget {
            id: row.id,
            name: row.name,
            account_id: row.account_id,
            player: None,
        }),
        None => ReceiverLookup::NameNotFound,
    }
}

/// `DateTime.UtcNow`.
fn utc_now() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// `CashShopBuyTask.Execute()` for an online receiver (the C# signature).
pub async fn execute_buy(
    server: &GameServer,
    buy_mode: u8,
    buyer: &PlayerRef,
    target: &PlayerRef,
    cart: &[IcsSku],
) {
    let (id, name, account_id) = {
        let c = target.data();
        (c.id, c.unit.name.clone(), c.account_id)
    };
    let target = BuyTarget {
        id,
        name,
        account_id,
        player: Some(target.clone()),
    };
    execute_buy_for(server, buy_mode, buyer, &target, cart).await;
}

/// `CashShopBuyTask.Execute()` with the resolved receiver (online or offline).
pub async fn execute_buy_for(
    server: &GameServer,
    buy_mode: u8,
    buyer: &PlayerRef,
    target: &BuyTarget,
    cart: &[IcsSku],
) {
    let mgr = manager(server);
    let _purchase = mgr.purchases.lock().await;
    // The packet handler checked the receiver before the 1 s delay and outside this lock;
    // re-resolve it now so a deletion or transfer that started in between is refused before
    // any charge. `resolve_receiver` also re-applies the exact-name rule.
    let checked = match resolve_receiver(server, &target.name).await {
        ReceiverLookup::Found(target) => target,
        refused => {
            let error = refused
                .refusal_error()
                .unwrap_or(error_message::INGAME_SHOP_FIND_CHARACTER_NAME_FAIL);
            fail(buyer, error, buy_mode, &target.name);
            return;
        }
    };
    let target = &checked;
    let store = mgr.store();
    let (buyer_name, buyer_id, buyer_account, buyer_level, buyer_money) = {
        let c = buyer.data();
        (
            c.unit.name.clone(),
            c.id,
            c.account_id,
            c.unit.level,
            c.money,
        )
    };
    let (target_name, target_id, target_account) =
        (target.name.clone(), target.id, target.account_id);
    let is_gift = target_id != buyer_id;

    if mgr.mail.get().is_none() {
        fail(buyer, buy_fail_error(is_gift), buy_mode, &target_name);
        return;
    }
    // ---- check_costs
    // Calculate costs (of all different types in the cart)
    let mut costs = [0u32; currency::MAX as usize];
    let mut quantities = HashMap::<u32, u32>::new();
    for sku in cart {
        // AA points are not supported (owner decision 2026-11-06); an AA-priced SKU is
        // refused with the same error as before the wallet existed. This is distinct from
        // bank money2.
        if sku.currency == currency::AA_POINTS {
            fail(
                buyer,
                error_message::INGAME_SHOP_BUY_FAIL_AA_POINT,
                buy_mode,
                &target_name,
            );
            return;
        }
        if sku.item_count == 0
            || sku.item_count > i32::MAX as u32
            || sku.bonus_item_count > i32::MAX as u32
            || server.game_data.items.template(sku.item_id).is_none()
            || (sku.bonus_item_id > 0
                && sku.bonus_item_count > 0
                && server.game_data.items.template(sku.bonus_item_id).is_none())
        {
            fail(buyer, buy_fail_error(is_gift), buy_mode, &target_name);
            return;
        }
        let Some(c) = costs.get_mut(usize::from(sku.currency)) else {
            // C#: IndexOutOfRangeException ends the task.
            fail(buyer, buy_fail_error(is_gift), buy_mode, &target_name);
            return;
        };
        let Some(total) = c
            .checked_add(sku.effective_price())
            .filter(|v| *v <= i32::MAX as u32)
        else {
            fail(buyer, buy_fail_error(is_gift), buy_mode, &target_name);
            return;
        };
        *c = total;
        let quantity = quantities.entry(sku.shop_id).or_default();
        let Some(total) = quantity
            .checked_add(sku.item_count)
            .filter(|v| *v <= i32::MAX as u32)
        else {
            fail(buyer, buy_fail_error(is_gift), buy_mode, &target_name);
            return;
        };
        *quantity = total;
    }
    let before = store.account_details(server, buyer_account).await;
    // Check Credits
    if i64::from(costs[currency::CREDITS as usize]) > i64::from(before.credits) {
        fail(
            buyer,
            error_message::INGAME_SHOP_NOT_ENOUGH_AA_CASH,
            buy_mode,
            &target_name,
        );
        return;
    }
    // Check Loyalty
    if i64::from(costs[currency::LOYALTY as usize]) > i64::from(before.loyalty) {
        fail(
            buyer,
            error_message::INGAME_SHOP_NOT_ENOUGH_BM_MILEAGE,
            buy_mode,
            &target_name,
        );
        return;
    }
    // Check Copper Coins
    if i64::from(costs[currency::COINS as usize]) > buyer_money {
        fail(
            buyer,
            error_message::NOT_ENOUGH_COIN,
            buy_mode,
            &target_name,
        );
        return;
    }

    // ---- validate_cart
    let now = utc_now();
    for sku in cart {
        let shop_item = mgr.state().shop_items.get(&sku.shop_id).cloned();
        let Some(shop_item) = shop_item else {
            fail(buyer, buy_fail_error(is_gift), buy_mode, &target_name); // generic error
            return;
        };
        // Check Event Date
        if db::is_set(sku.event_end_date) && sku.event_end_date.is_some_and(|d| now >= d) {
            fail(
                buyer,
                error_message::INGAME_SHOP_EXPIRED_SELL_BY_DATE,
                buy_mode,
                &target_name,
            );
            return;
        }
        // Check Sale Start Date
        if db::is_set(shop_item.sale_start) && shop_item.sale_start.is_some_and(|d| now <= d) {
            fail(
                buyer,
                error_message::INGAME_SHOP_EXPIRED_SELL_BY_DATE,
                buy_mode,
                &target_name,
            );
            return;
        }
        // Check Sale End Date
        if db::is_set(shop_item.sale_end) && shop_item.sale_end.is_some_and(|d| now >= d) {
            fail(
                buyer,
                error_message::INGAME_SHOP_EXPIRED_SELL_BY_DATE,
                buy_mode,
                &target_name,
            );
            return;
        }
        // Check Minimum Level
        if shop_item.level_min > 0 && buyer_level < shop_item.level_min {
            fail(
                buyer,
                error_message::INGAME_SHOP_BUY_LOW_LEVEL,
                buy_mode,
                &target_name,
            );
            return;
        }
        // Check Maximum Level (the shop has no dedicated text; "level too high to use" fits).
        if shop_item.level_max > 0 && buyer_level > shop_item.level_max {
            fail(
                buyer,
                error_message::LEVEL_HIGH_TO_USE,
                buy_mode,
                &target_name,
            );
            return;
        }
        // Check Minimum Level by Restriction Type
        if shop_item.buy_restrict_type == restrict_sale_type::LEVEL
            && u32::from(buyer_level) < shop_item.buy_restrict_id
        {
            fail(
                buyer,
                error_message::INGAME_SHOP_BUY_LOW_LEVEL,
                buy_mode,
                &target_name,
            );
            return;
        }
        // Check Quest by Restriction Type
        if shop_item.buy_restrict_type == restrict_sale_type::QUEST {
            let done = mgr
                .quests
                .get()
                .is_none_or(|q| q.has_quest_completed(server, buyer, shop_item.buy_restrict_id));
            if !done {
                fail(
                    buyer,
                    error_message::INGAME_SHOP_BUY_QUEST_INCOMPLETE,
                    buy_mode,
                    &target_name,
                );
                return;
            }
        }
        // Check Remaining Stock (limited stock items)
        if shop_item.remaining >= 0 {
            let bought = i64::from(quantities[&sku.shop_id]);
            if i64::from(shop_item.remaining) < bought {
                fail(
                    buyer,
                    error_message::INGAME_SHOP_SOLD_OUT,
                    buy_mode,
                    &target_name,
                );
                return;
            }
        }
        // Validate Limited Sales
        if shop_item.limited_type != limit_type::NONE {
            let char_filter = if shop_item.limited_type == limit_type::CHARACTER {
                buyer_id
            } else {
                0
            };
            let old_sales = store
                .sales_for_shop_item(server, buyer_account, char_filter, shop_item.shop_id)
                .await;
            let mut old_count = 0u32;
            {
                let st = mgr.state();
                for old in &old_sales {
                    // Ignore if SKU no longer exists
                    let Some(old_sku) = st.sku(old.sku) else {
                        continue;
                    };
                    if shop_item.limited_type == limit_type::CHARACTER {
                        if old.buyer_char == buyer_id {
                            old_count = old_count.saturating_add(old_sku.item_count);
                        }
                    } else if shop_item.limited_type == limit_type::ACCOUNT
                        && old.buyer_account == buyer_account
                    {
                        old_count = old_count.saturating_add(old_sku.item_count);
                    }
                }
            }
            // Check if with the new amount we still stay under the limit
            if old_count.saturating_add(quantities[&sku.shop_id])
                > u32::from(shop_item.limited_stock_max)
            {
                fail(
                    buyer,
                    error_message::INGAME_SHOP_SOLD_OUT,
                    buy_mode,
                    &target_name,
                );
                return;
            }
        }
    }

    // ---- transactions
    let mut entries_sold = 0;
    for sku in cart {
        let shop_item = mgr.state().shop_items.get(&sku.shop_id).cloned();
        let Some(shop_item) = shop_item else {
            tracing::error!("Something went wrong in region transactions detecting shopItem");
            continue;
        };

        // `ItemManager.Create(id, count, FixedGrade >= 0 ? FixedGrade : 0)`; a missing template
        // makes the C# throw here (the task ends; earlier entries stay sold).
        let Some(main) = create_item(server, sku.item_id, sku.item_count) else {
            tracing::error!("Unknown item template {} in SKU {}", sku.item_id, sku.sku);
            return;
        };
        let mut items = vec![main];
        if sku.bonus_item_id > 0 && sku.bonus_item_count > 0 {
            let Some(bonus) = create_item(server, sku.bonus_item_id, sku.bonus_item_count) else {
                tracing::error!(
                    "Unknown item template {} in SKU {}",
                    sku.bonus_item_id,
                    sku.sku
                );
                return;
            };
            items.push(bonus);
        }

        // No item, stock or sale may be fulfilled when the debit fails.
        let price = sku.effective_price() as i32;
        let paid = if price == 0 {
            true
        } else {
            match sku.currency {
                currency::CREDITS => store.add_credits(server, buyer_account, -price).await,
                currency::LOYALTY => store.add_loyalty(server, buyer_account, -price).await,
                currency::COINS => {
                    let mut txn = ItemTxn::begin(server);
                    inventory::change_money(
                        &mut txn.out(),
                        buyer,
                        slot_type::INVENTORY,
                        slot_type::NONE,
                        price,
                        task_type::STORE_BUY,
                    )
                }
                _ => false,
            }
        };
        if !paid {
            for item in &items {
                server.ids.item.release_id(item.id as u32);
            }
            send_error(buyer, buy_fail_error(is_gift));
            continue;
        }

        // Reduce remaining stock if needed
        if shop_item.remaining >= 0 {
            let new_remaining = {
                let mut st = mgr.state();
                match st.shop_items.get_mut(&shop_item.shop_id) {
                    Some(si) if i64::from(si.remaining) >= i64::from(sku.item_count) => {
                        si.remaining -= sku.item_count as i32;
                        Some(si.remaining)
                    }
                    Some(si) => {
                        tracing::error!(
                            "Sale validation failed for {buyer_name}, ShopItem: {}, Sku: {}, not enough stock remaining {}",
                            si.shop_id,
                            sku.sku,
                            si.remaining
                        );
                        None
                    }
                    None => None,
                }
            };
            match new_remaining {
                Some(r) => {
                    if !store
                        .update_remaining_stock(server, shop_item.shop_id, r)
                        .await
                    {
                        if let Some(item) = mgr.state().shop_items.get_mut(&shop_item.shop_id) {
                            item.remaining = shop_item.remaining;
                        }
                        refund_checkout_price(
                            server,
                            store.as_ref(),
                            buyer,
                            buyer_account,
                            sku.currency,
                            price,
                        )
                        .await;
                        for item in &items {
                            server.ids.item.release_id(item.id as u32);
                        }
                        send_error(buyer, buy_fail_error(is_gift));
                        continue;
                    }
                    // The debit went through and the reduced count is persisted: tell the buyer's
                    // limited-stock page the committed count (`SCICSSyncGoodPacket`).
                    buyer.send(SCICSSyncGoodPacket {
                        cash_shop_id: shop_item.shop_id as i32,
                        remain_count: r,
                    });
                }
                None => {
                    refund_checkout_price(
                        server,
                        store.as_ref(),
                        buyer,
                        buyer_account,
                        sku.currency,
                        price,
                    )
                    .await;
                    for item in &items {
                        server.ids.item.release_id(item.id as u32);
                    }
                    send_error(buyer, error_message::INGAME_SHOP_SOLD_OUT);
                    continue;
                }
            }
        }

        let names = &server.game_data.systems.economy.cash_shop;
        let use_name = if !shop_item.name.trim().is_empty() {
            shop_item.name.clone()
        } else {
            names.item_name(sku.item_id)
        };
        let mail = CommercialMailRequest {
            receiver_id: target_id,
            receiver_name: target_name.clone(),
            sender_name: buyer_name.clone(),
            items,
            is_gift,
            is_refund: false,
            purchased_item_title: use_name.clone(),
        };
        let sent = match mgr.mail.get() {
            Some(m) => m.send_commercial_mail(server, mail),
            None => {
                tracing::warn!("No CommercialMail handler installed, {use_name} is not delivered");
                false
            }
        };
        if !sent {
            // The grant failed: no goods left the shop, so return the currency and the stock
            // reserved above. The debit and the grant must roll back together, as the
            // stock-update failure path already does.
            refund_checkout_price(
                server,
                store.as_ref(),
                buyer,
                buyer_account,
                sku.currency,
                price,
            )
            .await;
            if shop_item.remaining >= 0 {
                let restored = {
                    let mut st = mgr.state();
                    st.shop_items.get_mut(&shop_item.shop_id).map(|si| {
                        si.remaining += sku.item_count as i32;
                        si.remaining
                    })
                };
                if let Some(remaining) = restored {
                    if !store
                        .update_remaining_stock(server, shop_item.shop_id, remaining)
                        .await
                    {
                        tracing::error!(
                            "Cash shop could not restore stock for {} after a failed grant",
                            shop_item.shop_id
                        );
                    }
                    buyer.send(SCICSSyncGoodPacket {
                        cash_shop_id: shop_item.shop_id as i32,
                        remain_count: remaining,
                    });
                }
            }
            // The buyer paid and is the one who is told; an offline receiver has nobody to tell.
            send_error(buyer, buy_fail_error(is_gift));
            continue;
        }

        entries_sold += 1;
        tracing::info!(
            "ICSBuyGood {buyer_name} -> {target_name} - {use_name} x {}, SKU:{}",
            sku.item_count,
            sku.sku
        );
        let logged = store
            .log_sale(
                server,
                AuditIcsSale {
                    id: 0,
                    buyer_account,
                    buyer_char: buyer_id,
                    target_account,
                    target_char: target_id,
                    sale_date: Some(utc_now()),
                    shop_item_id: shop_item.shop_id,
                    sku: sku.sku,
                    sale_cost: sku.effective_price() as i32,
                    sale_currency: sku.currency,
                    description: String::new(),
                },
            )
            .await;
        if !logged {
            tracing::error!(
                "ICSBuyGood {buyer_name} -> {target_name} - {use_name} x {}, SKU:{}, save failed!",
                sku.item_count,
                sku.sku
            );
        }
    }

    let aa_points = costs[currency::AA_POINTS as usize] as i32;
    if entries_sold > 0 {
        let after = store.account_details(server, buyer_account).await;
        buyer.data().bm_point = i64::from(after.loyalty);
        buyer.send(SCICSCashPointPacket {
            point: after.credits,
        });
        buyer.send(SCBmPointPacket {
            bm_point: i64::from(after.loyalty),
        });
        buyer.send(SCICSBuyResultPacket {
            success: true,
            buy_mode,
            receiver_name: target_name,
            charge_aa_point: aa_points,
        });
    } else {
        buyer.send(SCICSBuyResultPacket {
            success: false,
            buy_mode,
            receiver_name: target_name,
            charge_aa_point: aa_points,
        });
    }
}

// Checkout still uses the existing in-memory mail delivery and independent SQL statements.
// A durable transaction/outbox is required for crash-atomic fulfillment; this compensation
// prevents known stock-update failures from consuming currency without any delivered goods.
async fn refund_checkout_price(
    server: &GameServer,
    store: &dyn CashShopDb,
    buyer: &PlayerRef,
    account: u32,
    currency: u8,
    price: i32,
) {
    if price == 0 {
        return;
    }
    let refunded = match currency {
        currency::CREDITS => store.add_credits(server, account, price).await,
        currency::LOYALTY => store.add_loyalty(server, account, price).await,
        currency::COINS => {
            let mut txn = ItemTxn::begin(server);
            inventory::add_money(
                &mut txn.out(),
                buyer,
                slot_type::INVENTORY,
                price,
                task_type::STORE_BUY,
            )
        }
        _ => false,
    };
    if !refunded {
        tracing::error!(
            account,
            price,
            currency,
            "Cash shop payment compensation failed"
        );
    }
}

/// `ItemManager.Create(templateId, count, FixedGrade >= 0 ? FixedGrade : 0, true)`.
fn create_item(server: &GameServer, template_id: u32, count: u32) -> Option<Item> {
    let data = &server.game_data.items;
    let t = data.template(template_id)?;
    let grade = if t.fixed_grade >= 0 {
        t.fixed_grade as u8
    } else {
        0
    };
    server.items.create(
        template_id,
        count as i32,
        grade,
        server.config.id,
        data,
        &server.ids.item,
        Utc::now().timestamp(),
    )
}
