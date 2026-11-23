//! The item side of the item skill effects (`Models/Game/Skills/Effects/SpecialEffects/*` and the
//! item effects of `Models/Game/Skills/Effects`), as plain functions the skills subsystem calls
//! from its effect dispatch with the resolved inputs:
//!
//! * `caster is Character` -> the caller passes the [`PlayerRef`] (or does not call when the
//!   caster is not a character; [`item_conversion`] / [`cleanup_look_convert`] then set
//!   `skill.Cancelled` as the C# does);
//! * `casterObj is SkillItem` -> `Some(SkillItem { item_id, item_template_id })`, else `None`;
//! * `targetObj is SkillCastItemTarget` -> `Some(id)` (or `Some((id, objId))`), else `None`;
//! * `skillObject is SkillObjectItemGradeEnchantingSupport` -> `Some(SupportItemId)`;
//! * `Random.Shared` -> a [`LootRng`] (`SharedRandom` in production, `ScriptedRng` in tests).
//!
//! Functions that return [`Outcome`] tell the caller whether the C# set `skill.Cancelled`.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::managers::chat::chat_type;
use crate::models::container::{slot_type, ContainerRef};
use crate::models::item::Item;
use crate::models::item_task::ItemTask;
use crate::net::packet::ServerPacket;
use crate::packets::world::SCChatMessagePacket;
use crate::server::GameServer;
use crate::systems::HookSlot;
use crate::world::{MateRef, ObjectKind, PlayerRef, WorldObject};

use super::inventory::ItemTxn;
use super::item_packets::SCItemTaskSuccessPacket;
use super::item_security;
use super::loot::LootRng;

mod backpack_and_slave;
mod enhancement;
mod regrade;
mod rewards;

pub use self::backpack_and_slave::{
    drop_trade_pack_to_floor, put_down_backpack, repair_slave_item,
};
pub use self::enhancement::{
    cleanup_look_convert, item_cap_scale, item_cap_scale_reset, item_socketing, magical_enchant,
    recharge_item_buff,
};
pub use self::regrade::{charm_chance, gold_cost, grade_enchant, roll_regrade};
pub use self::rewards::{
    apply_stamp, cleanup_ucc, conversion_effect, create_stamp, exp_to_item, fishing_loot,
    gain_item, gain_item_with_emblem_imprint, gain_item_with_pos_imprint, gain_loot_pack_item,
    imprint_ucc, item_conversion, GainLootPackItemEffect,
};

/// `ActabilityType.None` / `ActabilityType.Fishing`.
pub mod actability_type {
    pub const NONE: u8 = 0;
    pub const FISHING: u8 = 7;
}

/// `SkillItem` (the `casterObj` of an item-used skill).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkillItem {
    pub item_id: u64,
    pub item_template_id: u32,
}

/// Whether the effect set `skill.Cancelled = true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
}

// ---------------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------------

/// Tables the effect bodies read that no other subsystem loads yet.
#[derive(Debug, Default)]
pub struct ItemEffectsData {
    /// `ZoneGroup.FishingSeaLootPackId` / `FishingLandLootPackId` by zone group id (every
    /// `zone_groups` row: a missing key is `GetZoneGroupById == null`).
    pub fishing_loot_packs: HashMap<u32, FishingLootPacks>,
    /// `SlaveGameData._repairableSlaves` (slave id -> repair effect id; first row wins).
    pub repairable_slaves: HashMap<u32, u32>,
    /// `ItemGameData._itemGradeBuffs` (item template id -> grade -> buff id).
    pub item_grade_buffs: HashMap<u32, HashMap<u8, u32>>,
}

/// The fishing loot packs of a zone group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FishingLootPacks {
    pub sea: u32,
    pub land: u32,
}

