use crate::data::Feature;
use crate::models::container::slot_type;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory::{Inv, ItemTxn};
use super::super::mail_packets::{SCChargeMoneyPaidPacket, SCMailDeletedPacket};
use super::super::{error_message, inventory};
use super::{
    delete_mail_in, extract_extra_for_house, mail_status, mail_type,
    notify_delete_mail_by_name_if_online, now, ITEM_BOUND_TAX_CERTIFICATE, ITEM_TAX_CERTIFICATE,
};

/// Production tax-payment edge. Capture the current property/territory before ItemTxn and
/// serialize sales, captures and rate changes before the authority -> economy debit.
pub fn pay_housing_tax(
    server: &GameServer,
    character: &PlayerRef,
    mail_id: i64,
    auto_use_aa_point: bool,
) -> bool {
    let _items = server.systems.economy.operation_scope();
    let operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(mail) = server.systems.economy.mail.get_mail_by_id(mail_id) else {
        crate::systems::social::util::send_error(character, error_message::MAIL_INVALID);
        return false;
    };
    if mail.mail_type != mail_type::BILLING
        || mail.header.sender_id != 0
        || mail.header.sender_name != ".houseTax"
        || mail.header.receiver_id != character.character_id
        || mail.header.status != mail_status::UNPAID
        || mail.body.billing_amount < 0
        || mail.body.recv_date > now()
    {
        crate::systems::social::util::send_error(character, error_message::MAIL_INVALID);
        return false;
    }
    let (_, house_id) = extract_extra_for_house(mail.header.extra);
    let Some(house) = server.systems.housing.manager.by_id(house_id) else {
        crate::systems::social::util::send_error(character, error_message::INVALID_HOUSE_INFO);
        return false;
    };
    if !crate::systems::housing::dominion::runtime::is_live_object(
        server,
        &crate::world::WorldObject::from(&house),
    ) || {
        let house = house.data();
        house.unit.hp <= 0 || house.unit.owner_id != character.character_id
    } {
        crate::systems::social::util::send_error(character, error_message::INVALID_HOUSE_INFO);
        return false;
    }
    let Ok(quote) = crate::systems::housing::dominion::tax::quote_house(server, &house) else {
        crate::systems::social::util::send_error(character, error_message::INVALID_TAXATION);
        return false;
    };
    if mail.body.billing_amount != quote.info.total_tax_to_pay {
        crate::systems::housing::manager::update_tax_info(server, &house);
        crate::systems::social::util::send_error(character, error_message::INVALID_TAXATION);
        return false;
    }
    let mut effects = None;
    let result = crate::systems::housing::dominion::tax::charge_with(server, &quote, || {
        let mut txn = ItemTxn::begin(server);
        let paid = pay_charge_money(&mut txn, character, mail_id, auto_use_aa_point);
        effects = Some(txn.into_fx());
        paid
    });
    drop(operation);
    if let Some(effects) = effects {
        super::fx::flush(server, effects);
    }
    match result {
        Ok(state) => {
            if let Some(state) = state {
                crate::systems::social::dominion::broadcast_snapshot(server, &state, false);
            }
            true
        }
        Err(crate::systems::housing::dominion::DominionError::Resources) => false,
        Err(_) => {
            crate::systems::social::util::send_error(character, error_message::INVALID_TAXATION);
            false
        }
    }
}

