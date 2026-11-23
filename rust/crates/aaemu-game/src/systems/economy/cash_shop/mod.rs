//! Port of `Core/Managers/CashShopManager.cs`, `Models/Game/CashShop/*` (`IcsItem`, `IcsSku`,
//! `IcsMenu`, `AuditIcsSale`), `Models/Tasks/CashShop/CashShopBuyTask.cs`, the account credit /
//! loyalty members of `AccountManager` the shop uses, and the packets `CSICSMenuListPacket`,
//! `CSICSGoodsListPacket`, `CSICSBuyGoodPacket`, `CSICSMoneyRequestPacket`, `SCICSMenuListPacket`,
//! `SCICSGoodListPacket`, `SCICSGoodDetailPacket`, `SCICSCheckTimePacket`, `SCICSBuyResultPacket`,
//! `SCICSSyncGoodPacket`, `SCICSCashPointPacket`, `SCICSExchangeRatioPacket`,
//! `SCPremiumPointChangedPacket`, `SCBmPointPacket`, `SCAccountWarnedPacket`
//! (`SCUpdatePremiumPointPacket` lives in `packets::g2c`). The premium service shop (WP-C) lives
//! in [`super::premium_service`]; its types are re-exported below.
//!
//! The shop tables come from MySQL (`ics_skus`, `ics_shop_items`, `ics_menu`), loaded in
//! `System::start`. Account values and the sales audit go through [`CashShopDb`] (MySQL by
//! default, the C# SQL; tests install an in-memory store). Bought goods are delivered by
//! `CommercialMail`, which belongs to the mail port: [`CashShopMailHooks`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::NaiveDateTime;
use sqlx::Row;

use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;
use crate::db;
use crate::error::Result;
use crate::models::item::Item;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::cs;
use crate::server::GameServer;
use crate::systems::{DataLoadCtx, HookSlot};
use crate::world::PlayerRef;

use super::error_message;

mod checkout;
mod packets;
mod persistence;

pub use self::checkout::{execute_buy, execute_buy_for, resolve_receiver};
pub use self::packets::{
    CSICSBuyGoodPacket, CSICSGoodsListPacket, CSICSMenuListPacket, CSICSMoneyRequestPacket,
    IcsBuyEntry, SCAccountWarnedPacket, SCBmPointPacket, SCICSBuyResultPacket,
    SCICSCashPointPacket, SCICSCheckTimePacket, SCICSExchangeRatioPacket, SCICSGoodDetailPacket,
    SCICSGoodListPacket, SCICSMenuListPacket, SCICSSyncGoodPacket, SCPremiumPointChangedPacket,
};
pub use self::persistence::{
    add_credits_sql, add_loyalty_sql, find_character_by_name_sql, log_sale_sql,
    sales_for_shop_item_sql, update_remaining_stock_sql, CashShopDb, MySqlCashShopDb,
};

// ---------------------------------------------------------------------------------------------
// Static values
// ---------------------------------------------------------------------------------------------

/// `CashShopCurrencyType`.
pub mod currency {
    pub const CREDITS: u8 = 0;
    pub const AA_POINTS: u8 = 1;
    pub const LOYALTY: u8 = 2;
    pub const COINS: u8 = 3;
    pub const MAX: u8 = 4;
}

/// `CashShopLimitType`.
pub mod limit_type {
    pub const NONE: u8 = 0;
    pub const ACCOUNT: u8 = 1;
    pub const CHARACTER: u8 = 2;
}

/// `CashShopRestrictSaleType`.
pub mod restrict_sale_type {
    pub const NONE: u8 = 0;
    pub const LEVEL: u8 = 1;
    pub const QUEST: u8 = 2;
}

/// `CashShopCmdUiType`.
pub mod cmd_ui_type {
    pub const ALLOW_ALL: u8 = 0;
    pub const NO_CART_ALLOWED: u8 = 1;
    pub const NO_GIFT_ALLOWED: u8 = 2;
    pub const ONLY_BUY_ALLOWED: u8 = 3;
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

/// `IcsSku` (table `ics_skus`). Dates: `None` = `DateTime.MinValue`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IcsSku {
    pub sku: u32,
    pub shop_id: u32,
    pub position: i32,
    pub item_id: u32,
    pub item_count: u32,
    pub select_type: u8,
    pub is_default: bool,
    pub event_type: u8,
    pub event_end_date: Option<NaiveDateTime>,
    pub currency: u8,
    pub price: u32,
    pub discount_price: u32,
    pub bonus_item_id: u32,
    pub bonus_item_count: u32,
}

