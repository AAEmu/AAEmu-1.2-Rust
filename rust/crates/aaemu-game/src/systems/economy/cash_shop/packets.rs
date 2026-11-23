use std::time::Duration;

use aaemu_commons::{Marshal, PacketStream};
use futures::future::BoxFuture;

use crate::db;
use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::server::GameServer;

use super::super::error_message;
use super::super::item_handlers::{active, sync_client_packet};
use super::checkout::fail;
use super::{
    cmd_ui_type, currency, execute_buy_for, manager, resolve_receiver, send_ics_page, BuyTarget,
    IcsItem, IcsMenu, IcsSku, ReceiverLookup,
};

// ---------------------------------------------------------------------------------------------
// S->C packets
// ---------------------------------------------------------------------------------------------

/// `SCICSMenuListPacket(enabled)` (tab flags computed from `MenuItems` at construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSMenuListPacket {
    pub enabled: bool,
    /// `[mainTab - 1][subTab - 1]` used flags.
    pub tabs: [[bool; 7]; 6],
}
packet_def!(
    SCICSMenuListPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSMenuListPacket
);

impl SCICSMenuListPacket {
    pub const MAIN_TAB_COUNT: u8 = 6;
    pub const SUB_TAB_COUNT: u8 = 7;

    /// `new SCICSMenuListPacket(enabled)`; `None` where the C# constructor throws
    /// (a menu entry outside the 6x7 tab grid: KeyNotFoundException).
    pub fn new(enabled: bool, menu: &[IcsMenu]) -> Option<Self> {
        let mut tabs = [[false; 7]; 6];
        for m in menu {
            if !(1..=Self::MAIN_TAB_COUNT).contains(&m.main_tab)
                || !(1..=Self::SUB_TAB_COUNT).contains(&m.sub_tab)
            {
                return None;
            }
            tabs[usize::from(m.main_tab - 1)][usize::from(m.sub_tab - 1)] = true;
        }
        Some(Self { enabled, tabs })
    }
}

impl Marshal for SCICSMenuListPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.enabled);
        for main_tab in 1..=Self::MAIN_TAB_COUNT {
            s.write_u8(main_tab);
            for sub_tab in 1..=Self::SUB_TAB_COUNT {
                let used = self.tabs[usize::from(main_tab - 1)][usize::from(sub_tab - 1)];
                s.write_u8(if used { sub_tab } else { 0 });
            }
        }
    }
}

/// `SCICSGoodListPacket(pageEnd, totalPage, mainTab, subTab, item)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSGoodListPacket {
    pub page_end: bool,
    pub total_page: u16,
    pub main_tab: u8,
    pub sub_tab: u8,
    pub item: IcsItem,
}
packet_def!(
    SCICSGoodListPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSGoodListPacket
);
impl Marshal for SCICSGoodListPacket {
    fn write(&self, s: &mut PacketStream) {
        let item = &self.item;
        s.write_bool(self.page_end)
            .write_u16(self.total_page)
            .write_u32(item.shop_id)
            .write_str(&item.name)
            .write_u8(self.main_tab)
            .write_u8(self.sub_tab)
            .write_u8(item.level_min)
            .write_u8(item.level_max)
            .write_u32(item.display_item_id)
            .write_bool(item.is_sale)
            .write_bool(item.is_hidden)
            .write_u8(item.limited_type)
            .write_u16(item.limited_stock_max)
            .write_u8(item.buy_restrict_type)
            .write_u32(item.buy_restrict_id)
            .write_i64(db::unix(item.sale_start))
            .write_i64(db::unix(item.sale_end));
        match item.first_sku() {
            Some(first) => {
                s.write_u8(first.currency)
                    .write_u32(first.price)
                    .write_i32(item.remaining)
                    .write_u32(first.bonus_item_id)
                    .write_u32(first.bonus_item_count)
                    .write_u8(item.shop_buttons);
            }
            None => {
                // No item for some reason?
                s.write_u8(currency::CREDITS)
                    .write_i32(0)
                    .write_i32(0)
                    .write_i32(0)
                    .write_i32(0)
                    .write_u8(cmd_ui_type::ALLOW_ALL);
            }
        }
    }
}

