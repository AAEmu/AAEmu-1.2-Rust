//! Port of `Core/Managers/World/SpecialtyManager` (trade pack prices and per-zone ratios, selling
//! a trade pack to a specialty trader), its tasks (`SpecialtyRatioConsumeTask`,
//! `SpecialtyRatioRegenTask`) and packets (`CSSpecialtyRatioPacket`,
//! `CSRequestSpecialtyCurrentPacket`, `CSListSpecialtyGoodsPacket`, `CSSpecialtyRecordLoadPacket`,
//! `CSBuySpecialtyItemPacket`, `SCSpecialtyRatioPacket`, `SCSpecialtyCurrentPacket`).
//! The tables live in [`super::store_data::StoreData`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aaemu_commons::{Marshal, PacketStream};

use crate::data::features::Feature;
use crate::data::items::ITEM_COINS;
use crate::error::Result;
use crate::models::container::equip_slot;
use crate::models::item_task::task_type;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::{cs, sc};
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{PlayerRef, WorldObject};

use super::craft::zone_group_id;
use super::error_message;
use super::inventory::{self, Inv, ItemTxn};
use super::item_handlers::{active, sync_client_packet};
use super::store::{parent_world, world_distance, SpecialtyMail};

/// `ActabilityType.Commerce`.
pub const ACTABILITY_COMMERCE: i32 = 31;

/// 1.2 `SCSpecialtyGoodsPacket` reader `FUN_397aec90`: an `i32` count (it caps the read at 20)
/// followed by that many records read by `FUN_398a9e30`.
pub const MAX_SPECIALTY_GOODS: usize = 20;

/// Upper bound for the configured ratio tick periods (one year) so a bad config value cannot
/// panic `Duration::from_secs_f64`.
const MAX_TICK_MINUTES: f64 = 24.0 * 60.0 * 365.0;

/// The ratio/tick part of `AppConfiguration.Specialty` (`SpecialtyConfig`; `initialize` copies
/// it from `server.config.specialty`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpecialtyConfig {
    pub max_specialty_ratio: i32,
    pub min_specialty_ratio: i32,
    pub ratio_decrease_per_pack: f64,
    pub ratio_increase_per_tick: f64,
    pub ratio_decrease_tick_minutes: f64,
    pub ratio_regen_tick_minutes: f64,
}

impl Default for SpecialtyConfig {
    fn default() -> Self {
        Self {
            max_specialty_ratio: 130,
            min_specialty_ratio: 70,
            ratio_decrease_per_pack: f64::from(0.5f32),
            ratio_increase_per_tick: 5.0,
            ratio_decrease_tick_minutes: 1.0,
            ratio_regen_tick_minutes: 60.0,
        }
    }
}

#[derive(Debug, Default)]
struct Ratios {
    /// `_priceRatios` (item id -> zone group id -> ratio).
    price_ratios: HashMap<u32, HashMap<u32, f64>>,
    /// `_soldPackAmountInTick` (item id -> zone group id -> count).
    sold_pack_amount_in_tick: HashMap<u32, HashMap<u32, i32>>,
}

impl Ratios {
    /// `InitRatioInZoneForPack`.
    fn init(&mut self, cfg: &SpecialtyConfig, item_id: u32, zone_group_id: u32) -> &mut f64 {
        self.price_ratios
            .entry(item_id)
            .or_default()
            .entry(zone_group_id)
            .or_insert(f64::from(cfg.max_specialty_ratio))
    }
}

/// One `SCSpecialtyGoodsPacket` record as read by `FUN_398a9e30`: `itemId` u32, `refund` i32,
/// 1-byte `Currency`, 1-byte unknown (10 bytes on the wire). The content of the list is not
/// evidenced, so the server currently sends count 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecialtyGood {
    pub item_id: u32,
    pub refund: i32,
    pub currency: u8,
    pub unknown: u8,
}

/// `SpecialtyManager` runtime state (`server.systems.economy.specialty`).
#[derive(Debug, Default)]
pub struct SpecialtyManager {
    ratios: Mutex<Ratios>,
    config: Mutex<SpecialtyConfig>,
}