impl IcsSku {
    /// `DiscountPrice > 0 ? DiscountPrice : Price`.
    pub fn effective_price(&self) -> u32 {
        if self.discount_price > 0 {
            self.discount_price
        } else {
            self.price
        }
    }
}

/// `IcsItem` (table `ics_shop_items`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IcsItem {
    pub shop_id: u32,
    pub display_item_id: u32,
    pub name: String,
    pub limited_type: u8,
    pub limited_stock_max: u16,
    pub level_min: u8,
    pub level_max: u8,
    pub buy_restrict_type: u8,
    pub buy_restrict_id: u32,
    pub is_sale: bool,
    pub is_hidden: bool,
    pub sale_start: Option<NaiveDateTime>,
    pub sale_end: Option<NaiveDateTime>,
    pub remaining: i32,
    pub shop_buttons: u8,
    /// `Skus` (insertion order).
    pub skus: Vec<IcsSku>,
}

impl IcsItem {
    /// `FirstSku`.
    pub fn first_sku(&self) -> Option<&IcsSku> {
        self.skus.first()
    }
}

/// `IcsMenu` (table `ics_menu`); `ShopItem` is referenced by its shop id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IcsMenu {
    pub id: i64,
    pub main_tab: u8,
    pub sub_tab: u8,
    pub tab_pos: i32,
    pub shop_id: u32,
}

/// `AuditIcsSale` (table `audit_ics_sales`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditIcsSale {
    pub id: i64,
    pub buyer_account: u32,
    pub buyer_char: u32,
    pub target_account: u32,
    pub target_char: u32,
    pub sale_date: Option<NaiveDateTime>,
    pub shop_item_id: u32,
    pub sku: u32,
    pub sale_cost: i32,
    pub sale_currency: u8,
    pub description: String,
}

// The premium shop moved to `systems::economy::premium_service` (WP-C). The old paths stay
// available for callers of the cash shop module.
pub use super::premium_service::{
    CSPremiumServiceBuyPacket, CSPremiumServiceListPacket, CSPremiumServiceMsgPacket,
    PremiumDetail, SCPremiumServiceListPacket,
};

// ---------------------------------------------------------------------------------------------
// Game data: item names (`LocalizationManager.Get("items", "name", id)`)
// ---------------------------------------------------------------------------------------------

/// The localized texts the cash shop reads (`server.game_data.systems.economy.cash_shop`).
#[derive(Debug, Clone, Default)]
pub struct CashShopData {
    /// `localized_texts` rows `items.name` in `DefaultLanguage`.
    pub item_names: HashMap<u32, String>,
}

impl CashShopData {
    pub fn load(ctx: &DataLoadCtx<'_>) -> std::result::Result<Self, GameDataError> {
        let mut d = Self::default();
        let sql = format!(
            "SELECT idx, \"{}\" AS txt FROM localized_texts \
             WHERE tbl_name = 'items' AND tbl_column_name = 'name'",
            ctx.language.replace('"', "")
        );
        for_each_row(ctx.conn, &sql, [], |r| {
            let text = r.string_or("txt", "")?;
            d.item_names.insert(r.i64("idx")? as u32, text);
            Ok(())
        })?;
        Ok(d)
    }

