use crate::models::container::ContainerRef;
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemFx, ItemTask};
use crate::server::GameServer;
use crate::systems::skills::geometry::{instance_and_zone, world_pos, Target};
use crate::world::PlayerRef;

use super::super::inventory::{send_error_message_to, Inv, ItemTxn};
use super::super::item_effect_packets::SCItemUccDataChangedPacket;
use super::super::{error_message, inventory, loot};
use super::LootRng;
use super::{actability_type, lock, task_packet, Outcome, SkillItem};

// ---------------------------------------------------------------------------------------------
// Conversion / gain item / loot packs
// ---------------------------------------------------------------------------------------------

/// `ItemConversion.Execute` (disenchanting a bag item into its conversion product).
pub fn item_conversion(
    server: &GameServer,
    player: &PlayerRef,
    target_item_id: Option<u64>,
    values: [i32; 4],
    rng: &mut dyn LootRng,
) -> Outcome {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: ItemConversion value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let Some(target_id) = target_item_id else {
        return Outcome::Cancelled;
    };
    let data = &server.game_data.items;
    let conv = &server.game_data.systems.economy.items.conversion;
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let bag = inv.bag();
    let Some(target) = lock(&bag).get_item_by_id(target_id).cloned() else {
        return Outcome::Cancelled;
    };
    let Some(template) = data.template(target.template_id) else {
        return Outcome::Cancelled;
    };
    if super::item_security::refuse_if_locked(player, &target) {
        return Outcome::Cancelled;
    }
    if !template.disenchantable {
        return Outcome::Cancelled;
    }
    let id = target.template_id;
    let Some(reagent) = conv.reagent_for_item(target.grade, template.impl_id, id, template.level)
    else {
        tracing::error!("Couldn't find Reagent for item {id}");
        return Outcome::Cancelled;
    };
    let Some(product) = conv.product_from_reagent(reagent) else {
        tracing::error!("Couldn't find Product from Reagent for item {id}");
        return Outcome::Cancelled;
    };
    let product_roll = rng.next_i32(0, 10000);
    if product_roll < product.chance_rate {
        // give product (TODO in C#: weights)
        let value = rng.next_i32(product.min_output, product.max_output.wrapping_add(1));
        let ok = lock(&bag).acquire_default_item(
            task_type::CONVERSION,
            product.output_item_id,
            value,
            -1,
            0,
            &mut txn.env,
        );
        if !ok {
            send_error_message_to(&mut txn.out(), player, error_message::BAG_FULL);
            return Outcome::Cancelled;
        }
    }
    // consumes target item from stack or if there is only 1, destroy item
    lock(&bag).consume_item(
        task_type::CONVERSION,
        target.template_id,
        1,
        Some(target.id),
        &mut txn.env,
    );
    Outcome::Done
}

/// `GainItem.Execute` (`value1` = item template, `value2` = count).
pub fn gain_item(server: &GameServer, player: &PlayerRef, values: [i32; 4]) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: GainItem value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    if value1 <= 0 {
        tracing::warn!("Special effects: GainItem ignored invalid item template id {value1}");
        return;
    }
    let item_id = value1 as u32;
    let count = if value2 > 0 { value2 } else { 1 };
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let acquired = inventory::try_add_new_item(
        &mut txn,
        &inv,
        task_type::SKILL_EFFECT_GAIN_ITEM,
        item_id,
        count,
        0,
        0,
    );
    if !acquired {
        tracing::warn!(
            "Special effects: GainItem failed to give item {item_id} x{count} to character {}",
            player.character_id
        );
        // Deviation from the C# (log only): a valid item that did not fit tells the client.
        if txn.data().template(item_id).is_some() {
            send_error_message_to(&mut txn.out(), player, error_message::BAG_FULL);
        }
    }
}

