use std::sync::Arc;

use crate::data::items::TemplateKind;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::{Item, ItemClass};
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::skills::data::{EffectKind, EffectTemplate};
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{DoodadRef, NpcRef, PlayerRef, WorldObject};

use super::super::inventory::{send_error_message_to, Inv, ItemTxn};
use super::super::item_effects::SkillItem;
use super::super::loot::{GeneratedLoot, SharedRandom};
use super::super::{error_message, inventory, item_effects};
use super::{lock, move_to_system_container};

// ---------------------------------------------------------------------------------------------
// npcs: EconomyBridge
// ---------------------------------------------------------------------------------------------

/// The npcs subsystem's view of items, loot, money and coffers (`EconomyBridge`).
pub struct EconomyForNpcs;

impl crate::systems::npcs::hooks::EconomyBridge for EconomyForNpcs {
    fn on_npc_loot(&self, server: &GameServer, npc: &NpcRef, killer: Option<&PlayerRef>) -> bool {
        let owner = WorldObject::from(npc);
        let killer = killer.map(WorldObject::from);
        super::looting::LootManager::generate_loot(server, &owner, killer.as_ref());
        server.systems.economy.loot.item_count(npc.obj_id) > 0
    }

    fn forget_loot(&self, server: &GameServer, obj_id: u32) {
        server.systems.economy.loot.forget(obj_id);
    }

    fn item_template_and_ucc(&self, server: &GameServer, item_id: u64) -> Option<(u32, u64)> {
        server
            .items
            .item_by_id(item_id)
            .map(|i| (i.template_id, i.ucc_id))
    }

