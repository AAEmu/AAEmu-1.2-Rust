//! Pet / vehicle gear: `CSChangeMateEquipmentPacket` (moves between the bag and a mate's
//! `MateEquipmentContainer`), the `MateEquipmentContainer.OnEnter/LeaveContainer` packets
//! (`SCMateEquipmentChangedPacket`), the base `EquipmentContainer` events of a mate (the default
//! [`SummonItemHooks::on_mate_equipment_changed`]), the repairman's slave summon item repair
//! (`CSRepairSlaveItemsPacket`, handled in `vehicles::slaves`), and the stablehand mate revive
//! packet (`CSRepairPetItemsPacket`, handled in `vehicles::mates`). The slave equipment move
//! packet lives in `vehicles::packets` with the other slave packets.
//!
//! `change_mate_equipment` enforces the 1.2 client's slot permission (`mate_equip_slot_packs`
//! through `npcs.mate_equip_slot_pack_id`, see [`super::mate_equipment_data`]) and, while
//! FeatureSet bit 78 is set, the per-mate item list from `mate_equip_pack_groups`.

use std::collections::HashSet;
use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::error::Result;
use crate::models::container::{slot_type, ContainerRef};
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemFx, ItemFxList};
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::cs;
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

use super::hooks::SummonItemHooks;
use super::inventory::{self, Inv, ItemTxn};
use super::item_effect_packets::{ItemAndLocation, SCMateEquipmentChangedPacket};
use super::item_handlers::{active, sync_client_packet};
use super::item_packets::SCUnitEquipmentsChangedPacket;
use super::mate_equipment_data::MateSlotPack;

/// A raw wire item (`Item.Read` / `Item.Write`): the fields the container code needs. The detail
/// body is consumed according to its `ItemDetailType` (gear 55 bytes; the 1.2 reader has no
/// `SlaveEquipment` tag, so ship parts carry no body).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireItem {
    pub template_id: u32,
    pub id: u64,
    pub grade: u8,
    pub flags: u8,
    pub count: i32,
    pub detail_type: u8,
}

/// `new Item().Read(stream)`.
pub fn read_wire_item(s: &mut PacketStream) -> Result<WireItem> {
    let template_id = s.read_u32()?;
    if template_id == 0 {
        return Ok(WireItem::default());
    }
    let id = s.read_u64()?;
    let grade = s.read_u8()?;
    let flags = s.read_u8()?;
    let count = s.read_i32()?;
    let detail_type = s.read_u8()?;
    let detail_len = crate::models::item::detail_body_len(detail_type);
    if detail_len > 0 && s.remaining() >= detail_len {
        s.read_bytes(detail_len)?;
    }
    s.read_i64()?; // CreateTime
    s.read_i32()?; // LifespanMins
    s.read_u32()?; // MadeUnitId
    s.read_u8()?; // WorldId
    s.read_i64()?; // UnsecureTime
    s.read_i64()?; // UnpackTime
    Ok(WireItem {
        template_id,
        id,
        grade,
        flags,
        count,
        detail_type,
    })
}

/// `new EquipItem().Read(stream)`: skips one wire item, returns its template id.
pub fn read_wire_equip_item(s: &mut PacketStream) -> Result<u32> {
    Ok(read_wire_item(s)?.template_id)
}