impl ItemEffectsData {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut d = Self::default();
        for_each_row(conn, "SELECT * FROM zone_groups", [], |r| {
            d.fishing_loot_packs.insert(
                r.u32("id")?,
                FishingLootPacks {
                    sea: r.u32_or("fishing_sea_loot_pack_id", 0)?,
                    land: r.u32_or("fishing_land_loot_pack_id", 0)?,
                },
            );
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM item_grade_buffs", [], |r| {
            // `_itemGradeBuffs[itemId].Add(itemGrade, buffId)` (a duplicate throws in C#; the first
            // row is kept).
            d.item_grade_buffs
                .entry(r.u32("item_id")?)
                .or_default()
                .entry(r.u8("item_grade_id")?)
                .or_insert(r.u32("buff_id")?);
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM repairable_slaves", [], |r| {
            let slave_id = r.u32("slave_id")?;
            let effect_id = r.u32("repair_slave_effect_id")?;
            match d.repairable_slaves.entry(slave_id) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    tracing::warn!("Duplicate entry for repairable_slaves");
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(effect_id);
                }
            }
            Ok(())
        })?;
        Ok(d)
    }

    /// `ItemGameData.GetItemBuff(itemId, gradeId)` (the buff id).
    pub fn item_grade_buff(&self, item_id: u32, grade: u8) -> Option<u32> {
        self.item_grade_buffs.get(&item_id)?.get(&grade).copied()
    }

    /// `SlaveGameData.HasRepairEffectId(templateId, expectedEffectId)`.
    pub fn has_repair_effect_id(&self, slave_id: u32, effect_id: u32) -> bool {
        self.repairable_slaves.get(&slave_id) == Some(&effect_id)
    }
}

// ---------------------------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------------------------

/// `HousingManager.GetHouseAtLocation(x, y)` + `house.AllowedToInteract(character)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HouseAtLocation {
    pub house_id: u32,
    pub house_obj_id: u32,
    pub allowed_to_interact: bool,
}

/// What `PutDownBackpackEffect` hands to `DoodadManager.Create(...)` and the doodad setup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackpackDoodadSpawn {
    /// `BackpackDoodadId` (doodad template).
    pub doodad_template_id: u32,
    pub item_id: u64,
    pub item_template_id: u32,
    pub ucc_id: u64,
    /// `pos` (world position one metre in front of the character, rotation 0).
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Rotation (roll, pitch, yaw): 0 for a pack put down, the character's rotation for a pack
    /// dropped on death.
    pub rot: [f32; 3],
    /// The house the pack is put on (`OwnerType = Housing`, `ParentObj = house`).
    pub house: Option<HouseAtLocation>,
}

/// What the item effects need from other subsystems (mates: `systems::npcs` / mate owner,
/// housing, doodads). Defaults are the "not ported yet" behaviour; the mate lookups scan the
/// world's mates.
pub trait ItemEffectHooks: Send + Sync {
    /// `ParentWorld.MateManager.GetActiveMateByTlId(tlId)`.
    fn active_mate_by_tl_id(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        tl_id: u16,
    ) -> Option<MateRef> {
        world_mates(server, &WorldObject::from(player))
            .into_iter()
            .find(|m| m.data().unit.tl_id == tl_id)
    }

    /// The mate whose `Id` (mates table id) is `mate_id` (`MateEquipmentContainer.ParentUnit`).
    fn mate_by_id(&self, server: &GameServer, owner: &PlayerRef, mate_id: u32) -> Option<MateRef> {
        world_mates(server, &WorldObject::from(owner))
            .into_iter()
            .find(|m| m.data().id == mate_id)
    }

    /// `mate.Equipment` (`ItemManager.GetItemContainerForCharacter(owner.Id, EquipmentMate,
    /// mate, mate.Id)`, as `CharacterMates.SpawnMount` sets it).
    fn mate_equipment(&self, server: &GameServer, mate: &MateRef) -> Option<ContainerRef> {
        let (owner_obj_id, mate_id) = {
            let m = mate.data();
            (m.owner_obj_id, m.id)
        };
        let owner = server.world.character_by_obj_id(owner_obj_id)?;
        Some(server.items.container_for_character(
            owner.character_id,
            slot_type::EQUIPMENT_MATE,
            mate_id,
            &server.ids.container,
        ))
    }