impl SpecialtyManager {
    fn lock(&self) -> MutexGuard<'_, Ratios> {
        self.ratios.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn config(&self) -> SpecialtyConfig {
        *self.config.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_config(&self, cfg: SpecialtyConfig) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// The current ratio of `item_id` in `zone_group_id` (initialised like
    /// `InitRatioInZoneForPack`).
    pub fn ratio(&self, item_id: u32, zone_group_id: u32) -> f64 {
        let cfg = self.config();
        *self.lock().init(&cfg, item_id, zone_group_id)
    }

    /// `_soldPackAmountInTick[itemId][zoneGroupId]`.
    pub fn sold_in_tick(&self, item_id: u32, zone_group_id: u32) -> i32 {
        self.lock()
            .sold_pack_amount_in_tick
            .get(&item_id)
            .and_then(|z| z.get(&zone_group_id))
            .copied()
            .unwrap_or(0)
    }

    /// `ConsumeRatio()`.
    pub fn consume_ratio(&self) {
        let cfg = self.config();
        let mut r = self.lock();
        let entries: Vec<(u32, u32, i32)> = r
            .sold_pack_amount_in_tick
            .iter()
            .flat_map(|(i, z)| z.iter().map(move |(g, c)| (*i, *g, *c)))
            .collect();
        for (item_id, zone_group_id, count) in entries {
            if count <= 0 {
                continue;
            }
            let ratio_decrease = (f64::from(count) * cfg.ratio_decrease_per_pack).ceil() as i32;
            r.init(&cfg, item_id, zone_group_id);
            if let Some(c) = r
                .sold_pack_amount_in_tick
                .get_mut(&item_id)
                .and_then(|z| z.get_mut(&zone_group_id))
            {
                *c = 0;
            }
            let ratio = r.init(&cfg, item_id, zone_group_id);
            *ratio = f64::from(cfg.min_specialty_ratio).max(*ratio - f64::from(ratio_decrease));
        }
    }

    /// `RegenRatio()`.
    pub fn regen_ratio(&self) {
        let cfg = self.config();
        let mut r = self.lock();
        let entries: Vec<(u32, u32)> = r
            .sold_pack_amount_in_tick
            .iter()
            .flat_map(|(i, z)| z.keys().map(move |g| (*i, *g)))
            .collect();
        for (item_id, zone_group_id) in entries {
            let ratio = r.init(&cfg, item_id, zone_group_id);
            *ratio = f64::from(cfg.max_specialty_ratio).min(*ratio + cfg.ratio_increase_per_tick);
        }
    }

    fn add_sold_pack(&self, item_id: u32, zone_group_id: u32) {
        *self
            .lock()
            .sold_pack_amount_in_tick
            .entry(item_id)
            .or_default()
            .entry(zone_group_id)
            .or_insert(0) += 1;
    }
}

/// A ratio tick period: at least 1 ms and at most [`MAX_TICK_MINUTES`]. A non-finite value
/// (bad JSON / arithmetic) uses the maximum instead of panicking in `Duration::from_secs_f64`.
fn tick_period(minutes: f64) -> std::time::Duration {
    let seconds = if minutes.is_finite() {
        (minutes * 60.0).clamp(0.001, MAX_TICK_MINUTES * 60.0)
    } else {
        MAX_TICK_MINUTES * 60.0
    };
    std::time::Duration::from_secs_f64(seconds)
}

/// `SpecialtyManager.Initialize()`: the configuration and the ratio consume / regen timers.
pub fn initialize(server: &Arc<GameServer>) {
    let configured = &server.config.specialty;
    server
        .systems
        .economy
        .specialty
        .set_config(SpecialtyConfig {
            max_specialty_ratio: configured.max_specialty_ratio,
            min_specialty_ratio: configured.min_specialty_ratio,
            ratio_decrease_per_pack: configured.ratio_decrease_per_pack,
            ratio_increase_per_tick: configured.ratio_increase_per_tick,
            ratio_decrease_tick_minutes: configured.ratio_decrease_tick_minutes,
            ratio_regen_tick_minutes: configured.ratio_regen_tick_minutes,
        });
    server
        .systems
        .economy
        .mail
        .set_trade_pack_mail_delay_minutes(configured.trade_pack_mail_delay_in_minutes);
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let cfg = server.systems.economy.specialty.config();
    let every = |minutes: f64| tick_period(minutes);
    let s = Arc::downgrade(server);
    let consume = every(cfg.ratio_decrease_tick_minutes);
    rt.spawn(async move {
        let mut t = tokio::time::interval_at(tokio::time::Instant::now() + consume, consume);
        loop {
            t.tick().await;
            let Some(s) = s.upgrade() else { break };
            s.systems.economy.specialty.consume_ratio();
        }
    });
    let s = Arc::downgrade(server);
    let regen = every(cfg.ratio_regen_tick_minutes);
    rt.spawn(async move {
        let mut t = tokio::time::interval_at(tokio::time::Instant::now() + regen, regen);
        loop {
            t.tick().await;
            let Some(s) = s.upgrade() else { break };
            s.systems.economy.specialty.regen_ratio();
        }
    });
}

/// The player's zone group (`ZoneManager.GetZoneByKey(player.Transform.ZoneId)?.GroupId ?? 0`).
fn player_zone_group(server: &GameServer, p: &PlayerRef) -> u32 {
    let zone = p.data().unit.transform.zone_id;
    zone_group_id(server, zone)
}

/// `GetRatioForSpecialty(player)` (rounded down).
pub fn get_ratio_for_specialty(server: &GameServer, p: &PlayerRef) -> i32 {
    let Some(backpack) = Inv::of(p).equipped(equip_slot::BACKPACK) else {
        return 0;
    };
    let zone_group = player_zone_group(server, p);
    server
        .systems
        .economy
        .specialty
        .ratio(backpack.template_id, zone_group)
        .floor() as i32
}

/// `GetRatiosForTargetRoute(fromZoneGroupId, toZoneGroupId)`.
pub fn get_ratios_for_target_route(
    server: &GameServer,
    from_zone_group_id: u32,
    to_zone_group_id: u32,
) -> Vec<(u32, u32)> {
    let mgr = &server.systems.economy.specialty;
    server
        .game_data
        .systems
        .economy
        .store
        .specialty_packs
        .iter()
        .filter(|(_, z)| *z == from_zone_group_id)
        .map(|(id, _)| (*id, mgr.ratio(*id, to_zone_group_id).floor() as u32))
        .collect()
}

/// The pack the client asks about with `CSSpecialtyRatioPacket.id`: the requested template when
/// it is a specialty pack, else the equipped trade pack.
fn specialty_ratio_item_id(server: &GameServer, p: &PlayerRef, requested: u32) -> Option<u32> {
    let is_pack = server
        .game_data
        .systems
        .economy
        .store
        .specialty_packs
        .iter()
        .any(|(id, _)| *id == requested);
    if is_pack {
        return Some(requested);
    }
    Inv::of(p)
        .equipped(equip_slot::BACKPACK)
        .map(|item| item.template_id)
}

/// The pack-sale labor cost. The 1.2 C# `SpecialtyManager` charged the flat 60 and skill 25205
/// has `consume_lp = 0`, so the Commerce `GetLaborCostMultiplier` table
/// (1.00/1.00/0.95/0.90/0.85/0.80/0.77/0.77 by Commerce step) is only applied when
/// `Specialty:ScaleSaleLaborByCommerce` is enabled (capture-pending LABOR-1), never below 1.
fn labor_cost_for_sale(commerce_step: u8, scale_by_commerce: bool) -> i32 {
    if !scale_by_commerce {
        return 60;
    }
    let cost = f64::from(60.0f32 * inventory::actability_labor_cost_multiplier(commerce_step));
    (cost.round() as i32).max(1)
}

/// The Commerce actability step (0 when the character has no Commerce row).
fn commerce_step(p: &PlayerRef) -> u8 {
    p.data()
        .actability
        .actabilities
        .iter()
        .find(|a| a.id == ACTABILITY_COMMERCE as u32)
        .map_or(0, |a| a.step)
}

/// Negotiation ("bargaining") bonus. The 1.2 client only displays the server-computed values:
/// `ui_texts` `sell_backpack_body2` / `bargaining_money` / `SPECIALTY_MARGIN` (626) /
/// `SPECIALTY_COIN_MARGIN` (677) say a Commerce-proficiency bargaining bonus exists, but no
/// formula or threshold is evidenced. Keep the C# 0 by default; operators may configure a
/// percentage, applied to the pre-interest sale price.
fn negotiation_bonus(server: &GameServer, price_no_interest: f32) -> i32 {
    let percent = server.config.specialty.negotiation_bonus_percent;
    if !percent.is_finite() || percent <= 0.0 {
        return 0;
    }
    let percent = percent.clamp(0.0, 100.0);
    (f64::from(price_no_interest) * percent / 100.0).round_ties_even() as i32
}

/// `GetBasePriceForSpecialty(player, npcObjId)`.
fn get_base_price_for_specialty(server: &GameServer, p: &PlayerRef, npc_obj_id: u32) -> i32 {
    // Sanity checks
    let Some(backpack) = Inv::of(p).equipped(equip_slot::BACKPACK) else {
        inventory::send_error_message(p, error_message::STORE_BACKPACK_NOGOODS, 0, true);
        return 0;
    };
    let world = parent_world(server, p);
    let Some((world, npc)) = world.and_then(|w| w.npc(npc_obj_id).map(|n| (w, n))) else {
        inventory::send_error_message(p, error_message::INVALID_TARGET, 0, true);
        return 0;
    };
    let npc_template = npc.data().unit.template_id;
    tracing::info!(
        "GetBasePriceForSpecialty - {} backpack:{}, npcObjId:{npc_obj_id}, npc: {npc_template}",
        p.data().unit.name,
        backpack.template_id
    );
    if f64::from(world_distance(
        &world,
        &WorldObject::from(p),
        &WorldObject::from(&npc),
    )) > 2.5
    {
        inventory::send_error_message(p, error_message::TOO_FAR_AWAY, 0, true);
        return 0;
    }
    let data = &server.game_data.systems.economy.store;
    let Some(specialty_npc) = data.specialty_npcs.get(&npc_template) else {
        inventory::send_error_message(p, error_message::STORE_CANT_SELL_SAME_ZONE, 0, true);
        return 0;
    };
    let bundle_id_at_npc = specialty_npc.specialty_bundle_id;
    let Some(bundle_item) = data
        .specialty_bundle_items_mapped
        .get(&backpack.template_id)
        .and_then(|m| m.get(&bundle_id_at_npc))
    else {
        inventory::send_error_message(p, error_message::INVALID, 0, true);
        return 0;
    };
    let refund = server
        .game_data
        .items
        .template(bundle_item.item_id)
        .map_or(0, |t| t.refund);
    ((f64::from(bundle_item.profit as f32 * (bundle_item.ratio as f32 / 1000f32))).floor()
        + f64::from(refund)) as i32
}

/// `Math.Round` (banker's rounding) of a float, as an int.
fn round_f32(v: f32) -> i32 {
    f64::from(v).round_ties_even() as i32
}

/// `SellSpecialty(player, npcObjId)`: returns the base price.
pub fn sell_specialty(server: &GameServer, p: &PlayerRef, npc_obj_id: u32) -> i32 {
    let labor_cost = labor_cost_for_sale(
        commerce_step(p),
        server.config.specialty.scale_sale_labor_by_commerce,
    );
    if p.data().labor_power < labor_cost as i16 {
        inventory::send_error_message(p, error_message::NOT_ENOUGH_LABOR_POWER, 0, true);
        return 0;
    }
    let base_price = get_base_price_for_specialty(server, p, npc_obj_id);
    if base_price == 0 {
        // We had an error, no need to keep going
        return base_price;
    }
    let price_ratio = get_ratio_for_specialty(server, p);

    let inv = Inv::of(p);
    let Some(backpack) = inv.equipped(equip_slot::BACKPACK) else {
        inventory::send_error_message(p, error_message::STORE_BACKPACK_NOGOODS, 0, true);
        return base_price;
    };
    if super::item_security::refuse_if_locked(p, &backpack) {
        return base_price;
    }
    let Some(npc) = parent_world(server, p).and_then(|w| w.npc(npc_obj_id)) else {
        return base_price;
    };

    // Our backpack isn't null, we have the NPC, time to calculate the profits
    let player_id = p.character_id;
    // The craft path stores the crafting character id in the item's `crafter_id` (and, before
    // that column existed, in `made_unit_id`); `Item::crafter` reads both, and the item packet
    // writes the value back to the client. Any other field the C# hoped for does not exist in
    // the 1.2 data.
    let crafter_id = if backpack.crafter() != player_id {
        backpack.crafter()
    } else {
        0
    };
    let seller_share = 0.80f32; // 80% default, set this to 1f for packs that don't share profit
    let interest_rate = 5;

    let final_price_no_interest = base_price as f32 * (price_ratio as f32 / 100f32);
    let interest = final_price_no_interest * (interest_rate as f32 / 100f32);
    let raw_bonus = negotiation_bonus(server, final_price_no_interest);
    let final_price = final_price_no_interest + interest + raw_bonus as f32;

    let hooks = server.systems.economy.store.hooks();
    let specialty_coin_id = hooks.npc_specialty_coin_id(server, &npc);
    let mut item_type_to_deliver = specialty_coin_id;
    let mut amount_total = round_f32(final_price);
    let mut amount_seller = amount_total;
    let mut amount_crafter = 0;
    let mut amount_base = base_price;
    let mut amount_bonus = raw_bonus;

    if specialty_coin_id != 0 {
        // Items are listed in the DB at the same rate as "amounts of gold" so the value needs
        // to be divided by 10000
        amount_total = round_f32(amount_total as f32 / 10000f32);
        amount_seller = round_f32(amount_seller as f32 / 10000f32);
        amount_base = round_f32(base_price as f32 / 10000f32);
        amount_bonus = round_f32(amount_bonus as f32 / 10000f32);
    } else {
        item_type_to_deliver = ITEM_COINS;
    }

    // `FeaturesManager.Fsets` (the global / runtime set), not a fresh default.
    let fsets = server.systems.services.features(server);

    // Split up the profit if needed
    if crafter_id != 0 && fsets.check(Feature::BackpackProfitShare) {
        amount_seller = round_f32(amount_total as f32 * seller_share);
        amount_crafter = amount_total - amount_seller;
    }

    let mail = |for_crafter| SpecialtyMail {
        seller_character_id: player_id,
        crafter_id,
        trade_pack_template_id: backpack.template_id,
        trade_rate: price_ratio,
        item_reward_template_id: item_type_to_deliver,
        item_count_base: amount_base,
        item_count_bonus: amount_bonus,
        item_count_for_seller: amount_seller,
        item_count_for_crafter: amount_crafter,
        interest_rate,
        for_crafter,
    };

    // Mail for seller
    if amount_seller > 0 && !hooks.send_specialty_mail(server, p, &mail(false)) {
        inventory::send_error_message(p, error_message::MAIL_UNKNOWN_FAILURE, 0, true);
        return base_price;
    }

    // Mail for crafter. If seller is not crafter, send a crafter mail as well
    if amount_crafter > 0 && crafter_id != 0 && !hooks.send_specialty_mail(server, p, &mail(true)) {
        inventory::send_error_message(p, error_message::MAIL_UNKNOWN_FAILURE, 0, true);
        // don't cancel here if we fail to send mail to crafter
    }

    {
        let mut txn = ItemTxn::begin(server);
        // Delete the backpack
        inv.equipment()
            .lock()
            .expect("container poisoned")
            .consume_item(
                task_type::SELL_BACKPACK,
                backpack.template_id,
                1,
                Some(backpack.id),
                &mut txn.env,
            );
        let mut out = txn.out();
        inventory::change_labor(
            &mut out,
            server,
            p,
            -(labor_cost as i16),
            ACTABILITY_COMMERCE,
        );
    }

    // The client shows a "bargaining succeeded" notice for a non-zero bonus (`specialty_margin`
    // 626 for coins, `specialty_coin_margin` 677 for specialty coin items).
    if amount_bonus > 0 {
        let message = if item_type_to_deliver == ITEM_COINS {
            error_message::SPECIALTY_MARGIN
        } else {
            error_message::SPECIALTY_COIN_MARGIN
        };
        inventory::send_error_message(p, message, 0, true);
    }

    // SELL_ITEM: the pack sold (value1) at the destination npc template (value2).
    let npc_template_id = npc.data().unit.template_id;
    crate::systems::report_achievement(
        server,
        p,
        AchievementInput::Increment {
            kind_id: char_record_kind::SELL_ITEM,
            amount: 1,
            matcher: RecordMatch::Value1And2 {
                value1: backpack.template_id,
                value2: npc_template_id,
            },
        },
    );
    // Add one pack sold in this zone during this tick
    let zone_group = player_zone_group(server, p);
    server
        .systems
        .economy
        .specialty
        .add_sold_pack(backpack.template_id, zone_group);
    base_price
}

// -------------------------------------------------------------------------------------------
// Packets
// -------------------------------------------------------------------------------------------

pub fn register(reg: &mut PacketRegistry) {
    // Not registered in the 1.2 `GameNetwork` (no opcode): `CSSpecialtyRecordLoadPacket`, `CSBuySpecialtyItemPacket`.
    reg.register::<CSSpecialtyRatioPacket>(LEVEL_PLAIN, cs::CSSpecialtyRatioPacket);
    reg.register::<CSRequestSpecialtyCurrentPacket>(
        LEVEL_PLAIN,
        cs::CSRequestSpecialtyCurrentPacket,
    );
    reg.register::<CSListSpecialtyGoodsPacket>(LEVEL_PLAIN, cs::CSListSpecialtyGoodsPacket);
}

/// `CSSpecialtyRatioPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSpecialtyRatioPacket {
    pub id: u32,
}

