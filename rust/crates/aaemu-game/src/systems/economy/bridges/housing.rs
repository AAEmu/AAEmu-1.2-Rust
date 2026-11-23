use std::collections::HashSet;

use crate::models::container::slot_type;
use crate::models::item::{Item, ItemClass};
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory;
use super::super::inventory::{Inv, ItemTxn};
use super::{lock, move_to_system_container};

// ---------------------------------------------------------------------------------------------
// housing: HousingEconomyHooks
// ---------------------------------------------------------------------------------------------

/// The housing subsystem's view of items, money and mail (`HousingEconomyHooks`).
pub struct EconomyForHousing;

fn house_item(server: &GameServer, item: &Item) -> crate::systems::housing::hooks::HouseItem {
    crate::systems::housing::hooks::HouseItem {
        item_id: item.id,
        template_id: item.template_id,
        owner_id: item.owner_id,
        count: item.count,
        max_count: server
            .game_data
            .items
            .template(item.template_id)
            .map_or(1, |t| t.max_count),
        soul_bound: item.has_flag(crate::models::item::ITEM_FLAG_SOUL_BOUND),
        secure: item.has_flag(crate::models::item::ITEM_FLAG_SECURE),
        slot_type: item.slot_type,
        ucc_id: item.ucc_id,
        big_fish: (item.class == ItemClass::BigFish)
            .then_some((item.details.fish_weight, item.details.fish_length)),
    }
}

/// A `BaseMail` from a housing mail (without attachments).
fn base_mail(m: &crate::systems::housing::hooks::HouseMail) -> super::mail::BaseMail {
    let mut mail = super::mail::BaseMail {
        mail_type: m.mail_type,
        title: m.title.clone(),
        receiver_name: m.receiver_name.clone().unwrap_or_default(),
        ..super::mail::BaseMail::default()
    };
    mail.header.receiver_id = m.receiver_id;
    mail.header.sender_id = m.sender_id;
    mail.header.sender_name = m.sender_name.clone();
    mail.header.extra = m.extra;
    mail.header.status = m.status;
    mail.body.text = m.text.clone();
    mail.body.send_date = m.send_date_ms / 1000;
    mail.body.recv_date = m.recv_date_ms / 1000;
    mail.body.copper_coins = m.copper_coins;
    mail.body.billing_amount = m.billing_amount;
    mail
}

impl crate::systems::housing::hooks::HousingEconomyHooks for EconomyForHousing {
    fn pay_house_sale_certificates(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        count: i32,
    ) -> bool {
        use crate::systems::housing::hooks::item_task;
        let certificate = crate::systems::housing::manager::appraisal_certificate(server);
        let mut txn = ItemTxn::begin(server);
        let bag = Inv::of(player).bag();
        let mut bag = lock(&bag);
        if count <= 0 || bag.items_by_template_unlocked(certificate, -1).1 < count {
            return false;
        }
        bag.consume_item(item_task::BUY_HOUSE, certificate, count, None, &mut txn.env) == count
    }

    fn pay_house_construction(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        design: crate::systems::housing::hooks::HouseItem,
        total: i32,
        tax_item: bool,
    ) -> Result<(), crate::systems::housing::hooks::HouseConstructionError> {
        if let Some(effects) =
            self.pay_house_construction_deferred(server, player, design, total, tax_item)?
        {
            super::fx::flush(server, effects);
        }
        Ok(())
    }