    /// `PublicFarmManager.InPublicFarm(character.ParentWorld.Template, position)`.
    fn in_public_farm(&self, _server: &GameServer, _player: &PlayerRef) -> bool {
        false
    }

    /// `HousingManager.GetHouseAtLocation(x, y)` (+ `AllowedToInteract(character)`).
    fn house_at(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        _x: f32,
        _y: f32,
    ) -> Option<HouseAtLocation> {
        None
    }

    /// `DoodadManager.Create(world, 0, BackpackDoodadId, character, true)` + the setup in
    /// `PutDownBackpackEffect` (`IsPersistent`, transform, `ItemId`, `ItemTemplateId`, `UccId`,
    /// scale 1, `PlantTime`, house parent, `InitDoodad`, `Spawn`, `Save`). `false` = `Create`
    /// returned null.
    fn spawn_backpack_doodad(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        spawn: &BackpackDoodadSpawn,
    ) -> bool {
        tracing::warn!(
            "Doodad {}, from BackpackDoodadId could not be created",
            spawn.doodad_template_id
        );
        false
    }
}

/// The default [`ItemEffectHooks`].
#[derive(Debug, Default)]
pub struct DefaultItemEffectHooks;

impl ItemEffectHooks for DefaultItemEffectHooks {}

/// State of the item effects (`server.systems.economy.item_effects`).
#[derive(Debug, Default)]
pub struct ItemEffectsManager {
    pub hooks: HookSlot<dyn ItemEffectHooks>,
}

impl ItemEffectsManager {
    /// The installed hooks, or the defaults.
    pub fn hooks(&self) -> Arc<dyn ItemEffectHooks> {
        self.hooks
            .get()
            .unwrap_or_else(|| Arc::new(DefaultItemEffectHooks))
    }
}

/// Every mate in the world of `near`.
pub fn world_mates(server: &GameServer, near: &WorldObject) -> Vec<MateRef> {
    let Some(world) = server.world.world_of(near) else {
        return Vec::new();
    };
    world
        .all_of_kind(ObjectKind::Mate)
        .into_iter()
        .filter_map(|o| o.as_mate().cloned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn lock(c: &ContainerRef) -> std::sync::MutexGuard<'_, crate::models::container::ItemContainer> {
    c.lock().expect("container poisoned")
}

/// `BroadcastPacket(packet, true)` of a character.
fn broadcast_with_self(server: &GameServer, player: &PlayerRef, packet: &dyn ServerPacket) {
    let obj = WorldObject::from(player);
    match server.world.world_of(&obj) {
        Some(world) => crate::world::broadcast(&world, obj, packet, true),
        None => player.send_dyn(packet),
    }
}

/// `ItemManager.GetItemByItemId(id)` with mutation: the item in its persistent container, or
/// the loose item.
fn with_any_item_mut<R>(server: &GameServer, id: u64, f: impl FnOnce(&mut Item) -> R) -> Option<R> {
    if let Some(c) = server.items.holding_container(id) {
        let mut g = lock(&c);
        return g.get_item_by_id_mut(id).map(f);
    }
    let mut item = server.items.take_loose(id)?;
    let r = f(&mut item);
    server.items.add_loose(item);
    Some(r)
}

/// `Character.SendMessage(ChatType.System, message)` through the transaction queue.
fn system_message(txn: &mut ItemTxn<'_>, player: &PlayerRef, message: String) {
    txn.out().send(
        player,
        SCChatMessagePacket {
            chat_type: chat_type::SYSTEM,
            sender: None,
            message,
            ability: 0,
            language_type: 0,
        },
    );
}

fn task_packet(action: u8, tasks: Vec<ItemTask>) -> SCItemTaskSuccessPacket {
    SCItemTaskSuccessPacket {
        action,
        tasks,
        force_remove: Vec::new(),
    }
}