impl CSSpecialtyRatioPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self { id: s.read_u32()? })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        let zone_group = player_zone_group(server, &p);
        // The client sends the pack's item template id; answer for that pack (falling back to
        // the equipped one when the id is not a specialty pack, as the 1.2 C# always did).
        let item_id = specialty_ratio_item_id(server, &p, self.id);
        let ratio = item_id.map_or(0, |item_id| {
            server
                .systems
                .economy
                .specialty
                .ratio(item_id, zone_group)
                .floor() as i32
        });
        p.send(SCSpecialtyRatioPacket { ratio });
        // The 1.2 client discards `SCSpecialtyRecordsPacket` (`FUN_39181ff0` ->
        // `FUN_394fb300`), so the trader-talk path keeps the captured sequence and sends no
        // 0x9b.
    }
}
sync_client_packet!(CSSpecialtyRatioPacket);

/// `CSRequestSpecialtyCurrentPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSRequestSpecialtyCurrentPacket {
    pub from_zone_group_id: u16,
    pub to_zone_group_id: u16,
}

impl CSRequestSpecialtyCurrentPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            from_zone_group_id: s.read_u16()?,
            to_zone_group_id: s.read_u16()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        tracing::trace!(
            "RequestSpecialityRates: Player {}, FromZoneGroup {}, ToZoneGroup {}",
            p.data().unit.name,
            self.from_zone_group_id,
            self.to_zone_group_id
        );
        let items = get_ratios_for_target_route(
            server,
            u32::from(self.from_zone_group_id),
            u32::from(self.to_zone_group_id),
        );
        p.send(SCSpecialtyCurrentPacket {
            from_zone_group: self.from_zone_group_id,
            to_zone_group: self.to_zone_group_id,
            results: items,
        });
    }
}
sync_client_packet!(CSRequestSpecialtyCurrentPacket);