/// Adds the `MateEquipmentContainer.OnEnterContainer` / `OnLeaveContainer` packets after the
/// mate gear events recorded in `fx` since `start` by `Inventory.SplitOrMoveItemEx`, which
/// passes the other container as `lastContainer` and `fromSlot` as `previousSlot` to every
/// event:
/// * enter: `(item, lastContainer.ContainerType, previousSlot)` / `(null, EquipmentMate,
///   item.Slot)`;
/// * leave: `(null, item.SlotType, item.Slot)` / `(item, EquipmentMate, previousSlot)`.
///
/// The packets go to the container owner, right after the event (whose base
/// `EquipmentContainer` part runs through [`SummonItemHooks::on_mate_equipment_changed`]).
pub fn insert_mate_equipment_packets(
    fx: &mut ItemFxList,
    start: usize,
    mate_id: u32,
    mate_tl_id: u16,
    last_container_type: u8,
    previous_slot: u8,
) {
    let tail = fx.0.split_off(start.min(fx.0.len()));
    for e in tail {
        let packet = match &e {
            ItemFx::EquipEnter {
                owner,
                mate_id: m,
                item,
                ..
            } if *m == mate_id && mate_id > 0 => Some((
                *owner,
                ItemAndLocation {
                    item: Some(*item.clone()),
                    slot_type: last_container_type,
                    slot: previous_slot,
                },
                ItemAndLocation {
                    item: None,
                    slot_type: slot_type::EQUIPMENT_MATE,
                    slot: item.slot as u8,
                },
            )),
            ItemFx::EquipLeave {
                owner,
                mate_id: m,
                prev_slot,
                item,
            } if *m == mate_id && mate_id > 0 => Some((
                *owner,
                ItemAndLocation {
                    item: None,
                    slot_type: item.slot_type,
                    slot: item.slot as u8,
                },
                ItemAndLocation {
                    item: Some(*item.clone()),
                    slot_type: slot_type::EQUIPMENT_MATE,
                    slot: *prev_slot,
                },
            )),
            _ => None,
        };
        fx.0.push(e);
        if let Some((owner, pet, bag)) = packet {
            fx.send(
                owner,
                SCMateEquipmentChangedPacket {
                    item_on_pet: pet,
                    item_in_bag: bag,
                    mate_tl_id,
                    character_id: owner,
                    passenger_id: 0,
                    bts: false,
                    success: true,
                },
            );
        }
    }
}

/// The base `EquipmentContainer.OnEnterContainer` / `OnLeaveContainer` of a mate's gear:
/// `ParentUnit.BroadcastPacket(SCUnitEquipmentsChangedPacket(mate.ObjId, slot, item|null),
/// false)` + `ParentUnit.UpdateGearBonuses(...)` (the `GearHooks` with the mate as unit). The
/// mate's equipment mirror (`Mate.equipment`, read by its visibility packets) is updated too.
pub fn on_mate_equipment_changed(
    server: &GameServer,
    owner: &PlayerRef,
    mate_id: u32,
    item: &Item,
    slot: u8,
    entered: bool,
) {
    let hooks = server.systems.economy.item_effects.hooks();
    let Some(mate) = hooks.mate_by_id(server, owner, mate_id) else {
        return; // ParentUnit == null
    };
    // The container already holds the post-move state. Reading it on leave keeps the mirror
    // and the broadcast correct for a swap: the C# sends `OnEnterContainer` for the moved item
    // before `OnLeaveContainer` of the item it displaced, both for the same slot.
    let current = if entered {
        Some(item.clone())
    } else {
        hooks.mate_equipment(server, &mate).and_then(|c| {
            c.lock()
                .expect("container poisoned")
                .get_item_by_slot(i32::from(slot))
                .cloned()
        })
    };
    {
        let mut m = mate.data();
        if let Some(e) = m.equipment.get_mut(usize::from(slot)) {
            *e = current.clone();
        }
    }
    let obj = WorldObject::from(&mate);
    if let Some(world) = server.world.world_of(&obj) {
        crate::world::broadcast(
            &world,
            obj.clone(),
            &SCUnitEquipmentsChangedPacket {
                obj_id: mate.obj_id,
                items: vec![(slot, current)],
            },
            false,
        );
    }
    if let Some(g) = server.systems.economy.hooks.gear.get() {
        if entered {
            g.update_gear_bonuses(server, &obj, Some(item), None);
        } else {
            g.update_gear_bonuses(server, &obj, None, Some(item));
        }
    }
}

/// The economy's default [`SummonItemHooks`] (installed by `System::start` when the slot is
/// empty): mate gear events per [`on_mate_equipment_changed`], the slave methods keep their
/// trait defaults.
#[derive(Debug, Default)]
pub struct EconomySummonHooks;

