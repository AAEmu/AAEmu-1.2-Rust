use crate::models::container::{equip_slot, slot_type};
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory::{Inv, ItemTxn};
use super::lock;

// ---------------------------------------------------------------------------------------------
// services: SheetMusicInventory / CharacterItemOps / WebApi / buy-back wipe
// ---------------------------------------------------------------------------------------------

/// The inventory side of `MusicManager.CreateSheetMusic`.
pub struct EconomySheetMusic<'a> {
    pub server: &'a GameServer,
}

impl crate::systems::services::music::SheetMusicInventory for EconomySheetMusic<'_> {
    fn source_item(&self, player: &PlayerRef, item_id: u64) -> Option<(u32, u32)> {
        let c = Inv::of(player).holder_of_any(item_id)?;
        let g = lock(&c);
        let owner = g.owner_id;
        g.get_item_by_id(item_id).map(|i| (owner, i.template_id))
    }

    fn bag_free_slots(&self, player: &PlayerRef) -> i32 {
        lock(&Inv::of(player).bag()).free_slot_count()
    }

    fn add_sheet_music(&self, player: &PlayerRef, song_id: u32) -> bool {
        let server = self.server;
        let mut txn = ItemTxn::begin(server);
        let Some(mut sheet) = txn.env.create(crate::data::items::ITEM_SHEET_MUSIC, 1, 0) else {
            return false;
        };
        sheet.owner_id = u64::from(player.character_id);
        sheet.made_unit_id = player.character_id;
        sheet.crafter_id = player.character_id;
        sheet.details.song_id = song_id;
        let data = txn.data();
        let bag = Inv::of(player).bag();
        let res = lock(&bag).add_item(
            task_type::SAVE_MUSIC_NOTES,
            sheet,
            -1,
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

    fn consume_source(&self, player: &PlayerRef, item_id: u64, template_id: u32) -> i32 {
        let mut txn = ItemTxn::begin(self.server);
        let result = lock(&Inv::of(player).bag()).consume_item(
            task_type::SAVE_MUSIC_NOTES,
            template_id,
            1,
            Some(item_id),
            &mut txn.env,
        );
        result
    }
}

/// The services subsystem's inventory operations (`CHARACTER_ITEM_OPS`) and WebApi economy.
pub struct EconomyForServices;

impl crate::systems::services::item_ops::CharacterItemOps for EconomyForServices {
    fn items_count(&self, _server: &GameServer, player: &PlayerRef, template_id: u32) -> i32 {
        Inv::of(player).items_count_in_unlocked(slot_type::INVENTORY, template_id, -1)
    }

    fn consume_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        template_id: u32,
        count: i32,
    ) -> i32 {
        let mut txn = ItemTxn::begin(server);
        let result =
            lock(&Inv::of(player).bag()).consume_item(task, template_id, count, None, &mut txn.env);
        result
    }

    fn replace_hair(&self, server: &GameServer, player: &PlayerRef, hair_model: u32) -> bool {
        let mut txn = ItemTxn::begin(server);
        let equipment = Inv::of(player).equipment();
        let mut g = lock(&equipment);
        // Remove old hair item
        if let Some(old) = g.get_item_by_slot(equip_slot::HAIR).map(|i| i.id) {
            g.remove_item_task(task_type::INVALID, old, true, &mut txn.env);
        }
        // Create new hair item
        let res = g.acquire_default_item_ex(
            task_type::INVALID,
            hair_model,
            1,
            -1,
            player.character_id,
            equip_slot::HAIR,
            &mut txn.env,
        );
        let name = player.data().unit.name.clone();
        match res {
            Some(r) if r.has_tasks => {
                if r.new.len() != 1 {
                    tracing::error!(
                        "Something failed during hairstyle creation for player {name} ({})!",
                        player.character_id
                    );
                }
                true
            }
            _ => false,
        }
    }
}

fn naive(t: crate::models::item::UnixTime) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp(t, 0)
        .unwrap_or_default()
        .naive_utc()
}