/// `CSListSpecialtyGoodsPacket` (`FUN_393bf840`: sends the interacted NPC object id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSListSpecialtyGoodsPacket {
    pub obj_id: u32,
}

impl CSListSpecialtyGoodsPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
        })
    }

    fn run(self, _server: &GameServer, conn: &mut GameConnection) {
        let Some(p) = active(conn) else { return };
        // The goods content is not evidenced. `FUN_398a9e30` proves each record's 10-byte
        // layout, but not which goods/prices belong in the list, so answer an empty list.
        tracing::debug!("ListSpecialtyGoods, ObjId: {} -> 0 goods", self.obj_id);
        p.send(SCSpecialtyGoodsPacket { goods: Vec::new() });
    }
}
sync_client_packet!(CSListSpecialtyGoodsPacket);

/// `CSSpecialtyRecordLoadPacket` (no 1.2 opcode: not registered, as in C#; its history packet
/// is not sent either because the 1.2 client discards it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSpecialtyRecordLoadPacket {
    pub zone_id: i32,
    pub id: u32,
}

impl CSSpecialtyRecordLoadPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            zone_id: s.read_i32()?,
            id: s.read_u32()?,
        })
    }

    fn run(self, _server: &GameServer, _conn: &mut GameConnection) {
        tracing::debug!(
            "CSSpecialtyRecordLoadPacket, ZoneId: {}, Id: {}",
            self.zone_id,
            self.id
        );
    }
}
sync_client_packet!(CSSpecialtyRecordLoadPacket);