    fn pay_house_construction_deferred(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        design: crate::systems::housing::hooks::HouseItem,
        total: i32,
        tax_item: bool,
    ) -> Result<
        Option<crate::models::item_task::ItemFxList>,
        crate::systems::housing::hooks::HouseConstructionError,
    > {
        use crate::systems::housing::hooks::{
            construction_certificates, item_task, HouseConstructionError,
        };
        use crate::systems::housing::manager::{BOUND_TAX_CERTIFICATE, TAX_CERTIFICATE};

        // ItemTxn serializes all economy operations. Effects are flushed only after the bag
        // and character locks are released, so a quest hook cannot change the checked items.
        let mut txn = ItemTxn::begin(server);
        let inv = Inv::of(player);
        let bag = inv.bag();
        let (bound, unbound) = {
            let bag = lock(&bag);
            let Some(item) = bag.get_item_by_id(design.item_id) else {
                return Err(HouseConstructionError::InvalidDesign);
            };
            if item.owner_id != u64::from(player.character_id)
                || item.template_id != design.template_id
                || item.count < 1
                || item.slot_type != slot_type::INVENTORY
            {
                return Err(HouseConstructionError::InvalidDesign);
            }
            if item.has_flag(crate::models::item::ITEM_FLAG_SECURE) {
                return Err(HouseConstructionError::ItemLocked);
            }
            let bound = bag
                .items_by_template_unlocked(BOUND_TAX_CERTIFICATE, -1)
                .1
                .max(0);
            let unbound = bag.items_by_template_unlocked(TAX_CERTIFICATE, -1).1.max(0);
            (bound, unbound)
        };
        if total < 0 {
            return Err(HouseConstructionError::InsufficientPayment);
        }
        let certificates = construction_certificates(total);
        if tax_item {
            if i64::from(bound) + i64::from(unbound) < i64::from(certificates) {
                return Err(HouseConstructionError::InsufficientPayment);
            }
        } else {
            if player.data().money < i64::from(total) {
                return Err(HouseConstructionError::InsufficientPayment);
            }
            if !inventory::subtract_money(
                &mut txn.out(),
                player,
                slot_type::INVENTORY,
                total,
                item_task::HOUSE_CREATION,
            ) {
                return Err(HouseConstructionError::InsufficientPayment);
            }
        }
        let mut bag = lock(&bag);
        if tax_item {
            let from_bound = bound.min(certificates);
            if from_bound > 0 {
                bag.consume_item(
                    item_task::HOUSE_CREATION,
                    BOUND_TAX_CERTIFICATE,
                    from_bound,
                    None,
                    &mut txn.env,
                );
            }
            let remainder = certificates - from_bound;
            if remainder > 0 {
                bag.consume_item(
                    item_task::HOUSE_CREATION,
                    TAX_CERTIFICATE,
                    remainder,
                    None,
                    &mut txn.env,
                );
            }
        }
        bag.consume_item(
            item_task::HOUSE_BUILDING,
            design.template_id,
            1,
            Some(design.item_id),
            &mut txn.env,
        );
        drop(bag);
        Ok(Some(txn.into_fx()))
    }