/// `GainItemWithPosImprint.Execute` (special 32). The C# body is a `// TODO` stub; the data
/// gives the behaviour: `special_effects` 405 (`value1` 5054, template not in 1.2 data), 546
/// (8120 Guide Map), 547 (8122 Guide Gate) and 548 (8124 Guide Compass) each create `value1` x1
/// for the player caster with `ItemTaskType.SkillEffectGainItemWithPos`. The tools that grant
/// them (5053 / 8119 / 8121 / 8123, skills 11402 / 11635 / 11637 / 11638) are consumed as skill
/// reagents. The position imprint has no 1.2 item detail type, so the item is created without
/// one (see `PORTING.md`).
pub fn gain_item_with_pos_imprint(server: &GameServer, player: &PlayerRef, values: [i32; 4]) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: GainItemWithPosImprint value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    if value1 <= 0 {
        tracing::warn!(
            "Special effects: GainItemWithPosImprint ignored invalid item template id {value1}"
        );
        return;
    }
    let item_id = value1 as u32;
    let mut txn = ItemTxn::begin(server);
    if txn.data().template(item_id).is_none() {
        tracing::warn!(
            "Special effects: GainItemWithPosImprint ignored unknown item template {item_id}"
        );
        return;
    }
    let inv = Inv::of(player);
    let acquired = inventory::try_add_new_item(
        &mut txn,
        &inv,
        task_type::SKILL_EFFECT_GAIN_ITEM_WITH_POS,
        item_id,
        1,
        0,
        0,
    );
    if !acquired {
        tracing::warn!(
            "Special effects: GainItemWithPosImprint failed to give item {item_id} to character {}",
            player.character_id
        );
    }
}

/// `ExpToItem.Execute` (special 96). The C# body is a `// TODO` stub; 1.2 data row 12462 has
/// `value1 = 10000` (experience spent) and `value2 = 28329` ("Timeglass Bottle"), cast from the
/// consumed tool item 28328 ("Empty Timeglass Bottle", skill 22580). The experience is deducted
/// directly (not through `AddExp`, which recomputes the level and could level the character
/// down) and reported with `SCExpChangedPacket`. `Outcome::Cancelled` leaves the reagent
/// unconsumed when the character lacks the experience or the bag cannot hold the product.
pub fn exp_to_item(server: &GameServer, player: &PlayerRef, values: [i32; 4]) -> Outcome {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: ExpToItem value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    if value1 <= 0 || value2 <= 0 {
        tracing::warn!("Special effects: ExpToItem ignored invalid values {value1} / {value2}");
        return Outcome::Cancelled;
    }
    let item_id = value2 as u32;
    if server.game_data.items.template(item_id).is_none() {
        tracing::warn!("Special effects: ExpToItem ignored unknown item template {item_id}");
        return Outcome::Cancelled;
    }
    let inv = Inv::of(player);
    // Check the room before the experience is spent; otherwise the product would be lost.
    if !bag_can_accept_item(&server.game_data.items, &inv, item_id, 1) {
        let mut txn = ItemTxn::begin(server);
        send_error_message_to(&mut txn.out(), player, error_message::BAG_FULL);
        return Outcome::Cancelled;
    }
    let exp_cost = value1;
    if player.data().experience < exp_cost {
        return Outcome::Cancelled;
    }
    player.data().experience -= exp_cost;
    player.send(crate::systems::skills::packets::SCExpChangedPacket {
        obj_id: player.obj_id,
        exp: -exp_cost,
        should_add_ability_exp: false,
    });
    let mut txn = ItemTxn::begin(server);
    let acquired = inventory::try_add_new_item(
        &mut txn,
        &inv,
        task_type::SKILL_EFFECT_EXP_TO_ITEM,
        item_id,
        1,
        0,
        0,
    );
    if !acquired {
        tracing::warn!(
            "Special effects: ExpToItem failed to give item {item_id} to character {}",
            player.character_id
        );
    }
    Outcome::Done
}

/// Whether the bag can take `count` units of `template_id` (`AcquireDefaultItemEx`'s free-space
/// calculation: partial stacks plus free slots).
fn bag_can_accept_item(
    data: &crate::data::items::ItemData,
    inv: &Inv,
    template_id: u32,
    count: i32,
) -> bool {
    let Some(template) = data.template(template_id) else {
        return false;
    };
    if count <= 0 {
        return true;
    }
    let max = template.max_count;
    if max <= 0 {
        return false;
    }
    let bag = inv.bag();
    let guard = bag.lock().expect("container poisoned");
    let (stacks, units) = guard.items_by_template(template_id, -1);
    let free = stacks.len() as i32 * max - units + guard.free_slot_count() * max;
    count <= free
}