/// `CSBuySpecialtyItemPacket` (only logged in C#).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSBuySpecialtyItemPacket {
    pub id: u32,
    pub refund: i32,
    pub currency: u8,
    pub kind: u8,
    pub obj_id: u32,
}

impl CSBuySpecialtyItemPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            id: s.read_u32()?,
            refund: s.read_i32()?,
            currency: s.read_u8()?,
            kind: s.read_u8()?,
            obj_id: s.read_bc()?,
        })
    }

    fn run(self, _server: &GameServer, _conn: &mut GameConnection) {
        tracing::debug!(
            "BuySpecialtyItem, Id: {}, Currency: {}",
            self.id,
            self.currency
        );
    }
}
sync_client_packet!(CSBuySpecialtyItemPacket);

/// `SCSpecialtyRatioPacket(ratio)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSpecialtyRatioPacket {
    pub ratio: i32,
}
packet_def!(
    SCSpecialtyRatioPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSpecialtyRatioPacket
);
impl Marshal for SCSpecialtyRatioPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.ratio);
    }
}

/// `SCSpecialtyCurrentPacket(fromZoneGroup, toZoneGroup, results)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCSpecialtyCurrentPacket {
    pub from_zone_group: u16,
    pub to_zone_group: u16,
    pub results: Vec<(u32, u32)>,
}
packet_def!(
    SCSpecialtyCurrentPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSpecialtyCurrentPacket
);
impl Marshal for SCSpecialtyCurrentPacket {
    fn write(&self, s: &mut PacketStream) {
        // 1.2 reader `FUN_397b87c0`: `i32` count, `u16` from zone group, `u16` to zone group,
        // then (u32 item id, i32 ratio) pairs.
        s.write_i32(self.results.len() as i32)
            .write_u16(self.from_zone_group)
            .write_u16(self.to_zone_group);
        for (item_id, rate) in &self.results {
            s.write_u32(*item_id).write_i32(*rate as i32);
        }
    }
}