fn web_mail_info(
    server: &GameServer,
    m: &super::mail::BaseMail,
) -> crate::systems::services::webapi::hooks::WebApiMailInfo {
    use crate::systems::services::webapi::json::DateKind;
    let _ = server;
    crate::systems::services::webapi::hooks::WebApiMailInfo {
        id: m.id,
        sender_id: m.header.sender_id,
        sender_name: m.header.sender_name.clone(),
        receiver_id: m.header.receiver_id,
        receiver_name: m.receiver_name.clone(),
        is_delivered: m.is_delivered,
        title: m.title.clone(),
        mail_type: m.mail_type,
        send_date: (naive(m.body.send_date), DateKind::Unspecified),
        receive_date: (naive(m.body.recv_date), DateKind::Unspecified),
        open_date: (naive(m.open_date), DateKind::Unspecified),
        attachment_count: i32::from(m.get_total_attachment_count()),
        copper_coins: i64::from(m.body.copper_coins),
    }
}

/// The Newtonsoft form of an `AuctionLot` (the `Item` member is reduced to its stored fields).
fn auction_lot_json(
    lot: &super::auction::AuctionLot,
) -> crate::systems::services::webapi::json::JValue {
    use crate::systems::services::webapi::json::{DateKind, JValue};
    let item = match &lot.item {
        Some(i) => JValue::Object(vec![
            ("Id".into(), JValue::UInt(i.id)),
            ("TemplateId".into(), JValue::UInt(u64::from(i.template_id))),
            ("Count".into(), JValue::Int(i64::from(i.count))),
            ("Grade".into(), JValue::UInt(u64::from(i.grade))),
            ("OwnerId".into(), JValue::UInt(i.owner_id)),
            ("SlotType".into(), JValue::UInt(u64::from(i.slot_type))),
            ("Slot".into(), JValue::Int(i64::from(i.slot))),
        ]),
        None => JValue::Null,
    };
    let now = chrono::Utc::now();
    JValue::Object(vec![
        ("Id".into(), JValue::UInt(lot.id)),
        ("Duration".into(), JValue::UInt(u64::from(lot.duration))),
        ("Item".into(), item),
        (
            "EndTime".into(),
            JValue::Date(lot.end_time.naive_utc(), DateKind::Utc),
        ),
        ("TimeLeft".into(), JValue::UInt(lot.time_left(now))),
        ("WorldId".into(), JValue::UInt(u64::from(lot.world_id))),
        ("ClientId".into(), JValue::UInt(u64::from(lot.client_id))),
        ("ClientName".into(), JValue::Str(lot.client_name.clone())),
        ("StartMoney".into(), JValue::Int(i64::from(lot.start_money))),
        (
            "DirectMoney".into(),
            JValue::Int(i64::from(lot.direct_money)),
        ),
        (
            "PostDate".into(),
            JValue::Date(lot.post_date.naive_utc(), DateKind::Utc),
        ),
        (
            "BidWorldId".into(),
            JValue::UInt(u64::from(lot.bid_world_id)),
        ),
        ("BidderId".into(), JValue::UInt(u64::from(lot.bidder_id))),
        ("BidderName".into(), JValue::Str(lot.bidder_name.clone())),
        ("BidMoney".into(), JValue::Int(i64::from(lot.bid_money))),
        ("Extra".into(), JValue::Int(i64::from(lot.extra))),
        ("IsDirty".into(), JValue::Bool(lot.dirty)),
    ])
}