/// `SCICSGoodDetailPacket(pageEnd, itemDetail)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSGoodDetailPacket {
    pub page_end: bool,
    pub sku: IcsSku,
}
packet_def!(
    SCICSGoodDetailPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSGoodDetailPacket
);
impl Marshal for SCICSGoodDetailPacket {
    fn write(&self, s: &mut PacketStream) {
        let d = &self.sku;
        s.write_bool(self.page_end)
            .write_u32(d.shop_id)
            .write_u32(d.sku)
            .write_u32(d.item_id)
            .write_u32(d.item_count)
            .write_u8(d.select_type)
            .write_bool(d.is_default)
            .write_u8(d.event_type)
            .write_i64(db::unix(d.event_end_date))
            .write_u8(d.currency)
            .write_u32(d.price)
            .write_u32(d.discount_price)
            .write_u32(d.bonus_item_id)
            .write_u32(d.bonus_item_count);
    }
}

/// `SCICSCheckTimePacket` (empty: the client re-requests the shop list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSCheckTimePacket;
packet_def!(
    SCICSCheckTimePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSCheckTimePacket
);
impl Marshal for SCICSCheckTimePacket {
    fn write(&self, _s: &mut PacketStream) {}
}

/// `SCICSBuyResultPacket(success, buyMode, receiverName, chargeAaPoint)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSBuyResultPacket {
    pub success: bool,
    pub buy_mode: u8,
    pub receiver_name: String,
    pub charge_aa_point: i32,
}
packet_def!(
    SCICSBuyResultPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSBuyResultPacket
);
impl Marshal for SCICSBuyResultPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.success)
            .write_u8(self.buy_mode)
            .write_str(&self.receiver_name)
            .write_i32(self.charge_aa_point);
    }
}

/// `SCICSSyncGoodPacket(cashShopId, remainCount)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSSyncGoodPacket {
    pub cash_shop_id: i32,
    pub remain_count: i32,
}
packet_def!(
    SCICSSyncGoodPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSSyncGoodPacket
);
impl Marshal for SCICSSyncGoodPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.cash_shop_id).write_i32(self.remain_count);
    }
}

/// `SCICSCashPointPacket(point)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSCashPointPacket {
    pub point: i32,
}
packet_def!(
    SCICSCashPointPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSCashPointPacket
);
impl Marshal for SCICSCashPointPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.point);
    }
}

/// `SCICSExchangeRatioPacket(exchangeRatio)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCICSExchangeRatioPacket {
    pub exchange_ratio: i32,
}
packet_def!(
    SCICSExchangeRatioPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCICSExchangeRatioPacket
);
impl Marshal for SCICSExchangeRatioPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.exchange_ratio);
    }
}

/// `SCPremiumPointChangedPacket(objId, point)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCPremiumPointChangedPacket {
    pub obj_id: u32,
    pub point: i32,
}
packet_def!(
    SCPremiumPointChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCPremiumPointChangedPacket
);
impl Marshal for SCPremiumPointChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id).write_i32(self.point);
    }
}

/// `SCBmPointPacket(bmPoint)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCBmPointPacket {
    pub bm_point: i64,
}
packet_def!(
    SCBmPointPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCBmPointPacket
);
impl Marshal for SCBmPointPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i64(self.bm_point);
    }
}

/// `SCAccountWarnedPacket(source, msg)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCAccountWarnedPacket {
    pub source: u8,
    pub msg: String,
}
packet_def!(
    SCAccountWarnedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCAccountWarnedPacket
);
impl Marshal for SCAccountWarnedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.source).write_str(&self.msg);
    }
}

// ---------------------------------------------------------------------------------------------
// C->S packets
// ---------------------------------------------------------------------------------------------

/// `CSICSMenuListPacket` (empty body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSICSMenuListPacket;

impl CSICSMenuListPacket {
    fn read_body(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        tracing::warn!("ICSMenuList");
        let packet = {
            let st = manager(server).state();
            SCICSMenuListPacket::new(st.enabled, &st.menu_items)
        };
        if let Some(p) = packet {
            conn.send(&p);
        }
    }
}
sync_client_packet!(CSICSMenuListPacket);

/// `CSICSGoodsListPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSICSGoodsListPacket {
    pub main_tab: u8,
    pub sub_tab: u8,
    pub page: u16,
}

impl CSICSGoodsListPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            main_tab: s.read_u8()?,
            sub_tab: s.read_u8()?,
            page: s.read_u16()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        send_ics_page(server, conn, self.main_tab, self.sub_tab, self.page);
    }
}
sync_client_packet!(CSICSGoodsListPacket);