/// `SCSpecialtyGoodsPacket` reader `FUN_397aec90`: `i32` count (capped at 20), then records read
/// by `FUN_398a9e30` as `u32 itemId`, `i32 refund`, 1-byte `Currency`, 1-byte unknown (10 bytes).
/// The content of the list is not evidenced, so only the empty list is sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SCSpecialtyGoodsPacket {
    pub goods: Vec<SpecialtyGood>,
}
packet_def!(
    SCSpecialtyGoodsPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSpecialtyGoodsPacket
);
impl Marshal for SCSpecialtyGoodsPacket {
    fn write(&self, s: &mut PacketStream) {
        let count = self.goods.len().min(MAX_SPECIALTY_GOODS);
        s.write_i32(count as i32);
        for good in self.goods.iter().take(count) {
            s.write_u32(good.item_id)
                .write_i32(good.refund)
                .write_u8(good.currency)
                .write_u8(good.unknown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tick_period, MAX_TICK_MINUTES};

    #[test]
    fn tick_period_is_clamped_and_never_panics() {
        let max = std::time::Duration::from_secs_f64(MAX_TICK_MINUTES * 60.0);
        assert_eq!(tick_period(60.0), std::time::Duration::from_secs(3600));
        assert_eq!(tick_period(-5.0), std::time::Duration::from_secs_f64(0.001));
        assert_eq!(tick_period(f64::INFINITY), max);
        assert_eq!(tick_period(f64::NAN), max);
        assert_eq!(tick_period(1.0e300), max);
    }
}