/// `GainItemWithEmblemImprint.Execute`: `UccManager.CreateStamp(target, sourceItem)` where
/// the source is the casting item (`ItemManager.GetItemByItemId`) and `target` must be a
/// character.
pub fn gain_item_with_emblem_imprint(
    server: &GameServer,
    target: Option<&PlayerRef>,
    source: Option<SkillItem>,
    source_item: Option<&Item>,
    values: [i32; 4],
) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: GainItemWithEmblemImprint value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let Some(source) = source else {
        tracing::error!("GainItemWithEmblemImprint: casterObj is not a SkillItem"); // InvalidCast
        return;
    };
    let source_item = match source_item {
        Some(item) => item.clone(),
        None => {
            let Some(item) = server.items.item_by_id(source.item_id) else {
                return;
            };
            item
        }
    };
    if let Some(player) = target {
        create_stamp(server, player, &source_item);
    }
}

/// `UccManager.CreateStamp(player, sourceInk)`: a new crest stamp with the ink's UCC.
pub fn create_stamp(server: &GameServer, player: &PlayerRef, source_ink: &Item) {
    let mut txn = ItemTxn::begin(server);
    let Some(mut item) = txn.env.create(crate::data::items::ITEM_CREST_STAMP, 1, 0) else {
        tracing::error!("CreateStamp: could not create a crest stamp"); // NRE in C#
        return;
    };
    item.set_ucc(source_ink.ucc_id);
    let bag = Inv::of(player).bag();
    let data = txn.data();
    let res = lock(&bag).add_item(
        task_type::GAIN_ITEM_WITH_UCC,
        item,
        -1,
        data,
        &mut txn.env.fx,
    );
    if let Err(item) = res {
        // The new item stays in the C# `_allItems`.
        txn.env.fx.push(ItemFx::Orphaned(Box::new(item)));
    }
}

/// `UccManager.ApplyStamp(stamp, targetItem)`.
pub fn apply_stamp(stamp: &Item, target: &mut Item) {
    target.set_ucc(stamp.ucc_id);
}

/// Resolve both UCC operands, preferring the cast's pre-debit `source_item` snapshot over the
/// live (already debited) source. The caller holds an ItemTxn.
fn ucc_operands(
    player: &PlayerRef,
    source: SkillItem,
    source_item: Option<&Item>,
    target_id: u64,
) -> Option<(Item, ContainerRef)> {
    if let Some(item) = source_item {
        if source.item_id == target_id
            || item.id != source.item_id
            || item.template_id != source.item_template_id
            || item.count <= 0
        {
            return None;
        }
        let inv = Inv::of(player);
        let holder = inv.holder_of(target_id)?;
        let valid_target = lock(&holder)
            .get_item_by_id(target_id)
            .is_some_and(|target| {
                target.owner_id == u64::from(player.character_id) && target.count > 0
            });
        return valid_target.then_some((item.clone(), holder));
    }
    owned_ucc_items(player, source, target_id)
}

/// Resolve both UCC operands in the caster's inventory while the caller holds an ItemTxn.
/// Loose items and other characters' containers cannot be addressed by an item skill.
fn owned_ucc_items(
    player: &PlayerRef,
    source: SkillItem,
    target_id: u64,
) -> Option<(Item, ContainerRef)> {
    if source.item_id == target_id {
        return None;
    }
    let inv = Inv::of(player);
    let source_item = inv.item_by_id(source.item_id)?;
    if source_item.owner_id != u64::from(player.character_id)
        || source_item.template_id != source.item_template_id
        || source_item.count <= 0
    {
        return None;
    }
    let holder = inv.holder_of(target_id)?;
    let valid_target = lock(&holder)
        .get_item_by_id(target_id)
        .is_some_and(|item| item.owner_id == u64::from(player.character_id) && item.count > 0);
    valid_target.then_some((source_item, holder))
}