    fn acquire_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        template_id: u32,
        count: i32,
        grade: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        inventory::try_add_new_item(
            &mut txn,
            &Inv::of(player),
            task,
            template_id,
            count,
            grade,
            0,
        )
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
        inventory::consume_item(
            &mut txn,
            &Inv::of(player),
            None,
            task,
            template_id,
            count,
            None,
        )
    }

    fn item_count(&self, _server: &GameServer, player: &PlayerRef, template_id: u32) -> i32 {
        Inv::of(player).items_count_unlocked(template_id, -1)
    }

    fn give_loot_pack(&self, server: &GameServer, player: &PlayerRef, loot_pack_id: u32) -> bool {
        let Some(pack) = server.game_data.systems.economy.loot.pack(loot_pack_id) else {
            return false;
        };
        let mut txn = ItemTxn::begin(server);
        super::loot::give_loot_pack(
            &mut txn,
            player,
            pack,
            item_effects::actability_type::NONE,
            task_type::LOOT_ALL,
            None,
            None,
            &mut SharedRandom,
        )
    }

    fn change_labor(&self, server: &GameServer, player: &PlayerRef, change: i16, actability: i32) {
        crate::systems::skills::progress::change_labor(server, player, change, actability);
    }

    fn change_money(&self, server: &GameServer, player: &PlayerRef, amount: i64, task: u8) -> bool {
        let mut txn = ItemTxn::begin(server);
        let amount = amount.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        if amount < 0 {
            if i64::from(-amount) > player.data().money {
                send_error_message_to(&mut txn.out(), player, error_message::NOT_ENOUGH_MONEY);
                return false;
            }
            inventory::subtract_money(&mut txn.out(), player, slot_type::INVENTORY, -amount, task)
        } else {
            inventory::add_money(&mut txn.out(), player, slot_type::INVENTORY, amount, task)
        }
    }

    fn try_claim_shop_purchase(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        count: i32,
    ) -> bool {
        let Some(limit) = super::shop_limits::purchase_limit(server, template_id) else {
            return true;
        };
        let claim = super::shop_limits::ShopClaim {
            item_id: template_id,
            count,
            limit,
        };
        server
            .systems
            .economy
            .shop_limits
            .try_claim(player.data().account_id, &[claim])
    }

    fn release_shop_purchase(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        count: i32,
    ) {
        let Some(limit) = super::shop_limits::purchase_limit(server, template_id) else {
            return;
        };
        let claim = super::shop_limits::ShopClaim {
            item_id: template_id,
            count,
            limit,
        };
        server
            .systems
            .economy
            .shop_limits
            .release(player.data().account_id, &[claim]);
    }

    fn npc_has_loot_items(&self, server: &GameServer, npc: &NpcRef) -> bool {
        server.systems.economy.loot.item_count(npc.obj_id) > 0
    }

    fn make_loot_public_tick(&self, server: &GameServer, world_id: u32) {
        let Some(world) = server.world.worlds().into_iter().find(|w| w.id == world_id) else {
            return;
        };
        let npcs: Vec<u32> = world
            .all_of_kind(crate::world::ObjectKind::Npc)
            .iter()
            .map(WorldObject::obj_id)
            .collect();
        for id in server.systems.economy.loot.npcs_to_make_public(&npcs) {
            super::looting::LootManager::make_loot_public(server, id);
        }
    }

    fn is_auto_equip_trade_pack(&self, server: &GameServer, template_id: u32) -> bool {
        inventory::is_auto_equip_trade_pack(&server.game_data.items, template_id)
    }

    fn takeoff_backpack(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        gliders_only: bool,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        inventory::take_off_backpack(&mut txn, &Inv::of(player), task, gliders_only)
    }

    fn can_replace_glider_in_backpack_slot(&self, server: &GameServer, player: &PlayerRef) -> bool {
        inventory::can_replace_glider_in_backpack_slot(&server.game_data.items, &Inv::of(player))
    }

    fn create_item_in_backpack_slot(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        template_id: u32,
        count: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let Some(item) = txn.env.create(template_id, count, 0) else {
            return false;
        };
        let data = txn.data();
        let res = lock(&Inv::of(player).equipment()).add_item(
            task,
            item,
            equip_slot::BACKPACK,
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

    fn move_item_to_backpack_slot(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        item_id: u64,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let target = Inv::of(player).equipment();
        super::mail::add_or_move_existing_item(
            &mut txn,
            &target,
            task,
            item_id,
            equip_slot::BACKPACK,
        )
    }

    fn move_item_to_bag(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        item_id: u64,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let target = Inv::of(player).bag();
        super::mail::add_or_move_existing_item(&mut txn, &target, task, item_id, -1)
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

    fn try_store_doodad_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        doodad: &DoodadRef,
        expected: &Item,
        required_count: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        doodad.data().phase.to_next_phase = false;
        let inv = Inv::of(player);
        let equipment = inv.equipment();
        let Some(system) = inv.get(slot_type::SYSTEM) else {
            return false;
        };
        if required_count != 1
            || expected.count != required_count
            || expected.slot_type != slot_type::EQUIPMENT
            || expected.owner_id != u64::from(inv.character_id)
        {
            return false;
        }
        // The rack lock spans the container move and its reference update. No character or
        // world lookup happens while it is held; item effects flush after both locks drop.
        let mut d = doodad.data();
        d.phase.to_next_phase = false;
        if d.phase.deleted || d.item_id != 0 || d.item_template_id != 0 {
            return false;
        }
        let moved = {
            let (mut target, source) = inventory::lock_pair(&system, &equipment);
            let Some(mut source) = source else {
                return false;
            };
            let matches = source.owner_id == inv.character_id
                && source.slot_type == slot_type::EQUIPMENT
                && source.get_item_by_slot(expected.slot).is_some_and(|item| {
                    item.id == expected.id
                        && item.template_id == expected.template_id
                        && item.count == required_count
                        && item.slot_type == slot_type::EQUIPMENT
                        && item.owner_id == u64::from(inv.character_id)
                });
            matches
                && target.move_item_from(
                    &mut source,
                    task_type::DOODAD_ITEM_CHANGER,
                    expected.id,
                    -1,
                    txn.data(),
                    &mut txn.env.fx,
                )
        };
        if moved {
            d.item_id = expected.id;
            d.item_template_id = expected.template_id;
        }
        moved
    }

    fn recover_doodad_cargo(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        doodad: &DoodadRef,
    ) -> crate::systems::npcs::hooks::CargoRecovery {
        use crate::systems::npcs::doodad::funcs::items::cargo_empty_phase;
        use crate::systems::npcs::hooks::CargoRecovery;

        let mut txn = ItemTxn::begin(server);
        doodad.data().phase.to_next_phase = false;
        let inv = Inv::of(player);
        let Some(empty) = cargo_empty_phase(server, doodad) else {
            return CargoRecovery::Failed;
        };
        let Some(can_repair) = cargo_attachment(server, doodad) else {
            return CargoRecovery::Failed;
        };
        let (item_id, template_id, group, attachment) = {
            let d = doodad.data();
            (
                d.item_id,
                d.item_template_id,
                d.func_group_id,
                (
                    d.parent_obj_id,
                    d.transform.parent_obj_id,
                    d.owner_db_id,
                    d.attach_point,
                ),
            )
        };
        if item_id == 0 || !inventory::is_auto_equip_trade_pack(txn.data(), template_id) {
            return CargoRecovery::Failed;
        }
        let item = server.items.item_by_id(item_id);
        let holder = server.items.holding_container(item_id);
        let occurrences: usize = server
            .items
            .all_containers()
            .iter()
            .map(|container| {
                lock(container)
                    .items
                    .iter()
                    .filter(|i| i.id == item_id)
                    .count()
            })
            .sum();
        let outside_escrow = match (&item, &holder) {
            (None, None) => occurrences == 0,
            (Some(item), Some(holder)) if occurrences == 1 => {
                let holder = lock(holder);
                holder.slot_type != slot_type::SYSTEM
                    && item.slot_type == holder.slot_type
                    && item.template_id == template_id
                    && item.class == ItemClass::Backpack
                    && item.count == 1
            }
            _ => false, // Loose or multiply held items do not prove a dangling reference.
        };
        let mut d = doodad.data();
        d.phase.to_next_phase = false;
        if d.phase.deleted
            || (d.item_id, d.item_template_id, d.func_group_id) != (item_id, template_id, group)
            || (
                d.parent_obj_id,
                d.transform.parent_obj_id,
                d.owner_db_id,
                d.attach_point,
            ) != attachment
        {
            return CargoRecovery::Failed;
        }
        if outside_escrow {
            if !can_repair {
                return CargoRecovery::Failed;
            }
            d.item_id = 0;
            d.item_template_id = 0;
            return CargoRecovery::ClearedStale(empty);
        }
        let (Some(item), Some(holder)) = (item, holder) else {
            return CargoRecovery::Failed;
        };
        if occurrences != 1
            || item.slot_type != slot_type::SYSTEM
            || item.class != ItemClass::Backpack
            || item.template_id != template_id
            || item.count != 1
            || lock(&holder).slot_type != slot_type::SYSTEM
        {
            return CargoRecovery::Failed;
        }
        let equipment = inv.equipment();
        if !lock(&equipment).can_accept(
            &item,
            txn.data().template(template_id),
            equip_slot::BACKPACK,
        ) {
            return CargoRecovery::Failed;
        }
        let backpack = inv.equipped(equip_slot::BACKPACK);
        let bag = inv.bag();
        if backpack.as_ref().is_some_and(|backpack| {
            !matches!(txn.data().template(backpack.template_id).map(|t| &t.kind),
                Some(TemplateKind::Backpack { backpack_type })
                    if *backpack_type == crate::data::items::backpack_type::GLIDER)
                || lock(&bag).place(backpack, -1, txn.data()).is_none()
        }) {
            return CargoRecovery::Failed;
        }
        // Preflight every destination before moving the glider. Keep a rollback snapshot for
        // an unexpected failed second move; no failed operation may move the player's backpack.
        let rollback = backpack.as_ref().map(|_| {
            let equipment_before = lock(&equipment).clone();
            let bag_before = lock(&bag).clone();
            (
                equipment_before,
                bag_before,
                server.systems.economy.previous_backpack(inv.character_id),
            )
        });
        if !inventory::take_off_backpack(&mut txn, &inv, task_type::RECOVER_DOODAD_ITEM, true) {
            return CargoRecovery::Failed;
        }
        let moved = {
            let (mut target, source) = inventory::lock_pair(&equipment, &holder);
            source.is_some_and(|mut source| {
                target.move_item_from(
                    &mut source,
                    task_type::RECOVER_DOODAD_ITEM,
                    item_id,
                    equip_slot::BACKPACK,
                    txn.data(),
                    &mut txn.env.fx,
                )
            })
        };
        if !moved {
            if let Some((equipment_before, bag_before, previous)) = rollback {
                *lock(&equipment) = equipment_before;
                *lock(&bag) = bag_before;
                server
                    .systems
                    .economy
                    .restore_previous_backpack(inv.character_id, previous);
                txn.env.fx.0.clear();
            }
            return CargoRecovery::Failed;
        }
        d.item_id = 0;
        d.item_template_id = 0;
        CargoRecovery::Recovered
    }

    fn remove_equipment_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        item_id: u64,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let result = lock(&Inv::of(player).equipment())
            .remove_item_task(task, item_id, true, &mut txn.env)
            .is_some();
        result
    }

    fn bag_space_left_for_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
    ) -> i32 {
        let money = player.data().money;
        lock(&Inv::of(player).bag()).space_left_for_template(
            template_id,
            &server.game_data.items,
            money,
        )
    }

    fn try_consume_doodad_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        count: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let bag = Inv::of(player).bag();
        let mut bag = lock(&bag);
        if count <= 0 || bag.items_by_template_unlocked(template_id, -1).1 < count {
            return false;
        }
        bag.consume_item(
            task_type::DOODAD_INTERACTION,
            template_id,
            count,
            None,
            &mut txn.env,
        ) == count
    }

    fn bag_consume_item(
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

    fn try_equip_new_backpack(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task: u8,
        template_id: u32,
        count: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        inventory::try_equip_new_backpack(
            &mut txn,
            &Inv::of(player),
            task,
            template_id,
            count,
            -1,
            0,
        )
    }

    fn give_doodad_loot_pack(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        loot_pack_id: u32,
        skill_id: u32,
        task: u8,
        roll: &mut Option<Vec<GeneratedLoot>>,
    ) -> Option<bool> {
        let pack = server.game_data.systems.economy.loot.pack(loot_pack_id)?;
        // SkillManager.GetSkillActAbility(skillId)
        let act = server
            .game_data
            .systems
            .skills
            .skill(skill_id)
            .map_or(0, |s| s.actability_group_id.max(0) as u8);
        if roll.is_none() {
            *roll = Some(
                super::loot::generate_pack_for_player(
                    server,
                    pack,
                    player,
                    act,
                    None,
                    &mut SharedRandom,
                )
                .ok()?,
            );
        }
        let mut txn = ItemTxn::begin(server);
        let given = super::loot::give_generated_pack(&mut txn, player, task, roll.as_deref()?);
        if given {
            // GET_LOOTPACK (deferred until the item transaction is released).
            crate::systems::report_achievement(
                server,
                player,
                AchievementInput::Increment {
                    kind_id: char_record_kind::GET_LOOTPACK,
                    amount: 1,
                    matcher: RecordMatch::Value1(loot_pack_id),
                },
            );
        }
        Some(given)
    }

    fn try_sell_equipped_fish(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        backpack_id: u64,
        backpack_template_id: u32,
        buy_func_id: u32,
    ) -> bool {
        super::fishing::sell_equipped_fish(
            server,
            player,
            backpack_id,
            backpack_template_id,
            buy_func_id,
        )
    }

    fn try_convert_equipped_fish(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        backpack_id: u64,
        backpack_template_id: u32,
        convert_func_id: u32,
        task: u8,
    ) -> bool {
        super::fishing::convert_equipped_fish(
            server,
            player,
            backpack_id,
            backpack_template_id,
            convert_func_id,
            task,
        )
    }

    fn give_convert_fish_trophy(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        backpack_template_id: u32,
        task: u8,
    ) {
        let mut rng = SharedRandom;
        let Some(trophies) = super::loot::loot_convert_fish(server, backpack_template_id, &mut rng)
        else {
            return;
        };
        // TODO (C#): use only the first item
        let Some(trophy) = trophies.into_iter().next() else {
            return;
        };
        let mut txn = ItemTxn::begin(server);
        let econ = &server.game_data.systems.economy;
        let Some(fish) = econ.items.create_big_fish(
            &server.game_data.items,
            &trophy,
            server.config.id,
            &mut rng,
            chrono::Utc::now().timestamp(),
        ) else {
            tracing::error!(
                "DoodadFuncConvertFish: no fish details for {} (KeyNotFoundException)",
                trophy.made_unit_id
            );
            return;
        };
        let caught = (fish.details.fish_length, fish.details.fish_weight);
        let data = txn.data();
        let res = lock(&Inv::of(player).bag()).add_item(task, fish, -1, data, &mut txn.env.fx);
        if let Err(item) = res {
            server.items.add_loose(item);
        }
        super::fishing::record_catch(server, player, caught.0, caught.1);
    }

    fn coffer_chest_type(&self, server: &GameServer, container_id: u64) -> u8 {
        server
            .items
            .container_by_id(container_id)
            .map_or(0, |c| lock(&c).coffer_type)
    }

    fn family_of_character(&self, server: &GameServer, character_id: u32) -> u32 {
        server
            .systems
            .social
            .families
            .family_of_character(character_id)
    }

    fn expedition_of_character(&self, server: &GameServer, character_id: u32) -> u32 {
        server.expeditions.expedition_of_character(character_id)
    }

    fn magical_enchant(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        target_item_id: u64,
        rune_template_id: u32,
    ) {
        item_effects::magical_enchant(
            server,
            player,
            Some(SkillItem {
                item_id: 0,
                item_template_id: rune_template_id,
            }),
            Some(target_item_id),
        );
    }

    fn doodad_source_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        item_id: u64,
        bag_only: bool,
    ) -> Option<crate::systems::npcs::hooks::DoodadSourceItem> {
        let inv = Inv::of(player);
        let item = if bag_only {
            lock(&inv.bag()).get_item_by_id(item_id).cloned()
        } else {
            inv.item_by_id(item_id)
        }?;
        if !inventory::item_use_zone_allowed(server, player, item.template_id) {
            return None;
        }
        let t = server.game_data.items.template(item.template_id);
        Some(crate::systems::npcs::hooks::DoodadSourceItem {
            item_id: item.id,
            template_id: item.template_id,
            max_count: t.map_or(1, |t| t.max_count),
            use_skill_id: t.map_or(0, |t| t.use_skill_id),
        })
    }

    fn item_ids_from_doodad(&self, server: &GameServer, doodad_template_id: u32) -> Vec<u32> {
        server
            .game_data
            .items
            .item_ids_from_doodad(doodad_template_id)
            .to_vec()
    }

    fn consume_doodad_source_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        preferred_item_id: Option<u64>,
    ) {
        if let Some(id) = preferred_item_id {
            // character.ItemUse(preferredItem)
            if let Some(item) = server.items.item_by_id(id) {
                crate::systems::quests::events::on_item_use(server, player, item.template_id);
            }
        }
        let mut txn = ItemTxn::begin(server);
        inventory::consume_item(
            &mut txn,
            &Inv::of(player),
            Some(&[slot_type::INVENTORY]),
            task_type::DOODAD_CREATE,
            template_id,
            1,
            preferred_item_id,
        );
    }

    fn initialize_coffer(&self, server: &GameServer, doodad: &DoodadRef, owner_character_id: u32) {
        let c = server
            .items
            .new_coffer_container(owner_character_id, &server.ids.container);
        let mut d = doodad.data();
        let Some(coffer) = d.coffer.as_mut() else {
            return;
        };
        let mut g = lock(&c);
        g.set_container_size(coffer.capacity);
        coffer.container_id = g.container_id;
    }

    fn try_consume_doodad_source_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        doodad_template_id: u32,
        item_id: u64,
        labor_cost: i16,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let inv = Inv::of(player);
        let bag = inv.bag();
        let mut bag = lock(&bag);
        let Some(item) = bag.get_item_by_id(item_id).cloned() else {
            tracing::warn!(
                "doodad {doodad_template_id}: source item {item_id} is not in the bag of character {}",
                player.character_id
            );
            return false;
        };
        let labor_power = player.data().labor_power;
        if item.count < 1
            || labor_cost < 0
            || !server
                .game_data
                .items
                .item_ids_from_doodad(doodad_template_id)
                .contains(&item.template_id)
            || labor_power < labor_cost
        {
            tracing::warn!(
                "doodad {doodad_template_id}: source item {item_id} (template {}, count {}) rejected: labor cost {labor_cost}, labor power {labor_power}",
                item.template_id,
                item.count
            );
            return false;
        }
        let preserve = server
            .game_data
            .items
            .template(item.template_id)
            .is_some_and(|t| t.max_count == 1);
        if preserve {
            if item.count != 1 {
                tracing::warn!(
                    "doodad {doodad_template_id}: unique source item {item_id} has count {}",
                    item.count
                );
                return false;
            }
            drop(bag);
            let Some(system) = inv.get(slot_type::SYSTEM) else {
                tracing::warn!(
                    "doodad {doodad_template_id}: character {} has no system container",
                    player.character_id
                );
                return false;
            };
            if !super::mail::add_or_move_existing_item(
                &mut txn,
                &system,
                task_type::DOODAD_CREATE,
                item_id,
                -1,
            ) {
                tracing::warn!(
                    "doodad {doodad_template_id}: could not move source item {item_id} to the system container"
                );
                return false;
            }
        } else {
            if bag.consume_item(
                task_type::DOODAD_CREATE,
                item.template_id,
                1,
                Some(item_id),
                &mut txn.env,
            ) != 1
            {
                tracing::warn!(
                    "doodad {doodad_template_id}: could not consume source item {item_id} (template {})",
                    item.template_id
                );
                return false;
            }
            drop(bag);
        }
        if labor_cost > 0 {
            inventory::change_labor(&mut txn.out(), server, player, -labor_cost, 0);
        }
        drop(txn);
        crate::systems::quests::events::on_item_use(server, player, item.template_id);
        true
    }

    fn remove_expired_doodad_item(
        &self,
        server: &GameServer,
        source: crate::systems::npcs::hooks::DoodadItemCleanup,
    ) {
        let mut txn = ItemTxn::begin(server);
        cleanup_doodad_escrow_item(&mut txn, source);
    }

    fn release_deleted_doodad_items(
        &self,
        server: &GameServer,
        source: crate::systems::npcs::hooks::DoodadItemCleanup,
    ) {
        let mut txn = ItemTxn::begin(server);
        cleanup_doodad_escrow_item(&mut txn, source);
        if source.container_id != 0 {
            // A coffer's container has no immutable doodad/generation association. Even an
            // empty container with a matching owner may be a reused allocation; retain it.
            tracing::warn!(?source, "Retained doodad container: allocation provenance unavailable; reconciliation deferred");
        }
    }
}