impl SummonItemHooks for EconomySummonHooks {
    fn on_mate_equipment_changed(
        &self,
        server: &GameServer,
        owner: &PlayerRef,
        mate_id: u32,
        item: &Item,
        slot: u8,
        entered: bool,
    ) {
        on_mate_equipment_changed(server, owner, mate_id, item, slot, entered);
    }
}

/// Installs [`EconomySummonHooks`] unless another subsystem already installed its own.
pub fn install_default_summon_hooks(server: &GameServer) {
    let slot = &server.systems.economy.hooks.summons;
    if slot.get().is_none() {
        slot.install(Arc::new(EconomySummonHooks));
    }
}

// ---------------------------------------------------------------------------------------------
// Packets
// ---------------------------------------------------------------------------------------------

/// `CSChangeMateEquipmentPacket` (the item entries are read lazily, as the C# reads them only
/// once the mate was found).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSChangeMateEquipmentPacket {
    pub owning_player_id: u32,
    pub mate_tl: u16,
    pub passenger_player_id: u32,
    pub bts: bool,
    pub item_count: u8,
    pub rest: Vec<u8>,
}

impl CSChangeMateEquipmentPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        let owning_player_id = s.read_u32()?;
        let mate_tl = s.read_u16()?;
        let passenger_player_id = s.read_u32()?;
        let bts = s.read_bool()?;
        let item_count = s.read_u8()?;
        let rest = s.read_bytes(s.remaining())?;
        Ok(Self {
            owning_player_id,
            mate_tl,
            passenger_player_id,
            bts,
            item_count,
            rest,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(character) = active(conn) else {
            return;
        };
        change_mate_equipment(server, &character, self);
    }
}
sync_client_packet!(CSChangeMateEquipmentPacket);

/// The two rules the 1.2 client applies to an item placed onto a mate, in its order:
/// `FUN_395ac760` (the `mate_equip_pack_groups` list, only while FeatureSet bit 78 is set) and
/// `FUN_3933a9c0` (the `mate_equip_slot_packs` row). `None` = the item may be placed; `Some` is
/// the reason for the debug log. A mate template the server data does not know skips both
/// checks (the client always knows its templates; this keeps test fixtures working); a known
/// template with no pack row refuses every slot like the client.
fn mate_gear_refusal(
    server: &GameServer,
    template_known: bool,
    slot_pack: Option<MateSlotPack>,
    pack_items: Option<&HashSet<u32>>,
    whitelist_enabled: bool,
    template_id: u32,
) -> Option<&'static str> {
    if !template_known {
        return None;
    }
    if whitelist_enabled && !pack_items.is_some_and(|items| items.contains(&template_id)) {
        return Some("is not in the mate's `mate_equip_pack_groups` list");
    }
    let Some(pack) = slot_pack else {
        return Some("has no `mate_equip_slot_packs` row for the mate's template");
    };
    let equip_slot = server
        .game_data
        .items
        .template(template_id)
        .and_then(|t| t.gear_slot_type())
        .and_then(|st| {
            crate::models::container::allowed_gear_slots(st)
                .first()
                .copied()
        });
    if !equip_slot.is_some_and(|slot| pack.allows(slot)) {
        return Some("is not allowed in the mate's equipment slots");
    }
    None
}