/// `ImprintUccEffect.Apply` (stamp a UCC onto an item). The caller supplies the caster and
/// cancels rejected effects before the skill's subsequent reagent consumption.
///
/// WP-U2: the target template must be a `ucc_applicables` kind-2 item and the stamp UCC's
/// non-zero patterns must be `emblem_patterns` ids. Both checks are permissive when the table
/// has no rows (the pre-WP-U2 behaviour); a UCC id with no loaded `uccs` row carries no known
/// patterns and is accepted. A refusal is the existing `Outcome::Cancelled` path, so the stamp
/// and the target keep their UCCs and the skill keeps its reagent.
pub fn imprint_ucc(
    server: &GameServer,
    target: Option<&PlayerRef>,
    stamp: Option<SkillItem>,
    stamp_item: Option<&Item>,
    target_item_id: Option<u64>,
) -> Outcome {
    tracing::trace!("ImprintUccEffect");
    let (Some(player), Some(stamp), Some(target_id)) = (target, stamp, target_item_id) else {
        return Outcome::Cancelled;
    };
    let mut txn = ItemTxn::begin(server);
    let Some((stamp_item, holder)) = ucc_operands(player, stamp, stamp_item, target_id) else {
        return Outcome::Cancelled;
    };
    let ucc_data = &server.game_data.systems.economy.ucc;
    if let Some(ucc) = server.streams.ucc.get_ucc(stamp_item.ucc_id) {
        for (field, pattern) in [("pattern1", ucc.pattern1), ("pattern2", ucc.pattern2)] {
            if !ucc_data.pattern_exists(pattern) {
                tracing::warn!(
                    "ImprintUccEffect refused: UCC {} {field} {pattern} is not an emblem_patterns id",
                    ucc.id
                );
                return Outcome::Cancelled;
            }
        }
    }
    let target_item = {
        let mut container = lock(&holder);
        let Some(target_item) = container.get_item_by_id_mut(target_id) else {
            return Outcome::Cancelled;
        };
        if !ucc_data.accepts_imprint(target_item.template_id) {
            tracing::warn!(
                "ImprintUccEffect refused: item template {} does not accept a UCC imprint",
                target_item.template_id
            );
            return Outcome::Cancelled;
        }
        apply_stamp(&stamp_item, target_item);
        target_item.clone()
    };
    txn.out().send(
        player,
        SCItemUccDataChangedPacket {
            ucc_id: stamp_item.ucc_id,
            player_id: player.character_id,
            target_item_id: target_item.id,
        },
    );
    txn.out().send(
        player,
        task_packet(
            task_type::GAIN_ITEM_WITH_UCC,
            vec![ItemTask::update_bits(&target_item)],
        ),
    );
    // Retail consumes the stamp with QuestRemoveSupplies; the C# leaves it to the skill.
    Outcome::Done
}

/// `CleanupUccEffect.Apply` (bleach: removes the UCC of an item).
pub fn cleanup_ucc(
    server: &GameServer,
    target: Option<&PlayerRef>,
    bleach: Option<SkillItem>,
    bleach_item: Option<&Item>,
    target_item_id: Option<u64>,
) -> Outcome {
    tracing::trace!("CleanupUccEffect");
    let (Some(player), Some(bleach), Some(target_id)) = (target, bleach, target_item_id) else {
        return Outcome::Cancelled;
    };
    let mut txn = ItemTxn::begin(server);
    let Some((_, holder)) = ucc_operands(player, bleach, bleach_item, target_id) else {
        return Outcome::Cancelled;
    };
    let target_item = {
        let mut container = lock(&holder);
        let Some(target_item) = container.get_item_by_id_mut(target_id) else {
            return Outcome::Cancelled;
        };
        target_item.set_ucc(0);
        target_item.clone()
    };
    txn.out().send(
        player,
        SCItemUccDataChangedPacket {
            ucc_id: 0,
            player_id: player.character_id,
            target_item_id: target_item.id,
        },
    );
    txn.out().send(
        player,
        task_packet(
            task_type::GAIN_ITEM_WITH_UCC,
            vec![ItemTask::update_bits(&target_item)],
        ),
    );
    Outcome::Done
}