/// Whether `template_id` is the doodad a trade pack turns into when it is put down or dropped on
/// death (`PutDownBackpackEffect.BackpackDoodadId`). Its item sits in the owner's System
/// container only for as long as the doodad stands, and the doodad's expiry
/// (`DoodadFuncFinal.After`, 6 days for most packs) is what ends the pack's life.
fn is_put_down_backpack_doodad(server: &GameServer, template_id: u32) -> bool {
    server
        .game_data
        .systems
        .skills
        .effects
        .get(&EffectKind::PutDownBackpackEffect)
        .is_some_and(|effects| {
            effects.values().any(|e| {
                matches!(e, EffectTemplate::PutDownBackpack { backpack_doodad_id, .. }
                    if *backpack_doodad_id == template_id)
            })
        })
}

/// Another live doodad that still references `item_id` (a duplicate of the expired doodad).
fn item_referenced_by_live_doodad(server: &GameServer, item_id: u64) -> bool {
    server.world.worlds().iter().any(|world| {
        world.all_doodads().iter().any(|d| {
            let d = d.data();
            !d.phase.deleted && d.item_id == item_id
        })
    })
}

/// Original rack references cannot authorize destruction after GUID reuse, regardless of the
/// current item's class. Other doodads may remove only a unique, matching singleton in their
/// original character owner's System escrow. Bag/equipment/bank, loose, foreign and mismatched
/// items are retained. These checks establish escrow authority, not an allocation generation.
/// Cargo/ambiguous item references and all saved container references still need reconciliation;
/// these synchronous hooks do not prove absence of unloaded aliases or collect those orphans.
///
/// Exception: a put-down / dropped trade pack. Its doodad is the pack's only reference (the
/// escrow check below still has to hold), so when it expires the pack goes with it, as in the C#
/// (`Doodad.Delete`: "Delete associated item if expired"). Retaining it left every expired
/// pack behind in its owner's System container for good.
fn cleanup_doodad_escrow_item(
    txn: &mut ItemTxn<'_>,
    source: crate::systems::npcs::hooks::DoodadItemCleanup,
) {
    use crate::models::doodad::doodad_owner_type;

    let server = txn.server;
    if source.item_id == 0 {
        return;
    }
    let doodads = &server.game_data.systems.npcs.doodad;
    let rack = doodads
        .doodad_func_groups(source.doodad_template_id)
        .iter()
        .any(|group| {
            doodads
                .funcs_for_group(group.id)
                .iter()
                .any(|f| f.func_type == "DoodadFuncConsumeChanger")
        });
    let template = server.game_data.items.template(source.item_template_id);
    let cargo = match template.map(|t| &t.kind) {
        Some(TemplateKind::Backpack { backpack_type }) => !matches!(
            *backpack_type,
            crate::data::items::backpack_type::GLIDER | crate::data::items::backpack_type::TOY_FLAG
        ),
        _ => false,
    };
    let dropped_pack = !rack
        && source.owner_type != doodad_owner_type::SLAVE
        && is_put_down_backpack_doodad(server, source.doodad_template_id)
        && !item_referenced_by_live_doodad(server, source.item_id);
    if rack
        || (cargo && !dropped_pack)
        || source.owner_type == doodad_owner_type::SLAVE
        || !matches!(
            source.owner_type,
            doodad_owner_type::CHARACTER | doodad_owner_type::HOUSING
        )
        || source.owner_id == 0
        || source.item_template_id == 0
        || !doodads.templates.contains_key(&source.doodad_template_id)
        || !template.is_some_and(|t| t.max_count == 1)
    {
        tracing::warn!(?source, "Retained doodad item: original escrow authority is ambiguous or cargo-origin; reconciliation deferred");
        return;
    }
    let copies: usize = server
        .items
        .all_containers()
        .iter()
        .map(|c| {
            lock(c)
                .items
                .iter()
                .filter(|i| i.id == source.item_id)
                .count()
        })
        .sum();
    if copies != 1 {
        return;
    }
    let Some(container) = server.items.holding_container(source.item_id) else {
        return;
    };
    let mut container = lock(&container);
    let matches = container.slot_type == slot_type::SYSTEM
        && container.owner_id == source.owner_id
        && container
            .get_item_by_id(source.item_id)
            .is_some_and(|item| {
                item.slot_type == slot_type::SYSTEM
                    && item.owner_id == u64::from(source.owner_id)
                    && item.template_id == source.item_template_id
                    && item.count == 1
            });
    if matches {
        container.remove_item_task(task_type::INVALID, source.item_id, true, &mut txn.env);
    } else {
        tracing::warn!(
            ?source,
            actual_container_id = container.container_id,
            actual_container_owner = container.owner_id,
            actual_slot_type = container.slot_type,
            "Retained doodad item: current holder does not match original owned System escrow"
        );
    }
}