/// `CSChangeMateEquipmentPacket.Read` (the logic part).
pub fn change_mate_equipment(
    server: &GameServer,
    character: &PlayerRef,
    p: CSChangeMateEquipmentPacket,
) {
    tracing::debug!(
        "CSChangeMateEquipmentPacket - TlId: {}, Owner: {}, Id2: {}, BTS: {}, Count: {}",
        p.mate_tl,
        p.owning_player_id,
        p.passenger_player_id,
        p.bts,
        p.item_count
    );
    let hooks = server.systems.economy.item_effects.hooks();
    let Some(mate) = hooks.active_mate_by_tl_id(server, character, p.mate_tl) else {
        tracing::warn!(
            "ChangeMateEquipment, Unable to find mate with tlId {}!",
            p.mate_tl
        );
        return;
    };
    // A tl id identifies a visible mate; it does not grant access to its equipment.
    let (mate_id, mate_template_id) = {
        let m = mate.data();
        if p.owning_player_id != character.character_id || m.owner_obj_id != character.obj_id {
            return;
        }
        (m.id, m.unit.template_id)
    };
    if p.item_count == 0 {
        return;
    }
    let Some(mate_equipment) = hooks.mate_equipment(server, &mate) else {
        tracing::error!("ChangeMateEquipment, mate {} has no equipment", p.mate_tl);
        return;
    };
    // `IsMateEquipableSlot` (`FUN_3933a9c0`): the mate's NPC template names a
    // `mate_equip_slot_packs` row through `mate_equip_slot_pack_id`; when the template exists
    // but its pack is missing (0 / NULL), the client refuses every slot. Only a template the
    // server data does not know (test fixtures) stays unrestricted.
    let mate_rules = &server.game_data.systems.economy.mate_equipment;
    let mate_template = server.game_data.systems.npcs.npc.template(mate_template_id);
    let mate_template_known = mate_template.is_some();
    let mate_slot_pack = mate_template.and_then(|t| {
        mate_rules
            .slot_packs
            .get(&t.mate_equip_slot_pack_id)
            .copied()
    });
    // `mate_equip_pack_groups` x `mate_equip_pack_items`: the client checks the list through
    // `FUN_395ac760` only while FeatureSet bit 78 (`flag_9_6`, server-controlled;
    // `fset[9] & 0x40`, dword `DAT_39b02294 + 0x1c` bit 0x4000) is set.
    let mate_pack_items = mate_rules.pack_items.get(&mate_template_id);
    let whitelist_enabled = server.systems.services.features(server).check_bit(78);

    let mut s = PacketStream::from_bytes(p.rest);
    let inv = Inv::of(character);
    let bag = inv.bag();
    // The refusal the client shows for both rules: `SCMateEquipmentChanged{success:false}`
    // plus the error (the client's whitelist failure falls to error `0xcc` = `ITEM_NOT_FOR_PET`
    // 204, `FUN_394e41f0` `dll.c:1021556`; the slot failure id `0x22f` is capture-pending).
    let refuse =
        |txn: &mut ItemTxn<'_>, player_loc: &ItemAndLocation, mate_loc: &ItemAndLocation| {
            txn.out().send(
                character,
                SCMateEquipmentChangedPacket {
                    item_on_pet: player_loc.clone(),
                    item_in_bag: mate_loc.clone(),
                    mate_tl_id: p.mate_tl,
                    character_id: p.owning_player_id,
                    passenger_id: p.passenger_player_id,
                    bts: p.bts,
                    success: false,
                },
            );
            inventory::send_error_message_to(
                &mut txn.out(),
                character,
                super::error_message::ITEM_NOT_FOR_PET,
            );
        };
    for _ in 0..p.item_count {
        let Ok(player_template) = read_wire_equip_item(&mut s) else {
            return;
        };
        if read_wire_equip_item(&mut s).is_err() {
            return;
        }
        let (Ok(p_st), Ok(p_slot), Ok(m_st), Ok(m_slot)) =
            (s.read_u8(), s.read_u8(), s.read_u8(), s.read_u8())
        else {
            return;
        };
        let is_equip = player_template != 0;

        let mut txn = ItemTxn::begin(server);
        // Override the Read data with the actual Item data
        let equip_at = |c: &ContainerRef, slot: u8| -> std::result::Result<Option<Item>, ()> {
            match c
                .lock()
                .expect("container poisoned")
                .get_item_by_slot(i32::from(slot))
            {
                Some(i) if !i.is_equip() => Err(()), // C#: InvalidCastException on (EquipItem)
                other => Ok(other.cloned()),
            }
        };
        let (Ok(bag_item), Ok(mate_item)) =
            (equip_at(&bag, p_slot), equip_at(&mate_equipment, m_slot))
        else {
            tracing::error!("ChangeMateEquipment: a slot holds a non-equipment item");
            return;
        };
        let player_loc = ItemAndLocation {
            item: bag_item,
            slot_type: p_st,
            slot: p_slot,
        };
        let mate_loc = ItemAndLocation {
            item: mate_item,
            slot_type: m_st,
            slot: m_slot,
        };
        // If un-equipping, swap the items around
        let (src_loc, dst_loc, src, dst) = if is_equip {
            (&player_loc, &mate_loc, &bag, &mate_equipment)
        } else {
            (&mate_loc, &player_loc, &mate_equipment, &bag)
        };
        let Some(src_item) = &src_loc.item else {
            continue;
        };
        // The item that lands on the mate: the moved bag item when equipping, or the bag
        // item swapped onto the mate when unequipping into an occupied bag slot. Both follow
        // the client's whitelist and slot-pack checks.
        if let Some(worn) = player_loc.item.as_ref() {
            if let Some(reason) = mate_gear_refusal(
                server,
                mate_template_known,
                mate_slot_pack,
                mate_pack_items,
                whitelist_enabled,
                worn.template_id,
            ) {
                tracing::debug!(
                    "ChangeMateEquipment: mate {} (template {}) refuses item template {}: {}",
                    p.mate_tl,
                    mate_template_id,
                    worn.template_id,
                    reason
                );
                refuse(&mut txn, &player_loc, &mate_loc);
                continue;
            }
        }
        let start = txn.env.fx.len();
        let res = inventory::split_or_move_item_ex(
            &mut txn,
            &inv,
            task_type::INVALID,
            Some(src.clone()),
            Some(dst.clone()),
            src_item.id,
            src_loc.slot_type,
            src_loc.slot,
            0,
            dst_loc.slot_type,
            dst_loc.slot,
            0,
        );
        // Events of the mate container: lastContainer = the bag (the other side in both
        // directions), previousSlot = fromSlot.
        let last_type = bag.lock().expect("container poisoned").slot_type;
        insert_mate_equipment_packets(
            &mut txn.env.fx,
            start,
            mate_id,
            p.mate_tl,
            last_type,
            src_loc.slot,
        );
        if !res {
            // (isEquip ? playerItem : mateItem, isEquip ? mateItem : playerItem): always the
            // bag side first.
            txn.out().send(
                character,
                SCMateEquipmentChangedPacket {
                    item_on_pet: player_loc.clone(),
                    item_in_bag: mate_loc.clone(),
                    mate_tl_id: p.mate_tl,
                    character_id: p.owning_player_id,
                    passenger_id: p.passenger_player_id,
                    bts: p.bts,
                    success: false, // rejected move: the client must not apply the item copy
                },
            );
        }
    }
}