    /// `LocalizationManager.Get("items", "name", id)` (empty texts and misses give "").
    pub fn item_name(&self, id: u32) -> String {
        self.item_names.get(&id).cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Hooks / stores
// ---------------------------------------------------------------------------------------------

/// `new CommercialMail(receiverId, receiverName, senderName, items, isGift, isRefund,
/// purchasedItemTitle)` + `FinalizeMail()` + `Send()`.
#[derive(Debug, Clone, PartialEq)]
pub struct CommercialMailRequest {
    pub receiver_id: u32,
    pub receiver_name: String,
    pub sender_name: String,
    /// Freshly created items (`ItemManager.Create`, not in any container).
    pub items: Vec<Item>,
    pub is_gift: bool,
    pub is_refund: bool,
    pub purchased_item_title: String,
}

/// A gift receiver resolved before the 1 s buy task runs. The online world only knows connected
/// characters, so an offline receiver comes from the `characters` row; the mail is then delivered
/// by `CommercialMail` (type 9, `CHARGED`) and counted on the receiver's next login.
#[derive(Debug, Clone)]
pub struct BuyTarget {
    pub id: u32,
    pub name: String,
    pub account_id: u32,
    /// `Some` while the receiver is connected.
    pub player: Option<PlayerRef>,
}

/// A `characters` row for a receiver that is not online (`characters.name` is matched with the
/// table's case-insensitive collation, like `WorldManager::character_by_name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterRow {
    pub id: u32,
    pub name: String,
    pub account_id: u32,
    pub deleted: bool,
    pub delete_requested: bool,
    pub transfer_requested: bool,
}

/// The outcome of looking a gift receiver up by name.
#[derive(Debug, Clone)]
pub enum ReceiverLookup {
    Found(BuyTarget),
    /// No `characters` row and nobody online with that name.
    NameNotFound,
    /// The row is deleted or its deletion is pending (`delete_request_time` set).
    DeleteRequested,
    /// The row is waiting for a server transfer (`transfer_request_time` set).
    TransferRequested,
}

impl ReceiverLookup {
    /// The 1.2 `SCErrorMsg` id the client maps to the matching gift-refusal text. 623 is
    /// `INGAME_SHOP_FIND_CHARACTER_NAME_FAIL`, 790 `INGAME_SHOP_CHRACTER_DELETE_REQUESTED`,
    /// 791 `INGAME_SHOP_CHRACTER_TRANSFER_REQUESTED` (`x2game` error -> BFR reason map,
    /// `FUN_39040000`, and `ui_texts` keys of the same names). **791 as a *receiver* refusal is
    /// UNVERIFIED:** its client text ("Can't make purchases while a server transfer is
    /// pending") describes the buyer, and `transfer_request_time` is never set by the server
    /// (only loaded and saved), so this refusal cannot fire today.
    pub fn refusal_error(&self) -> Option<i16> {
        match self {
            Self::Found(_) => None,
            Self::NameNotFound => Some(error_message::INGAME_SHOP_FIND_CHARACTER_NAME_FAIL),
            Self::DeleteRequested => Some(error_message::INGAME_SHOP_CHRACTER_DELETE_REQUESTED),
            Self::TransferRequested => Some(error_message::INGAME_SHOP_CHRACTER_TRANSFER_REQUESTED),
        }
    }
}

/// Cash shop deliveries (implemented by the mail port: `CommercialMail`).
pub trait CashShopMailHooks: Send + Sync {
    /// Builds, finalizes (moves the items to the receiver's mail attachments when online) and
    /// sends the mail; returns `mail.Send()`.
    fn send_commercial_mail(&self, _server: &GameServer, _mail: CommercialMailRequest) -> bool {
        false
    }
}

/// Quest facts the shop restrictions read (implemented by `systems::quests`).
pub trait CashShopQuestHooks: Send + Sync {
    /// `character.Quests.HasQuestCompleted(questId)`.
    fn has_quest_completed(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        _quest_id: u32,
    ) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

/// The loaded shop (`SKUs`, `ShopItems`, `MenuItems`, `Enabled`).
#[derive(Debug, Clone, Default)]
pub struct CashShopState {
    pub enabled: bool,
    /// `SKUs` (load order: `shop_id, position`).
    pub skus: Vec<IcsSku>,
    /// `ShopItems` by shop id.
    pub shop_items: HashMap<u32, IcsItem>,
    /// `MenuItems` (ordered by main tab, sub tab, position).
    pub menu_items: Vec<IcsMenu>,
}

impl CashShopState {
    /// `SKUs.TryGetValue(sku)`.
    pub fn sku(&self, sku: u32) -> Option<&IcsSku> {
        self.skus.iter().find(|s| s.sku == sku)
    }