/// A live cargo attachment, plus whether its persistent authored binding proves it safe to
/// repair. Nonpersistent or equipment-created racks can recover normally but are not repaired.
fn cargo_attachment(server: &GameServer, rack: &DoodadRef) -> Option<bool> {
    let (instance, parent, transform_parent, hull_id, point, template_id, persistent) = {
        let d = rack.data();
        (
            d.transform.instance_id,
            d.parent_obj_id,
            d.transform.parent_obj_id,
            d.owner_db_id,
            d.attach_point,
            d.template_id,
            d.is_persistent && !d.phase.deleted,
        )
    };
    if parent == 0 || parent != transform_parent {
        return None;
    }
    let world = server.world.world(instance)?;
    if !world
        .doodad(rack.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, rack))
    {
        return None;
    }
    let hull = world.slave(parent)?;
    let (id, hull_template, attached) = {
        let hull = hull.data();
        (
            hull.id,
            hull.unit.template_id,
            hull.attached_doodads.contains(&rack.obj_id),
        )
    };
    if id != hull_id || !attached {
        return None;
    }
    let repairable = persistent
        && hull_id > 0
        && server
            .game_data
            .systems
            .vehicles
            .slave_template(hull_template)
            .is_some_and(|t| {
                t.doodad_bindings
                    .iter()
                    .any(|b| b.persist && b.attach_point_id == point && b.doodad_id == template_id)
            });
    Some(
        repairable
            && !rack.core().children().iter().any(|child| {
                world.object(child.obj_id()).is_some_and(|live| {
                    // ObjectCore belongs to the immutable Arc allocation, not its recycled id.
                    std::ptr::eq(child.core(), live.core()) && {
                        let transform = live.transform();
                        transform.parent_obj_id == rack.obj_id && transform.instance_id == instance
                    }
                })
            }),
    )
}