/// One entry of `CSICSBuyGoodPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcsBuyEntry {
    pub cash_shop_id: u32,
    pub main_tab: u8,
    pub sub_tab: u8,
    pub detail_index: u8,
}

/// `CSICSBuyGoodPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSICSBuyGoodPacket {
    pub buys: Vec<IcsBuyEntry>,
    pub receiver_name: String,
}

impl CSICSBuyGoodPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        let num = s.read_u8()?;
        let mut buys = Vec::with_capacity(usize::from(num));
        for _ in 0..num {
            buys.push(IcsBuyEntry {
                cash_shop_id: s.read_u32()?,
                main_tab: s.read_u8()?,
                sub_tab: s.read_u8()?,
                detail_index: s.read_u8()?,
            });
        }
        Ok(Self {
            buys,
            receiver_name: s.read_string()?,
        })
    }

    async fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(this_char) = active(conn) else {
            return;
        };
        let buy_mode = 1u8; // No idea what this means
        let name = this_char.data().unit.name.clone();
        let buyer_account = this_char.data().account_id;
        let mut buy_list = Vec::new();
        {
            let st = manager(server).state();
            for b in &self.buys {
                let Some(shop_item) = st.shop_items.get(&b.cash_shop_id) else {
                    tracing::warn!(
                        "{name} is trying to shop for invalid ShopItem: {}",
                        b.cash_shop_id
                    );
                    continue;
                };
                let Some(sku) = shop_item.skus.get(usize::from(b.detail_index)) else {
                    tracing::warn!(
                        "{name} is trying to shop from ShopItem: {}, but with invalid index: {}",
                        shop_item.shop_id,
                        b.detail_index
                    );
                    continue;
                };
                buy_list.push(sku.clone());
            }
        }
        // The receiver is an empty name (buy for self) or a name resolved from the online world
        // first and the `characters` table second, so an offline character still gets the gift.
        let target = if self.receiver_name.is_empty() {
            let (id, name, account_id) = {
                let c = this_char.data();
                (c.id, c.unit.name.clone(), c.account_id)
            };
            ReceiverLookup::Found(BuyTarget {
                id,
                name,
                account_id,
                player: Some(this_char.clone()),
            })
        } else {
            resolve_receiver(server, &self.receiver_name).await
        };
        match target {
            ReceiverLookup::Found(target) => {
                // The 1.2 text `INGAME_SHOP_FIND_CHARACTER_SAME_ACCOUNT` (779) says "you cannot
                // gift your own character"; it covers an alt on the buyer's account and typing
                // one's own name in gift mode.
                if !self.receiver_name.is_empty() && target.account_id == buyer_account {
                    fail(
                        &this_char,
                        error_message::INGAME_SHOP_FIND_CHARACTER_SAME_ACCOUNT,
                        buy_mode,
                        &self.receiver_name,
                    );
                    return;
                }
                if buy_list.is_empty() {
                    fail(
                        &this_char,
                        error_message::BUY_CART_EMPTY,
                        buy_mode,
                        &self.receiver_name,
                    );
                    return;
                }
                // Create task for the transaction (TaskManager.Schedule(..., 1 second))
                let Some(server) = server.self_arc() else {
                    tracing::error!("CSICSBuyGoodPacket: server handle not registered");
                    return;
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    execute_buy_for(&server, buy_mode, &this_char, &target, &buy_list).await;
                });
            }
            refused => {
                let error = refused
                    .refusal_error()
                    .unwrap_or(error_message::INGAME_SHOP_FIND_CHARACTER_NAME_FAIL);
                fail(&this_char, error, buy_mode, &self.receiver_name);
            }
        }
    }
}

impl ClientPacket for CSICSBuyGoodPacket {
    const NAME: &'static str = "CSICSBuyGoodPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Self::read_body(s)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.run(server, conn).await;
            Ok(())
        })
    }
}

/// `CSICSMoneyRequestPacket` (empty body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSICSMoneyRequestPacket;

impl ClientPacket for CSICSMoneyRequestPacket {
    const NAME: &'static str = "CSICSMoneyRequestPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!("ICSMoneyRequest");
            let points = manager(server)
                .store()
                .account_details(server, conn.account_id())
                .await;
            conn.send(&SCICSCashPointPacket {
                point: points.credits,
            });
            Ok(())
        })
    }
}