/// `FishingLoot.Execute`: the fishing loot pack of the target's zone group (land pack above
/// z 101, sea pack otherwise). The target is usually a position (fishing is a `Pos` skill).
pub fn fishing_loot(
    server: &GameServer,
    player: &PlayerRef,
    target: &Target,
    values: [i32; 4],
    rng: &mut dyn LootRng,
) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: FishingLoot value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let zone_key = instance_and_zone(target).1;
    let Some(zone) = server.game_data.zones.zone_by_key(zone_key) else {
        tracing::error!("FishingLoot: unknown zone {zone_key}"); // NRE in C#
        return;
    };
    let packs = server
        .game_data
        .systems
        .economy
        .item_effects
        .fishing_loot_packs
        .get(&zone.group_id)
        .copied();
    let Some(packs) = packs else {
        tracing::warn!(
            "{} seems to be trying to fish out of bounds.",
            player.data().unit.name
        );
        return;
    };
    let z = world_pos(server, target).z;
    let loot_table_id = if z > 101.0 { packs.land } else { packs.sea };
    let Some(pack) = server.game_data.systems.economy.loot.pack(loot_table_id) else {
        return;
    };
    if pack.loots.is_empty() {
        return;
    }
    let mut txn = ItemTxn::begin(server);
    if !loot::give_loot_pack(
        &mut txn,
        player,
        pack,
        actability_type::FISHING,
        task_type::SKILL_EFFECT_GAIN_ITEM,
        None,
        None,
        rng,
    ) {
        send_error_message_to(&mut txn.out(), player, error_message::BAG_FULL);
    }
}

/// `GainLootPackItemEffect` template fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GainLootPackItemEffect {
    pub loot_pack_id: u32,
    pub consume_source_item: bool,
    pub consume_item_id: u32,
    pub consume_count: i32,
    pub inherit_grade: bool,
}

/// `GainLootPackItemEffect.Apply`.
pub fn gain_loot_pack_item(
    server: &GameServer,
    player: &PlayerRef,
    effect: &GainLootPackItemEffect,
    source: Option<SkillItem>,
    source_item: Option<&Item>,
    source_debited: bool,
    rng: &mut dyn LootRng,
) {
    let Some(pack) = server
        .game_data
        .systems
        .economy
        .loot
        .pack(effect.loot_pack_id)
    else {
        return;
    };
    if pack.loots.is_empty() {
        return;
    }
    tracing::debug!("GainLootPackItemEffect {}", effect.loot_pack_id);
    // TODO (C#): Find the related ActAbility (items-crafting#9). The 1.2 client only parses
    // the `GainLootPackItemEffect` descriptor (`FUN_396e91c0`), so which ActAbility supplies
    // the inherited grade is not derivable from it: capture-pending **ITEM-LOOT-GRADE**. The
    // cast's source item grade stays the only input.
    let mut inherited_grade = None;
    let mut txn = ItemTxn::begin(server);
    let bag = Inv::of(player).bag();
    if !effect.consume_source_item && effect.consume_item_id > 0 && effect.consume_count > 0 {
        // the tractor collects water
        lock(&bag).consume_item(
            task_type::CONSUME_SKILL_SOURCE,
            effect.consume_item_id,
            effect.consume_count,
            None,
            &mut txn.env,
        );
    }
    if effect.consume_source_item {
        let Some(source) = source else { return };
        // The skill debited the source item before this effect, so only its captured data is
        // available; consuming it again would hit the bag it was already taken out of.
        let Some(source_item) = source_item else {
            tracing::warn!(
                "Invalid loot result items {} in lootpack {}",
                source.item_id,
                effect.loot_pack_id
            );
            return;
        };
        if effect.inherit_grade {
            inherited_grade = Some(source_item.grade);
        }
        if !source_debited {
            lock(&bag).remove_item_task(
                task_type::CONSUME_SKILL_SOURCE,
                source_item.id,
                true,
                &mut txn.env,
            );
        }
    }
    // Give the results
    if !loot::give_loot_pack(
        &mut txn,
        player,
        pack,
        actability_type::NONE,
        task_type::SKILL_EFFECT_GAIN_ITEM,
        None,
        inherited_grade,
        rng,
    ) {
        send_error_message_to(&mut txn.out(), player, error_message::LOOT_ITEM_CREATE);
    }
}

/// `ConversionEffect.Apply` (only traces in C#).
pub fn conversion_effect() {
    tracing::trace!("ConversionEffect");
}