    fn inventory_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        item_id: u64,
    ) -> Option<crate::systems::housing::hooks::HouseItem> {
        let c = Inv::of(player).holder_of_any(item_id)?;
        let item = lock(&c).get_item_by_id(item_id).cloned()?;
        Some(house_item(server, &item))
    }

    fn item(
        &self,
        server: &GameServer,
        item_id: u64,
    ) -> Option<crate::systems::housing::hooks::HouseItem> {
        server
            .items
            .item_by_id(item_id)
            .map(|i| house_item(server, &i))
    }

    fn inventory_item_count(
        &self,
        _server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
    ) -> i32 {
        Inv::of(player).items_count_in_unlocked(slot_type::INVENTORY, template_id, -1)
    }

    fn bag_consume_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        template_id: u32,
        count: i32,
        preferred_item_id: Option<u64>,
    ) -> i32 {
        let mut txn = ItemTxn::begin(server);
        let result = lock(&Inv::of(player).bag()).consume_item(
            task,
            template_id,
            count,
            preferred_item_id,
            &mut txn.env,
        );
        result
    }

    fn subtract_money(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        amount: i32,
        task: u8,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        inventory::subtract_money(&mut txn.out(), player, slot_type::INVENTORY, amount, task)
    }

    fn move_item_to_system_container(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        item_id: u64,
    ) -> bool {
        move_to_system_container(server, player, task, item_id)
    }

    fn destroy_item(&self, server: &GameServer, item_id: u64) {
        let Some(c) = server.items.holding_container(item_id) else {
            return;
        };
        let mut txn = ItemTxn::begin(server);
        let mut g = lock(&c);
        let Some((template_id, count)) =
            g.get_item_by_id(item_id).map(|i| (i.template_id, i.count))
        else {
            return;
        };
        g.consume_item(
            task_type::INVALID,
            template_id,
            count,
            Some(item_id),
            &mut txn.env,
        );
    }

    fn take_coffer_items(
        &self,
        server: &GameServer,
        owner_id: u32,
        container_id: u64,
    ) -> Vec<crate::systems::housing::hooks::HouseItem> {
        let Some(coffer) = server.items.container_by_id(container_id) else {
            return Vec::new();
        };
        let system = server.items.container_for_character(
            owner_id,
            slot_type::SYSTEM,
            0,
            &server.ids.container,
        );
        let mut txn = ItemTxn::begin(server);
        let data = txn.data();
        let mut ids: Vec<(i32, u64)> = lock(&coffer).items.iter().map(|i| (i.slot, i.id)).collect();
        ids.sort_by_key(|(slot, _)| std::cmp::Reverse(*slot));
        let mut out = Vec::new();
        for (_, id) in ids {
            let (mut sys, src) = inventory::lock_pair(&system, &coffer);
            let Some(mut src) = src else { break };
            if sys.move_item_from(&mut src, task_type::INVALID, id, -1, data, &mut txn.env.fx) {
                if let Some(item) = sys.get_item_by_id(id) {
                    out.push(house_item(server, item));
                }
            }
        }
        out
    }

    fn send_mail_with_new_items(
        &self,
        server: &GameServer,
        owner: &PlayerRef,
        mail: crate::systems::housing::hooks::HouseMail,
        template_id: u32,
        count: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let Some(c) = super::mail::mail_container(owner) else {
            return false;
        };
        let res = lock(&c).acquire_default_item_ex(
            task_type::INVALID,
            template_id,
            count,
            -1,
            0,
            -1,
            &mut txn.env,
        );
        let Some(res) = res.filter(|r| r.has_tasks) else {
            return false;
        };
        let mut m = base_mail(&mail);
        m.body.attachments.extend(res.new);
        super::mail::send(&mut txn, m);
        true
    }

    fn send_mail(
        &self,
        server: &GameServer,
        mail: crate::systems::housing::hooks::HouseMail,
    ) -> bool {
        use crate::systems::housing::hooks::MailAttachment;
        let mut txn = ItemTxn::begin(server);
        let mut m = base_mail(&mail);
        for a in &mail.attachments {
            match *a {
                MailAttachment::NewItem {
                    template_id,
                    count,
                    grade,
                    owner_id,
                } => {
                    let Some(mut item) = txn.env.create(template_id, count, 0) else {
                        continue;
                    };
                    item.grade = grade;
                    item.owner_id = u64::from(owner_id);
                    item.slot_type = slot_type::MAIL;
                    m.body.attachments.push(item.id);
                    server.items.add_loose(item);
                }
                MailAttachment::Existing {
                    item_id,
                    owner_id,
                    add_count,
                } => {
                    if add_count != 0 {
                        super::mail::mutate_item(server, item_id, |i| {
                            i.count += add_count;
                            i.dirty = true;
                        });
                    }
                    let target = server
                        .world
                        .character_by_id(owner_id)
                        .and_then(|p| super::mail::mail_container(&p));
                    match target {
                        Some(c) => {
                            super::mail::add_or_move_existing_item(
                                &mut txn,
                                &c,
                                task_type::INVALID,
                                item_id,
                                -1,
                            );
                        }
                        None => {
                            super::mail::mutate_item(server, item_id, |i| {
                                i.slot_type = slot_type::MAIL;
                                i.dirty = true;
                            });
                        }
                    }
                    m.body.attachments.push(item_id);
                }
            }
        }
        super::mail::send(&mut txn, m)
    }

    fn coffer_items(
        &self,
        server: &GameServer,
        container_id: u64,
    ) -> Vec<crate::systems::housing::hooks::HouseItem> {
        let Some(container) = server.items.container_by_id(container_id) else {
            return Vec::new();
        };
        let mut items = lock(&container).items.clone();
        items.sort_by_key(|item| std::cmp::Reverse(item.slot));
        items.iter().map(|item| house_item(server, item)).collect()
    }

    fn send_return_mails(
        &self,
        server: &GameServer,
        mails: Vec<crate::systems::housing::hooks::HouseMail>,
        payment: Option<(&PlayerRef, i32)>,
    ) -> bool {
        use crate::systems::housing::hooks::{item_task, MailAttachment};
        let mut txn = ItemTxn::begin(server);
        let mut prepared = Vec::new();
        let mut new_items = Vec::new();
        let mut existing = Vec::new();
        let mut reserved_mails = Vec::new();
        let mut seen_existing = HashSet::new();
        let staged: Result<(), ()> = (|| {
            if let Some((buyer, amount)) = payment {
                if amount < 0 || buyer.data().money < i64::from(amount) {
                    return Err(());
                }
            }
            for housing_mail in &mails {
                let mut mail = base_mail(housing_mail);
                let name = server
                    .names
                    .get_character_name(mail.header.receiver_id)
                    .ok_or(())?;
                if !name.eq_ignore_ascii_case(&mail.receiver_name)
                    || server.names.get_character_id(&mail.receiver_name) != mail.header.receiver_id
                    || housing_mail.attachments.len() > super::mail::MAX_MAIL_ATTACHMENTS
                {
                    return Err(());
                }
                let mail_id = server.ids.mail.get_next_id().map_err(|_| ())?;
                reserved_mails.push(mail_id);
                mail.id = i64::from(mail_id);
                for attachment in &housing_mail.attachments {
                    match *attachment {
                        MailAttachment::NewItem {
                            template_id,
                            count,
                            grade,
                            owner_id,
                        } => {
                            if count <= 0 || owner_id != mail.header.receiver_id {
                                return Err(());
                            }
                            // Reserve without ItemManager::new_id: an aborted batch must
                            // keep pending deletions for a previously used item ID intact.
                            let mut item = server
                                .items
                                .create_detached(
                                    template_id,
                                    count,
                                    0,
                                    txn.env.world_id,
                                    txn.env.data,
                                    txn.env.now,
                                )
                                .ok_or(())?;
                            item.id = u64::from(server.ids.item.get_next_id().map_err(|_| ())?);
                            item.grade = grade;
                            item.owner_id = u64::from(owner_id);
                            item.slot_type = slot_type::MAIL;
                            item.slot = mail.body.attachments.len() as i32;
                            mail.body.attachments.push(item.id);
                            new_items.push(item);
                        }
                        MailAttachment::Existing {
                            item_id,
                            owner_id,
                            add_count,
                        } => {
                            let item = server.items.item_by_id(item_id).ok_or(())?;
                            if owner_id != mail.header.receiver_id
                                || add_count < 0
                                || item.count.checked_add(add_count).is_none()
                                || !seen_existing.insert(item_id)
                            {
                                return Err(());
                            }
                            existing.push((
                                item_id,
                                owner_id,
                                add_count,
                                mail.body.attachments.len() as i32,
                            ));
                            mail.body.attachments.push(item_id);
                        }
                    }
                }
                mail.header.attachments = mail.get_total_attachment_count();
                prepared.push((mail, name));
            }
            Ok(())
        })();
        if staged.is_err() {
            for item in new_items {
                server.ids.item.release_id(item.id as u32);
            }
            for id in reserved_mails {
                server.ids.mail.release_id(id);
            }
            return false;
        }
        let mut state = server.systems.economy.mail.state();
        if prepared
            .iter()
            .any(|(mail, _)| state.all.contains_key(mail.id))
        {
            for item in new_items {
                server.ids.item.release_id(item.id as u32);
            }
            for id in reserved_mails {
                server.ids.mail.release_id(id);
            }
            return false;
        }
        // Every fallible check has completed while ItemTxn excludes other economy changes.
        if let Some((buyer, amount)) = payment {
            if !inventory::subtract_money(
                &mut txn.out(),
                buyer,
                slot_type::INVENTORY,
                amount,
                item_task::BUY_HOUSE,
            ) {
                for item in new_items {
                    server.ids.item.release_id(item.id as u32);
                }
                for id in reserved_mails {
                    server.ids.mail.release_id(id);
                }
                return false;
            }
        }
        for (id, owner, add_count, slot) in existing {
            let mut item = match server.items.holding_container(id) {
                Some(source) => lock(&source).remove_item(id),
                None => server.items.take_loose(id),
            }
            .expect("attachment was checked under the economy operation lock");
            item.owner_id = u64::from(owner);
            item.count += add_count;
            item.slot_type = slot_type::MAIL;
            item.slot = slot;
            item.dirty = true;
            server.items.add_loose(item);
        }
        for item in new_items {
            server.items.add_loose(item);
        }
        for (mail, name) in prepared {
            let id = mail.id;
            state.clear_deletion(id);
            let added = state.all.add(id, mail);
            debug_assert!(added, "mail IDs were reserved and checked before commit");
            if let Some(mail) = state.all.get_mut(id) {
                super::mail::notify_new_mail_by_name_if_online(
                    server,
                    &mut txn.env.fx,
                    mail,
                    &name,
                );
            }
        }
        true
    }

    fn delete_house_mails(&self, server: &GameServer, house_id: u32) {
        let mut txn = ItemTxn::begin(server);
        super::mail::delete_house_mails(&mut txn, house_id);
    }

    fn house_mail_ids(&self, server: &GameServer, house_id: u32) -> Vec<i64> {
        server.systems.economy.mail.get_my_house_mails(house_id)
    }

    fn update_tax_mail(
        &self,
        server: &GameServer,
        mail_id: i64,
        update: &crate::systems::housing::hooks::TaxMailUpdate,
    ) {
        server.systems.economy.mail.with_mail_mut(mail_id, |m| {
            if let Some((id, name)) = &update.receiver {
                m.header.receiver_id = *id;
                m.receiver_name = name.clone();
            }
            if let Some(t) = &update.title {
                m.title = t.clone();
            }
            if let Some((text, billing, extra)) = &update.body {
                m.body.text = text.clone();
                m.body.billing_amount = *billing;
                m.header.extra = *extra;
                m.header.status = crate::systems::housing::hooks::MAIL_STATUS_UNPAID;
            }
            m.is_dirty = true;
        });
    }
}