/// `CSRepairPetItemsPacket`: the stablehand revive of dead mates (not an equipment repair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSRepairPetItemsPacket {
    pub npc_id: u32,
}

impl CSRepairPetItemsPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            npc_id: s.read_bc()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(character) = active(conn) else {
            return;
        };
        crate::systems::vehicles::mates::revive_pet_items(server, &character, self.npc_id);
    }
}
sync_client_packet!(CSRepairPetItemsPacket);

/// `CSRepairSlaveItemsPacket`: the repairman restores destroyed slave summon items. The 1.2
/// C# only logged it; the client's cost preview (`GetTotalRepairsForSlaveItems`) and locale
/// strings define the service, and `vehicles::slaves::repair_slave_items` implements it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSRepairSlaveItemsPacket {
    pub npc_id: u32,
}

impl CSRepairSlaveItemsPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            npc_id: s.read_bc()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(character) = active(conn) else {
            return;
        };
        tracing::debug!("RepairSlaveItems, NpcId: {}", self.npc_id);
        crate::systems::vehicles::slaves::repair_slave_items(server, &character, self.npc_id);
    }
}
sync_client_packet!(CSRepairSlaveItemsPacket);

pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSChangeMateEquipmentPacket>(LEVEL_PLAIN, cs::CSChangeMateEquipmentPacket);
    reg.register::<CSRepairPetItemsPacket>(LEVEL_PLAIN, cs::CSRepairPetItemsPacket);
    reg.register::<CSRepairSlaveItemsPacket>(LEVEL_PLAIN, cs::CSRepairSlaveItemsPacket);
}