/// `PayChargeMoney(character, mailId, autoUseAAPoint)` debit primitive. Production callers
/// use pay_housing_tax so territory context is captured before the economy lock is acquired.
pub fn pay_charge_money(
    txn: &mut ItemTxn<'_>,
    character: &PlayerRef,
    mail_id: i64,
    _auto_use_aa_point: bool,
) -> bool {
    let server = txn.server;
    let mails = &server.systems.economy.mail;
    let Some(mail) = mails.get_mail_by_id(mail_id) else {
        inventory::send_error_message_to(&mut txn.out(), character, error_message::MAIL_INVALID);
        return false;
    };
    // Only an unpaid, delivered tax bill addressed to this character may be paid.
    if mail.mail_type != mail_type::BILLING
        || mail.header.sender_id != 0
        || mail.header.sender_name != ".houseTax"
        || mail.header.receiver_id != character.character_id
        || mail.header.status != mail_status::UNPAID
        || mail.body.billing_amount < 0
        || mail.body.recv_date > now()
    {
        inventory::send_error_message_to(&mut txn.out(), character, error_message::MAIL_INVALID);
        return false;
    }
    let (_zone_group, house_id) = extract_extra_for_house(mail.header.extra);
    let housing = mails.hooks.housing.get();
    let house_exists = housing
        .as_ref()
        .is_some_and(|h| h.house_exists(server, house_id));
    if !house_exists
        || housing
            .as_ref()
            .and_then(|h| h.house_owner_id(server, house_id))
            != Some(character.character_id)
    {
        inventory::send_error_message_to(
            &mut txn.out(),
            character,
            error_message::INVALID_HOUSE_INFO,
        );
        return false;
    }

    let tax_items = server.game_data.features.check(Feature::TaxItem);
    let required_certs = ((i64::from(mail.body.billing_amount) + 9999) / 10000) as i32;
    let user_bound_tax_count = if tax_items {
        let inv = Inv::of(character);
        let user_tax_count =
            inv.items_count_in_unlocked(slot_type::INVENTORY, ITEM_TAX_CERTIFICATE, -1);
        let user_bound_tax_count =
            inv.items_count_in_unlocked(slot_type::INVENTORY, ITEM_BOUND_TAX_CERTIFICATE, -1);
        let total = i64::from(user_tax_count) + i64::from(user_bound_tax_count);
        if total < i64::from(required_certs) {
            // Not enough certs
            inventory::send_error_message_to(
                &mut txn.out(),
                character,
                error_message::MAIL_NOT_ENOUGH_MONEY_TO_PAY_TAXES,
            );
            return false;
        }
        user_bound_tax_count
    } else {
        if i64::from(mail.body.billing_amount) > character.data().money {
            inventory::send_error_message_to(
                &mut txn.out(),
                character,
                error_message::MAIL_NOT_ENOUGH_MONEY_TO_PAY_TAXES,
            );
            return false;
        }
        0
    };

    // Keep the bill stable through the payment and removal. ItemTxn already prevents
    // concurrent inventory changes, so the validated cost can be consumed after housing
    // accepts the payment. A rejected/expired house never charges the player.
    let mut st = mails.state();
    let current = st.all.get(mail_id);
    if current.is_none_or(|m| {
        m.header.receiver_id != mail.header.receiver_id
            || m.header.status != mail_status::UNPAID
            || m.header.extra != mail.header.extra
            || m.body.billing_amount != mail.body.billing_amount
    }) {
        inventory::send_error_message_to(&mut txn.out(), character, error_message::MAIL_INVALID);
        return false;
    }
    if !housing.is_some_and(|h| h.pay_weekly_tax_for(server, house_id, character.character_id)) {
        inventory::send_error_message_to(
            &mut txn.out(),
            character,
            error_message::INVALID_HOUSE_INFO,
        );
        return false;
    }

    if tax_items {
        let inv = Inv::of(character);
        let bag = inv.bag();
        // Use Bound First
        let mut consumed_certs = required_certs;
        let mut c = consumed_certs;
        if user_bound_tax_count > 0 && c > 0 {
            if c > user_bound_tax_count {
                c = user_bound_tax_count;
            }
            consumed_certs -= bag.lock().expect("container poisoned").consume_item(
                task_type::MAIL,
                ITEM_BOUND_TAX_CERTIFICATE,
                c,
                None,
                &mut txn.env,
            );
        }
        c = consumed_certs;
        if c > 0 {
            consumed_certs -= bag.lock().expect("container poisoned").consume_item(
                task_type::MAIL,
                ITEM_TAX_CERTIFICATE,
                c,
                None,
                &mut txn.env,
            );
        }
        if consumed_certs != 0 {
            tracing::error!(
                "Something went wrong when paying tax for mailId {}",
                mail.id
            );
        }
    } else {
        // use gold as payment
        inventory::subtract_money(
            &mut txn.out(),
            character,
            slot_type::INVENTORY,
            mail.body.billing_amount,
            task_type::DEPOSIT_MONEY,
        );
    }

    {
        let mail_type = mail.mail_type;
        if let Some(m) = st.all.get_mut(mail_id) {
            if m.header.status != mail_status::READ {
                m.header.status = mail_status::READ;
                character.data().unread_mail.update_received(mail_type, -1);
            }
        }
        let count = character.data().unread_mail;
        let cid = character.character_id;
        txn.env.fx.send(cid, SCChargeMoneyPaidPacket { mail_id });
        txn.env.fx.send(
            cid,
            SCMailDeletedPacket {
                is_sent: false,
                mail_id,
                is_unread_mail_count_modified: false,
                count,
            },
        );
        delete_mail_in(server, &mut st, mail_id);
        drop(st);
        super::mail_character::send_unread_mail_count(&mut txn.env.fx, character);
    }
    delete_house_mails(txn, house_id);
    true
}

/// `DeleteHouseMails(houseId)`.
pub fn delete_house_mails(txn: &mut ItemTxn<'_>, house_id: u32) {
    let server = txn.server;
    let mails = &server.systems.economy.mail;
    // Check which mails to remove
    let delete_list = mails.get_my_house_mails(house_id);
    // Actually remove them by Id
    let mut st = mails.state();
    for d in delete_list {
        let Some(mail) = st.all.get(d).cloned() else {
            continue;
        };
        notify_delete_mail_by_name_if_online(server, &mut txn.env.fx, &mail, &mail.receiver_name);
        delete_mail_in(server, &mut st, d);
    }
}