impl crate::systems::services::webapi::hooks::WebApiEconomyHooks for EconomyForServices {
    fn send_mail(
        &self,
        server: &GameServer,
        mail: crate::systems::services::webapi::hooks::WebApiMail,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let mut m = super::mail::BaseMail {
            mail_type: mail.mail_type,
            title: mail.title,
            receiver_name: mail.receiver_name,
            ..super::mail::BaseMail::default()
        };
        m.header.sender_id = mail.sender_id;
        m.header.sender_name = mail.sender_name;
        m.header.receiver_id = mail.receiver_id;
        m.header.extra = mail.extra;
        m.body.text = mail.text;
        m.body.send_date = mail.send_date.and_utc().timestamp();
        m.body.recv_date = mail.recv_date.and_utc().timestamp();
        m.body.copper_coins = mail.copper_coins;
        m.body.billing_amount = mail.billing_amount;
        for item in mail.attachments {
            m.body.attachments.push(item.id);
            server.items.add_loose(item);
        }
        super::mail::send(&mut txn, m)
    }

    fn current_mail_list(
        &self,
        server: &GameServer,
        character_id: u32,
    ) -> Vec<crate::systems::services::webapi::hooks::WebApiMailInfo> {
        let ids = super::mail::get_current_mail_list_for(server, character_id);
        ids.into_iter()
            .filter_map(|id| server.systems.economy.mail.get_mail_by_id(id))
            .map(|m| web_mail_info(server, &m))
            .collect()
    }

    fn mail_by_id(
        &self,
        server: &GameServer,
        mail_id: i64,
    ) -> Option<crate::systems::services::webapi::hooks::WebApiMailInfo> {
        server
            .systems
            .economy
            .mail
            .get_mail_by_id(mail_id)
            .map(|m| web_mail_info(server, &m))
    }

    fn delete_mail(&self, server: &GameServer, mail_id: i64, trash_items: bool) -> bool {
        let mut txn = ItemTxn::begin(server);
        super::mail::delete_mail_trash(&mut txn, mail_id, trash_items)
    }

    fn auction_lots(
        &self,
        server: &GameServer,
    ) -> anyhow::Result<Vec<crate::systems::services::webapi::hooks::AuctionLotView>> {
        Ok(server
            .systems
            .economy
            .auction
            .all_lots()
            .iter()
            .map(
                |lot| crate::systems::services::webapi::hooks::AuctionLotView {
                    json: auction_lot_json(lot),
                    template_id: lot.item.as_ref().map_or(0, |i| i.template_id),
                    client_name: lot.client_name.clone(),
                    stack_size: lot.item.as_ref().map_or(0, |i| i.count.max(0) as u32),
                    direct_money: lot.direct_money,
                    bid_money: lot.bid_money,
                    bidder_name: Some(lot.bidder_name.clone()),
                },
            )
            .collect())
    }

    fn create_auction_lot(
        &self,
        server: &GameServer,
        lot: crate::systems::services::webapi::hooks::NewAuctionLot,
    ) -> anyhow::Result<crate::systems::services::webapi::json::JValue> {
        let new = super::auction::AuctionManager::create_auction_lot(
            server,
            lot.client_id,
            &lot.client_name,
            lot.item,
            lot.start_price,
            lot.buyout_price,
            lot.duration,
            chrono::Utc::now(),
        )
        .ok_or_else(|| anyhow::anyhow!("CreateAuctionLot failed"))?;
        let json = auction_lot_json(&new);
        server.systems.economy.auction.add_auction_lot(new);
        Ok(json)
    }

    fn acquire_auction_items(
        &self,
        server: &GameServer,
        character_id: u32,
        template_id: u32,
        quantity: i32,
        grade: u8,
    ) -> anyhow::Result<Option<Vec<Item>>> {
        let mut txn = ItemTxn::begin(server);
        let c = server.items.container_for_character(
            character_id,
            slot_type::AUCTION,
            0,
            &server.ids.container,
        );
        let res = lock(&c).acquire_default_item_ex(
            task_type::INVALID,
            template_id,
            quantity,
            i32::from(grade),
            0,
            -1,
            &mut txn.env,
        );
        let Some(res) = res.filter(|r| r.has_tasks) else {
            return Ok(None);
        };
        let g = lock(&c);
        Ok(Some(
            res.new
                .iter()
                .filter_map(|id| g.get_item_by_id(*id).cloned())
                .collect(),
        ))
    }
}