    /// The attach / name pass of `Load` after `SKUs` and `ShopItems` were read.
    pub fn attach_skus(&mut self, names: &CashShopData) {
        for sku in &self.skus {
            if let Some(shop_item) = self.shop_items.get_mut(&sku.shop_id) {
                if shop_item.skus.is_empty() && shop_item.name.trim().is_empty() {
                    // First Item, grab it's name when needed (`Get` never returns null: the
                    // "???" fallback of the C# is unreachable)
                    shop_item.name = names.item_name(sku.item_id);
                }
                shop_item.skus.push(sku.clone());
            } else {
                tracing::warn!(
                    "Found SKU without a valid Shop Item SKU: {}, ShopItem: {}",
                    sku.sku,
                    sku.shop_id
                );
            }
        }
        for (key, shop_item) in &self.shop_items {
            if shop_item.skus.is_empty() {
                tracing::error!("Shop Item found without any SKUs attached {key}");
            }
        }
    }
}

/// `CashShopManager` (`server.systems.economy.cash_shop`).
#[derive(Debug, Default)]
pub struct CashShopManager {
    state: Mutex<CashShopState>,
    /// Serializes the async checkout against other checkouts on this server.
    purchases: tokio::sync::Mutex<()>,
    /// Account / audit store; MySQL ([`MySqlCashShopDb`]) when nothing is installed.
    pub db: HookSlot<dyn CashShopDb>,
    /// `CommercialMail` (mail port).
    pub mail: HookSlot<dyn CashShopMailHooks>,
    /// `Quests.HasQuestCompleted` (quests port).
    pub quests: HookSlot<dyn CashShopQuestHooks>,
}

impl CashShopManager {
    pub fn state(&self) -> MutexGuard<'_, CashShopState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the shop tables (loaders and tests).
    pub fn set_state(&self, state: CashShopState) {
        *self.state() = state;
    }

    pub fn enabled(&self) -> bool {
        self.state().enabled
    }

    fn store(&self) -> Arc<dyn CashShopDb> {
        self.db.get().unwrap_or_else(|| Arc::new(MySqlCashShopDb))
    }
}

fn manager(server: &GameServer) -> &CashShopManager {
    &server.systems.economy.cash_shop
}

/// `CashShopManager.Load()` (MySQL) + `GameService`'s `EnabledShop()` right after.
pub async fn load(server: &GameServer) -> Result<()> {
    load_only(server).await?;
    // GameService: CashShopManager.Instance.EnabledShop() after the load stage.
    enable_shop(server);
    Ok(())
}

/// `CashShopManager.Load()` alone (the GM `ics reload`: the enabled flag is kept unless
/// something failed to load).
pub async fn load_only(server: &GameServer) -> Result<()> {
    let mut st = CashShopState::default();
    let pool = &server.db;

    // Load SKUs
    let rows = sqlx::query(
        "SELECT CAST(sku AS SIGNED) AS sku, CAST(shop_id AS SIGNED) AS shop_id, CAST(position AS SIGNED) AS position, \
         CAST(item_id AS SIGNED) AS item_id, CAST(item_count AS SIGNED) AS item_count, \
         CAST(select_type AS SIGNED) AS select_type, CAST(is_default AS SIGNED) AS is_default, \
         CAST(event_type AS SIGNED) AS event_type, event_end_date, CAST(currency AS SIGNED) AS currency, \
         CAST(price AS SIGNED) AS price, CAST(discount_price AS SIGNED) AS discount_price, \
         CAST(bonus_item_id AS SIGNED) AS bonus_item_id, CAST(bonus_item_count AS SIGNED) AS bonus_item_count \
         FROM ics_skus ORDER BY shop_id, position",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let entry = IcsSku {
            sku: db::int(&row, "sku")? as u32,
            shop_id: db::int(&row, "shop_id")? as u32,
            position: db::int(&row, "position")? as i32,
            item_id: db::int(&row, "item_id")? as u32,
            item_count: db::int(&row, "item_count")? as u32,
            select_type: db::int(&row, "select_type")? as u8,
            is_default: db::int(&row, "is_default")? != 0,
            event_type: db::int(&row, "event_type")? as u8,
            event_end_date: db::dt(&row, "event_end_date"),
            currency: db::int(&row, "currency")? as u8,
            price: db::int(&row, "price")? as u32,
            discount_price: db::int(&row, "discount_price")? as u32,
            bonus_item_id: db::int(&row, "bonus_item_id")? as u32,
            bonus_item_count: db::int(&row, "bonus_item_count")? as u32,
        };
        if st.sku(entry.sku).is_some() {
            tracing::error!("Duplicate SKU {}", entry.sku);
        } else {
            st.skus.push(entry);
        }
    }

    // Load Shop Items
    let rows = sqlx::query(
        "SELECT CAST(shop_id AS SIGNED) AS shop_id, CAST(display_item_id AS SIGNED) AS display_item_id, name, \
         CAST(limited_type AS SIGNED) AS limited_type, CAST(limited_stock_max AS SIGNED) AS limited_stock_max, \
         CAST(level_min AS SIGNED) AS level_min, CAST(level_max AS SIGNED) AS level_max, \
         CAST(buy_restrict_type AS SIGNED) AS buy_restrict_type, CAST(buy_restrict_id AS SIGNED) AS buy_restrict_id, \
         CAST(is_sale AS SIGNED) AS is_sale, CAST(is_hidden AS SIGNED) AS is_hidden, sale_start, sale_end, \
         CAST(remaining AS SIGNED) AS remaining, CAST(shop_buttons AS SIGNED) AS shop_buttons \
         FROM ics_shop_items",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let entry = IcsItem {
            shop_id: db::int(&row, "shop_id")? as u32,
            display_item_id: db::int(&row, "display_item_id")? as u32,
            name: row
                .try_get::<Option<String>, _>("name")
                .ok()
                .flatten()
                .unwrap_or_default(),
            limited_type: db::int(&row, "limited_type")? as u8,
            limited_stock_max: db::int(&row, "limited_stock_max")? as u16,
            level_min: db::int(&row, "level_min")? as u8,
            level_max: db::int(&row, "level_max")? as u8,
            buy_restrict_type: db::int(&row, "buy_restrict_type")? as u8,
            buy_restrict_id: db::int(&row, "buy_restrict_id")? as u32,
            is_sale: db::int(&row, "is_sale")? != 0,
            is_hidden: db::int(&row, "is_hidden")? != 0,
            sale_start: db::dt(&row, "sale_start"),
            sale_end: db::dt(&row, "sale_end"),
            remaining: db::int(&row, "remaining")? as i32,
            shop_buttons: db::int(&row, "shop_buttons")? as u8,
            skus: Vec::new(),
        };
        match st.shop_items.entry(entry.shop_id) {
            std::collections::hash_map::Entry::Occupied(_) => {
                tracing::error!("Duplicate ShopItem {}", entry.shop_id);
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(entry);
            }
        }
    }

    // Attach SKUs to Shop Items
    st.attach_skus(&server.game_data.systems.economy.cash_shop);

    let rows = sqlx::query(
        "SELECT CAST(id AS SIGNED) AS id, CAST(main_tab AS SIGNED) AS main_tab, CAST(sub_tab AS SIGNED) AS sub_tab, \
         CAST(tab_pos AS SIGNED) AS tab_pos, CAST(shop_id AS SIGNED) AS shop_id \
         FROM ics_menu ORDER BY main_tab, sub_tab, tab_pos",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let shop_id = db::int(&row, "shop_id")? as u32;
        if !st.shop_items.contains_key(&shop_id) {
            tracing::warn!("Menu Entry without a valid ShopId: {shop_id}");
            continue;
        }
        // Note that this List should technically always be in order by main, sub and position
        st.menu_items.push(IcsMenu {
            id: db::int(&row, "id")?,
            main_tab: db::int(&row, "main_tab")? as u8,
            sub_tab: db::int(&row, "sub_tab")? as u8,
            tab_pos: db::int(&row, "tab_pos")? as i32,
            shop_id,
        });
    }
    let empty = st.menu_items.is_empty() || st.shop_items.is_empty() || st.skus.is_empty();
    // `Load` does not touch `Enabled`.
    st.enabled = manager(server).enabled();
    manager(server).set_state(st);
    // If something didn't load, force close the shop
    if empty {
        disable_shop(server);
    }
    // One guard: two `state()` temporaries in the same statement would self-deadlock.
    let (items, menus) = {
        let st = manager(server).state();
        (st.shop_items.len(), st.menu_items.len())
    };
    tracing::info!("Loaded {items} cash shop items in {menus} menu entries");
    Ok(())
}

/// `EnabledShop()`.
pub fn enable_shop(server: &GameServer) {
    manager(server).state().enabled = true;
}

/// `DisableShop()`: every character re-requests the shop list.
pub fn disable_shop(server: &GameServer) {
    manager(server).state().enabled = false;
    for p in server.world.all_characters() {
        p.send(SCICSCheckTimePacket);
    }
}

/// `CreditDisperseTick` (its timer is commented out in the C# `Initialize`).
pub async fn credit_disperse_tick(server: &GameServer) {
    let store = manager(server).store();
    for p in server.world.all_characters() {
        let account_id = p.data().account_id;
        store.add_credits(server, account_id, 100).await;
        crate::world::session::send_message(
            &p,
            crate::managers::chat::chat_type::SYSTEM,
            "You have received 100 credits.",
        );
    }
}

/// `SendICSPage(connection, mainTabId, subTabId, page)`.
pub fn send_ics_page(
    server: &GameServer,
    conn: &mut GameConnection,
    main_tab: u8,
    sub_tab: u8,
    page: u16,
) {
    for p in ics_page(server, main_tab, sub_tab, page) {
        conn.send(p.as_ref());
    }
}

/// The packets of `SendICSPage`.
pub fn ics_page(
    server: &GameServer,
    main_tab: u8,
    sub_tab: u8,
    page: u16,
) -> Vec<Box<dyn crate::net::packet::ServerPacket>> {
    let st = manager(server).state();
    let this_tab: Vec<&IcsMenu> = st
        .menu_items
        .iter()
        .filter(|t| t.main_tab == main_tab && t.sub_tab == sub_tab)
        .collect();
    let is_limited_tab = main_tab == 1 && sub_tab == 1;
    let items_per_page: usize = if is_limited_tab { 4 } else { 8 };
    let number_of_pages = (this_tab.len() as f32 / items_per_page as f32).ceil() as u16;
    // `Skip(itemsPerPage * (page - 1))`: page 0 skips a negative count (= nothing).
    let skip = (items_per_page as i64 * (i64::from(page) - 1)).max(0) as usize;
    let this_page: Vec<&IcsItem> = this_tab
        .iter()
        .skip(skip)
        .take(items_per_page)
        .filter_map(|m| st.shop_items.get(&m.shop_id))
        .collect();
    let mut out: Vec<Box<dyn crate::net::packet::ServerPacket>> = Vec::new();
    for (i, shop_item) in this_page.iter().enumerate() {
        let is_last = i == this_page.len() - 1;
        out.push(Box::new(SCICSGoodListPacket {
            page_end: is_last,
            total_page: number_of_pages,
            main_tab,
            sub_tab,
            item: (*shop_item).clone(),
        }));
    }
    for (i, shop_item) in this_page.iter().enumerate() {
        let is_last_item = i + 1 >= this_page.len();
        for (n, sku) in shop_item.skus.iter().enumerate() {
            let is_last_sku = n + 1 >= shop_item.skus.len();
            out.push(Box::new(SCICSGoodDetailPacket {
                page_end: is_last_sku && is_last_item,
                sku: sku.clone(),
            }));
        }
    }
    out
}

pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSICSMenuListPacket>(LEVEL_PLAIN, cs::CSICSMenuListPacket);
    reg.register::<CSICSGoodsListPacket>(LEVEL_PLAIN, cs::CSICSGoodsListPacket);
    reg.register::<CSICSBuyGoodPacket>(LEVEL_PLAIN, cs::CSICSBuyGoodPacket);
    reg.register::<CSICSMoneyRequestPacket>(LEVEL_PLAIN, cs::CSICSMoneyRequestPacket);
    super::premium_service::register(reg);
}
