//! Port of `Models/Game/Char/CharacterCraft` (`Craft` / `EndCraft` / `CancelCraft`,
//! `FreeRegrade`), `Models/Tasks/Skills/CraftTask`, `CSExecuteCraft`, `CSSetCraftingPayPacket`
//! and `SCCraftFailedPacket`.
//!
//! A craft is a skill cast on the crafting doodad: [`craft`] validates the craft and asks the
//! skills subsystem to cast the craft skill ([`CraftHooks::start_craft_cast`]); when the cast's
//! `CraftEffect` applies, the skills subsystem calls [`end_craft`], which hands out the products,
//! consumes the materials and schedules the next craft of the batch. Labor is not consumed
//! here: `Skill.EndSkill` (skills subsystem) charges the skill's `ConsumeLaborPower` through
//! `inventory::change_labor`, exactly like the C#.
//!
//! `CharacterCraft`'s per-character state lives in [`CraftManager`] (keyed by character id).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aaemu_commons::{Marshal, PacketStream};

use crate::error::Result;
use crate::models::container::slot_type;
use crate::models::item_task::task_type;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::{cs, sc};
use crate::packets::packet_def;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::skills::types::skill_result;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::systems::HookSlot;
use crate::world::{DoodadRef, PlayerRef, WorldObject};

use super::craft_data::Craft;
#[cfg(test)]
#[path = "craft_publication_tests.rs"]
mod publication_tests;
use super::error_message;
use super::inventory::{self, Inv, ItemTxn, Out};
use super::item_handlers::{active, sync_client_packet};

#[cfg(test)]
#[path = "bench_tests.rs"]
mod bench_tests;

/// `DoodadFuncPermission`.
pub mod doodad_func_permission {
    pub const ANY: u8 = 0;
    pub const PERMISSION1: u8 = 1;
    pub const PERMISSION2: u8 = 2;
    pub const OWNER_ONLY: u8 = 3;
    pub const PERMISSION4: u8 = 4;
    pub const OWNER_RAID_MEMBERS: u8 = 5;
    pub const SAME_ACCOUNT: u8 = 6;
    pub const ZONE_RESIDENTS: u8 = 8;
}

/// What crafting needs from other subsystems. Station authorization defaults to the loaded
/// NPC/craft data; the skill, housing bonus and quest hooks are installed at startup.
pub trait CraftHooks: Send + Sync {
    /// skills: `new Skill(SkillManager.GetSkillTemplate(skillId)) { CastTimeMultiplier }
    /// .Use(owner, SkillCasterUnit(owner.ObjId), SkillCastDoodadTarget(doodadObjId), null,
    /// false)`. When the cast's `CraftEffect` applies, the skills subsystem calls
    /// [`end_craft`]. Returns the `Skill.Use` result (`skill_result::SUCCESS` when the cast
    /// started); the C# ignored it, which left the client waiting for a craft that never came.
    /// Default: nothing is cast (the craft never completes), reported as a failure.
    fn start_craft_cast(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        _skill_id: u32,
        _doodad_obj_id: u32,
        _cast_time_multiplier: f32,
    ) -> u8 {
        tracing::warn!("CraftHooks::start_craft_cast is not installed (skills subsystem)");
        skill_result::FAILURE
    }

    /// skills: `if (Owner.SkillTask != null) Owner.SkillTask.Skill.Cancelled = true;
    /// Owner.InterruptSkills();`.
    fn cancel_craft_skill(&self, _server: &GameServer, _player: &PlayerRef) {}

    /// npcs: permission of the current crafting function whose authored list contains the
    /// recipe. Missing membership denies access, including at the canonical req_doodad_id.
    fn doodad_craft_permission(
        &self,
        server: &GameServer,
        doodad: &DoodadRef,
        craft_id: u32,
    ) -> Option<u8> {
        crate::systems::npcs::bridges::doodad_craft_permission(server, doodad, craft_id)
    }

    /// housing: `HousingManager.GetByAccountId(accountId)` -> the `Transform.ZoneId` of every
    /// house of the account.
    fn account_house_zone_ids(&self, _server: &GameServer, _account_id: u32) -> Vec<u32> {
        Vec::new()
    }

    /// housing: `HousingManager.GetHouseAtLocation(x, y)` +
    /// `GetActAbilityBonusFromHouse(actabilityGroupId, house)` (0 without a house).
    fn house_actability_bonus(
        &self,
        _server: &GameServer,
        _x: f32,
        _y: f32,
        _actability_group_id: i32,
    ) -> u32 {
        0
    }

    /// Housing bonus scoped to the crafting player's world instance.
    fn house_actability_bonus_in_world(
        &self,
        server: &GameServer,
        _instance_id: u32,
        x: f32,
        y: f32,
        actability_group_id: i32,
    ) -> u32 {
        self.house_actability_bonus(server, x, y, actability_group_id)
    }

    /// quests: `QuestManager.DoOnCraftEvents(owner, craftId)`.
    fn on_craft(&self, _server: &GameServer, _player: &PlayerRef, _craft_id: u32) {}
}

/// `CharacterCraft`'s private state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CraftState {
    pub count: i32,
    /// `CurrentCraft?.Id`.
    pub current_craft: Option<u32>,
    /// Crafter doodad obj id.
    pub doodad_id: u32,
    pub consume_labor_power: i32,
    pub is_crafting: bool,
}

/// `CharacterCraft` of every character (`server.systems.economy.craft`).
#[derive(Debug, Default)]
pub struct CraftManager {
    states: Mutex<HashMap<u32, CraftState>>,
    /// Installed by the skills / npcs / housing / quests owners (see [`CraftHooks`]).
    pub hooks: HookSlot<dyn CraftHooks>,
}

struct NoHooks;
impl CraftHooks for NoHooks {}

impl CraftManager {
    /// A copy of `character_id`'s craft state.
    pub fn state(&self, character_id: u32) -> CraftState {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&character_id)
            .copied()
            .unwrap_or_default()
    }

    /// `CharacterCraft.IsCrafting`.
    pub fn is_crafting(&self, character_id: u32) -> bool {
        self.state(character_id).is_crafting
    }

    fn update<R>(&self, character_id: u32, f: impl FnOnce(&mut CraftState) -> R) -> R {
        let mut s = self.states.lock().unwrap_or_else(|e| e.into_inner());
        f(s.entry(character_id).or_default())
    }

    /// Forgets a character's state (the C# object dies with its `Character`).
    pub fn forget(&self, character_id: u32) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&character_id);
    }

    fn hooks(&self) -> Arc<dyn CraftHooks> {
        self.hooks.get().unwrap_or_else(|| Arc::new(NoHooks))
    }
}

/// `SendErrorMessage(errorMsgType1, errorMsgType2, type, isNotify)`.
fn error2(out: &mut Out<'_>, player: &PlayerRef, e1: i16, e2: i16, kind: u32, notify: bool) {
    out.send(
        player,
        SCErrorMsgPacket {
            error1: e1,
            error2: e2,
            kind,
            is_notify: notify,
        },
    );
}

/// `Character.SendDebugMessage(message)`: a system chat line when `DebugInfo` is on and the
/// character's access level reaches `DebugInfoLevel` (the account access level, read from the
/// DB by `GetEffectiveAccessLevel`, is not consulted here).
pub fn send_debug_message(server: &GameServer, out: &mut Out<'_>, player: &PlayerRef, msg: &str) {
    if !server.config.debug_info {
        return;
    }
    let level = player.data().access_level;
    if level < server.config.debug_info_level as i32 {
        return;
    }
    out.send(
        player,
        crate::packets::world::SCChatMessagePacket {
            chat_type: -2, // ChatType.System
            sender: None,
            message: msg.to_string(),
            ability: 0,
            language_type: 0,
        },
    );
}

/// `MathUtil.CalculateDistance(a, b)` (2D, computed in doubles).
pub fn distance_2d(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = f64::from(ax) - f64::from(bx);
    let dy = f64::from(ay) - f64::from(by);
    (dx * dx + dy * dy).sqrt() as f32
}

/// `ZoneManager.GetZoneByKey(zoneKey)?.GroupId ?? 0`.
pub fn zone_group_id(server: &GameServer, zone_key: u32) -> u32 {
    server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map_or(0, |z| z.group_id)
}

/// Authorize the recipe against the live station at both cast start and product commit.
fn bench_access(
    server: &GameServer,
    player: &PlayerRef,
    doodad_id: u32,
    craft: &Craft,
    hooks: &dyn CraftHooks,
) -> bool {
    match bench_denial(server, player, doodad_id, craft, hooks) {
        None => true,
        Some(reason) => {
            tracing::warn!(
                craft_id = craft.id,
                skill_id = craft.skill_id,
                "{} may not craft at doodad {doodad_id}: {reason}",
                player.data().unit.name,
            );
            false
        }
    }
}

/// Why [`bench_access`] refuses (`None` = allowed).
fn bench_denial(
    server: &GameServer,
    player: &PlayerRef,
    doodad_id: u32,
    craft: &Craft,
    hooks: &dyn CraftHooks,
) -> Option<&'static str> {
    // Portable skills can name no target or the crafter. Neither form may bypass a
    // station recipe's requirements; every other nonzero target must be a real bench.
    if doodad_id == 0 || doodad_id == player.obj_id {
        return (craft.req_doodad_id != 0 || craft.is_pack)
            .then_some("the recipe requires a crafting station");
    }
    let Some(doodad) = server
        .world
        .object(doodad_id)
        .and_then(|o| o.as_doodad().cloned())
    else {
        return Some("the crafting station does not exist");
    };
    let (owner_type, house_id, owner_id, instance_id, deleted) = {
        let d = doodad.data();
        (
            d.owner_type,
            d.owner_db_id,
            d.owner_id,
            d.transform.instance_id,
            d.phase.deleted,
        )
    };
    if deleted {
        return Some("the bench is deleted");
    }
    if player.data().unit.transform.instance_id != instance_id {
        return Some("player and bench are in different instances");
    }
    let Some(perm) = hooks.doodad_craft_permission(server, &doodad, craft.id) else {
        return Some("the current crafting function does not offer this recipe");
    };
    use crate::models::doodad::doodad_owner_type as owner;
    let house = if owner_type == owner::HOUSING {
        let Some(house) = crate::systems::housing::house_by_id(server, house_id) else {
            return Some("the bench's house does not exist");
        };
        if house.data().unit.transform.instance_id != instance_id {
            return Some("bench and house are in different instances");
        }
        // CraftPack Any explicitly opens the recipe to visitors (including the eight tax
        // plaques). Other functions retain the house's access rules.
        if perm != doodad_func_permission::ANY
            && !crate::systems::housing::allowed_to_interact(server, &house, player)
        {
            let h = house.data();
            tracing::warn!(
                house_id,
                house_owner = h.unit.owner_id,
                house_permission = h.permission,
                craft_id = craft.id,
                "Craft bench house access denied"
            );
            return Some("the house does not allow the player to interact");
        }
        Some(house)
    } else {
        None
    };
    use doodad_func_permission as p;
    match perm {
        p::ANY | p::PERMISSION1 | p::PERMISSION2 | p::PERMISSION4 | p::OWNER_RAID_MEMBERS => {}
        p::OWNER_ONLY => {
            let owner_id = house.as_ref().map_or(owner_id, |h| h.data().unit.owner_id);
            if owner_id == 0 || player.character_id != owner_id {
                return Some("the crafting function requires its owner");
            }
        }
        p::SAME_ACCOUNT => {
            let account = house.as_ref().map(|h| h.data().account_id).or_else(|| {
                (owner_type == owner::CHARACTER)
                    .then(|| server.names.get_character_account(owner_id))
            });
            if account.is_none_or(|a| a == 0 || a != player.data().account_id) {
                return Some("the crafting function requires the owner's account");
            }
        }
        p::ZONE_RESIDENTS => {
            let zone_group = zone_group_id(server, doodad.data().transform.zone_id);
            let account = player.data().account_id;
            if !hooks
                .account_house_zone_ids(server, account)
                .into_iter()
                .any(|zone| zone_group_id(server, zone) == zone_group)
            {
                return Some("the crafting function requires a zone resident");
            }
        }
        _ => return Some("the crafting function has an unknown permission"),
    }
    let Some(template) = server.game_data.systems.skills.skill(craft.skill_id) else {
        return Some("the craft skill does not exist");
    };
    if template.max_range <= 0 {
        return None;
    }
    let source = crate::systems::skills::geometry::Target::Obj(WorldObject::from(player));
    let target = crate::systems::skills::geometry::Target::Obj(WorldObject::from(&doodad));
    let source_pos = crate::systems::skills::geometry::world_pos(server, &source);
    let target_pos = crate::systems::skills::geometry::world_pos(server, &target);
    if ![
        source_pos.x,
        source_pos.y,
        source_pos.z,
        target_pos.x,
        target_pos.y,
        target_pos.z,
    ]
    .into_iter()
    .all(f32::is_finite)
    {
        return Some("a position is not finite");
    }
    (crate::systems::skills::geometry::distance_to(server, &source, &target, true)
        > template.max_range as f32)
        .then_some("the bench is out of the craft skill's range")
}

/// `CharacterCraft.Craft(craft, count, doodadId)`.
pub fn craft(server: &GameServer, player: &PlayerRef, craft_id: u32, count: i32, doodad_id: u32) {
    let Some(craft) = server.game_data.systems.economy.craft.craft(craft_id) else {
        // C#: CraftManager.GetCraftById throws KeyNotFoundException before Craft() runs and the
        // client is left waiting. Deviation: end the (client side) batch with an error.
        tracing::error!("Craft: unknown craft {craft_id}");
        error2(
            &mut Out::Direct,
            player,
            error_message::CRAFT_CANT_ACT_ANY_MORE,
            error_message::CRAFT_INVALID_CRAFT_TYPE,
            0,
            false,
        );
        cancel_craft(server, player);
        return;
    };
    let mgr = &server.systems.economy.craft;
    let char_id = player.character_id;
    mgr.update(char_id, |s| {
        s.current_craft = Some(craft.id);
        s.count = count;
        s.doodad_id = doodad_id;
    });
    let hooks = mgr.hooks();
    let mut out = Out::Direct;

    // check if you are equipped with a backpack or glider
    let inv = Inv::of(player);
    if !inventory::can_replace_glider_in_backpack_slot(&server.game_data.items, &inv) {
        // TODO verified
        error2(
            &mut out,
            player,
            error_message::CRAFT_CANT_ACT_ANY_MORE,
            error_message::BACKPACK_OCCUPIED,
            0,
            false,
        );
        cancel_craft(server, player);
        return;
    }

    // Check if we have enough materials
    let has_materials = craft.craft_materials.is_empty()
        || craft
            .craft_materials
            .iter()
            .all(|m| inv.items_count_in(slot_type::INVENTORY, m.item_id, -1) >= m.amount);
    if !has_materials {
        // TODO not verified
        error2(
            &mut out,
            player,
            error_message::CRAFT_CANT_ACT_ANY_MORE,
            error_message::NOT_ENOUGH_REQUIRED_ITEM,
            0,
            false,
        );
        cancel_craft(server, player);
        return;
    }

    if !bench_access(server, player, doodad_id, craft, hooks.as_ref()) {
        abort_craft_with(server, player, error_message::CRAFT_PERMISSION_DENY);
        return;
    }
    let world = server.world.world_of(&WorldObject::from(player));

    mgr.update(char_id, |s| s.is_crafting = true);

    let Some(skill) = server
        .game_data
        .systems
        .economy
        .craft
        .skills
        .get(&craft.skill_id)
        .copied()
    else {
        // C#: new Skill(null) -> NullReferenceException (IsCrafting stays set, the client stays
        // locked). Deviation: end the batch with an error.
        tracing::error!(
            "Craft: unknown skill {} of craft {}",
            craft.skill_id,
            craft.id
        );
        abort_craft(server, player, skill_result::INVALID_SKILL);
        return;
    };
    // The labor the skill really charges (`Skill.EndSkill` applies the vocation's labor cost
    // multiplier); the raw `ConsumeLaborPower` made `EndCraft` refuse a craft the skill still
    // charged for.
    let labor_step = player
        .data()
        .actability
        .actabilities
        .iter()
        .find(|a| i64::from(a.id) == i64::from(skill.actability_group_id))
        .map(|a| a.step);
    mgr.update(char_id, |s| {
        s.consume_labor_power = effective_labor_cost(skill.consume_labor_power, labor_step)
    });
    let mut speed_multiplier = 1f32;
    if skill.actability_group_id > 0 {
        let mut current_points = 0u32;
        let found = player
            .data()
            .actability
            .actabilities
            .iter()
            .find(|a| a.id == skill.actability_group_id as u32)
            .map(|a| (a.step, a.point));
        if let Some((step, point)) = found {
            speed_multiplier *= inventory::actability_time_multiplier(step);
            current_points = point as u32;
        }
        // Check bonus from housing
        if let Some(w) = &world {
            let pos = w.world_pos_rot(&WorldObject::from(player));
            current_points = current_points.wrapping_add(hooks.house_actability_bonus_in_world(
                server,
                w.id,
                pos.x,
                pos.y,
                skill.actability_group_id,
            ));
        }
        // Validate skill level
        if i64::from(craft.actability_limit) > i64::from(current_points) {
            inventory::send_error_message(
                player,
                error_message::ACTABILITY_NOT_ENOUGH_POINT,
                skill.actability_group_id as u32,
                true,
            );
            // The C# stopped here and left the craft panel locked ("This breaks the craft
            // panel"): the client only unlocks on a nonzero, non-notify CraftCantActAnyMore.
            abort_craft_with(server, player, error_message::ACTABILITY_NOT_ENOUGH_POINT);
            return;
        }
    }
    let result =
        hooks.start_craft_cast(server, player, craft.skill_id, doodad_id, speed_multiplier);
    if result != skill_result::SUCCESS {
        // `Skill.Use` refused the cast (labor, reagents, range, cooldown, ...): no skill ever
        // started, so nothing else would tell the client its craft is over.
        tracing::warn!(
            "{} craft {} refused by skill {}: skill result {result}",
            player.data().unit.name,
            craft.id,
            craft.skill_id
        );
        abort_craft(server, player, result);
    }
}

/// `SkillManager`'s labor cost of a craft skill for a character with the given actability step
/// (`validation::labor_cost`: the vocation multiplier, at least 1 when the skill costs labor).
fn effective_labor_cost(raw: i32, step: Option<u8>) -> i32 {
    let labor = raw.max(0);
    let cost = step.map_or(labor, |step| {
        (f64::from(labor)
            * f64::from(crate::systems::skills::progress::labor_cost_multiplier(
                step,
            )))
        .round_ties_even() as i32
    });
    if labor > 0 {
        cost.max(1)
    } else {
        0
    }
}

#[cfg(test)]
pub(crate) fn effective_labor_cost_for_tests(raw: i32, step: Option<u8>) -> i32 {
    effective_labor_cost(raw, step)
}

/// The client message for a refused craft cast (`SkillResult` -> `ErrorMessageType`).
fn abort_reason(result: u8) -> i16 {
    match result {
        skill_result::NEED_LABOR_POWER => error_message::NOT_ENOUGH_LABOR_POWER,
        skill_result::NEED_REAGENT => error_message::NOT_ENOUGH_REQUIRED_ITEM,
        skill_result::NO_PERM | skill_result::INVALID_TARGET | skill_result::NO_TARGET => {
            error_message::CRAFT_PERMISSION_DENY
        }
        skill_result::TOO_FAR_RANGE | skill_result::TOO_CLOSE_RANGE | skill_result::OBSTACLE => {
            error_message::NOT_NEAR_TO_TARGET
        }
        skill_result::BAG_FULL => error_message::NOT_ENOUGH_SPACE,
        skill_result::COOLDOWN_TIME | skill_result::ON_CASTING => error_message::CRAFT_COOLDOWN,
        _ => error_message::INTERNAL_ERROR,
    }
}

/// Ends the whole craft batch because the next craft cannot start (or its cast failed): the
/// C# just returned, leaving the client's craft window locked ("greyed out") until relog.
/// Tells the client with the same `CraftCantActAnyMore` error the other stop paths use and
/// clears the server state.
pub fn abort_craft(server: &GameServer, player: &PlayerRef, result: u8) {
    abort_craft_with(server, player, abort_reason(result));
}

/// [`abort_craft`] with the `ErrorMessageType` shown as the reason. The 1.2 client resets its
/// craft batch (`IsWorkingCraft`, `CRAFT_ENDED`) only on `CraftCantActAnyMore` with a nonzero
/// reason and `isNotify == false`, so every stop of a started batch goes through here.
pub fn abort_craft_with(server: &GameServer, player: &PlayerRef, reason: i16) {
    debug_assert_ne!(reason, 0);
    error2(
        &mut Out::Direct,
        player,
        error_message::CRAFT_CANT_ACT_ANY_MORE,
        reason,
        0,
        false,
    );
    cancel_craft(server, player);
}

/// The craft skill's cast failed after it started (`Skill.Cast` re-validation): ends the batch
/// when `skill_id` is the skill of the character's running craft.
pub fn cast_failed(server: &GameServer, player: &PlayerRef, skill_id: u32, result: u8) {
    let state = server.systems.economy.craft.state(player.character_id);
    let is_current = state.is_crafting
        && state
            .current_craft
            .and_then(|id| server.game_data.systems.economy.craft.craft(id))
            .is_some_and(|c| c.skill_id == skill_id);
    if is_current {
        abort_craft(server, player, result);
    }
}

/// `CharacterCraft.EndCraft()` (called by the skills subsystem when the craft skill's
/// `CraftEffect` applies).
pub fn end_craft(server: &GameServer, player: &PlayerRef) {
    let mgr = &server.systems.economy.craft;
    let char_id = player.character_id;
    let state = mgr.update(char_id, |s| {
        s.count -= 1;
        s.is_crafting = false;
        *s
    });
    let Some(craft) = state
        .current_craft
        .and_then(|id| server.game_data.systems.economy.craft.craft(id))
    else {
        cancel_craft(server, player);
        return;
    };
    let labor = player.data().labor_power;
    if i32::from(labor) < state.consume_labor_power {
        let mut out = Out::Direct;
        send_debug_message(
            server,
            &mut out,
            player,
            "|cFFFFFF00[Craft] Not enough Labor Powers for crafting! Performing a fictitious crafting step...|r",
        );
        // TODO not verified
        // Deviation: the C# went on with the rest of the batch ("fictitious step"), but the
        // client resets its batch on this error, so the remaining crafts would run unseen.
        abort_craft_with(server, player, error_message::NOT_ENOUGH_LABOR_POWER);
        return;
    }

    let inv = Inv::of(player);
    if inv.free_slot_count(slot_type::INVENTORY) < craft.craft_products.len() as i32 {
        // TODO not verified (stops the batch like the labor case above)
        abort_craft_with(server, player, error_message::NOT_ENOUGH_SPACE);
        return;
    }

    // Keep the permission check and product/material commit together with house transfers
    // and permission changes. Enter the item scope before housing; ItemTxn follows it.
    let _items = server.systems.economy.operation_scope();
    let housing_bench = server
        .world
        .object(state.doodad_id)
        .and_then(|o| o.as_doodad().cloned())
        .is_some_and(|d| d.data().owner_type == crate::models::doodad::doodad_owner_type::HOUSING);
    let operation = housing_bench.then(|| {
        server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    });
    if !bench_access(server, player, state.doodad_id, craft, mgr.hooks().as_ref()) {
        drop(operation);
        error2(
            &mut Out::Direct,
            player,
            error_message::CRAFT_CANT_ACT_ANY_MORE,
            error_message::CRAFT_PERMISSION_DENY,
            0,
            false,
        );
        cancel_craft(server, player);
        return;
    }
    if !hand_out_products(server, player, &inv, craft) {
        drop(operation);
        cancel_craft(server, player);
        return;
    }
    drop(operation);

    if let Some(h) = mgr.hooks.get() {
        h.on_craft(server, player, craft.id);
    }

    if state.count > 0 {
        schedule_craft(server, player);
    } else {
        cancel_craft(server, player);
    }
}

/// The product / material part of `EndCraft` ("Improper" heuristic grade inheritance of the
/// 1.2 C#). `false` = a trade pack could not be equipped (the craft is cancelled).
fn hand_out_products(server: &GameServer, player: &PlayerRef, inv: &Inv, craft: &Craft) -> bool {
    let items = &server.game_data.items;
    let mut txn = ItemTxn::begin(server);
    let bag = inv.bag();

    // The materials are consumed from the bag only (the C# counted bank and equipment too and
    // then crafted without consuming them): check again before anything is handed out.
    let materials_ok = craft.craft_materials.iter().all(|m| {
        let have = bag
            .lock()
            .expect("container poisoned")
            .items_by_template(m.item_id, -1)
            .1;
        have >= m.amount
    });
    if !materials_ok {
        tracing::warn!(
            "{} crafted {} without the materials in the bag, craft refused.",
            player.data().unit.name,
            craft.id
        );
        error2(
            &mut Out::Direct,
            player,
            error_message::CRAFT_CANT_ACT_ANY_MORE,
            error_message::NOT_ENOUGH_REQUIRED_ITEM,
            0,
            false,
        );
        return false;
    }

    // "Improper" Heuristic Grade inheritance to be used for 1.2 only, due to unset flags in
    // compact. Search for the first equipment material in the craft
    let mut inherited_grade: Option<u8> = None;
    for material in &craft.craft_materials {
        if items
            .template(material.item_id)
            .is_some_and(|t| t.equip().is_some())
        {
            let g = bag.lock().expect("container poisoned");
            let (found, _) = g.items_by_template(material.item_id, -1);
            if let Some(first) = found.first().and_then(|id| g.get_item_by_id(*id)) {
                inherited_grade = Some(first.grade);
                break;
            }
        }
    }

    // The products handed out, for the MAKE_ITEM_* achievement credit once the craft went through.
    let mut produced: Vec<(u32, i32, i32)> = Vec::new();
    for product in &craft.craft_products {
        let grade_to_use = match inherited_grade {
            // If we found an equipment material, inherit grade and roll for free regrade
            Some(g) => free_regrade(server, i32::from(g)),
            None if product.item_grade_id > 0 => product.item_grade_id as i32,
            None => -1,
        };
        // Check if template allows grade changes
        if let Some(t) = items.template(product.item_id) {
            let msg = format!(
                "Product template {} - FixedGrade: {}, Gradable: {}",
                product.item_id,
                t.fixed_grade,
                if t.gradable { "True" } else { "False" }
            );
            let mut out = txn.out();
            send_debug_message(server, &mut out, player, &msg);
        }
        // Check if we're crafting a trade pack, if so, try to remove currently equipped
        // backpack slot
        if !inventory::is_auto_equip_trade_pack(items, product.item_id) {
            bag.lock()
                .expect("container poisoned")
                .acquire_default_item(
                    task_type::CRAFT_ACT_SAVED,
                    product.item_id,
                    product.amount,
                    grade_to_use,
                    player.character_id,
                    &mut txn.env,
                );
            produced.push((product.item_id, grade_to_use, product.amount));
        } else if !inventory::try_equip_new_backpack(
            &mut txn,
            inv,
            task_type::CRAFT_PICKUP_PRODUCT,
            product.item_id,
            product.amount,
            grade_to_use,
            player.character_id,
        ) {
            let mut out = txn.out();
            error2(
                &mut out,
                player,
                error_message::CRAFT_CANT_ACT_ANY_MORE,
                error_message::BACKPACK_OCCUPIED,
                0,
                false,
            );
            return false;
        } else {
            produced.push((product.item_id, grade_to_use, product.amount));
        }
    }

    for material in &craft.craft_materials {
        bag.lock().expect("container poisoned").consume_item(
            task_type::CRAFT_ACT_SAVED,
            material.item_id,
            material.amount,
            None,
            &mut txn.env,
        );
    }
    report_crafted(server, player, &produced);
    true
}

/// MAKE_ITEM_TYPE / MAKE_ITEM_IMPL achievement credit for the products of one craft. Runs inside
/// the craft's item transaction: the achievement hooks defer the report until it is released.
fn report_crafted(server: &GameServer, player: &PlayerRef, produced: &[(u32, i32, i32)]) {
    let items = &server.game_data.items;
    let mut inputs = Vec::new();
    for &(item_id, requested_grade, amount) in produced {
        let Some(template) = items.template(item_id) else {
            continue;
        };
        // The grade the container gives the new item (`AcquireDefaultItem`).
        let mut grade = requested_grade;
        if template.fixed_grade >= 0 && !template.gradable {
            grade = template.fixed_grade;
        }
        if grade == -1 {
            grade = template.fixed_grade;
        }
        let grade = grade.max(0) as u32;
        let amount = i64::from(amount);
        inputs.push(AchievementInput::Increment {
            kind_id: char_record_kind::MAKE_ITEM_TYPE,
            amount,
            matcher: RecordMatch::GradeWildcard {
                value1: item_id,
                grade: Some(grade),
            },
        });
        if template.impl_id > 0 {
            inputs.push(AchievementInput::Increment {
                kind_id: char_record_kind::MAKE_ITEM_IMPL,
                amount,
                matcher: RecordMatch::GradeWildcard {
                    value1: template.impl_id as u32,
                    grade: Some(grade),
                },
            });
        }
    }
    crate::systems::report_achievements(server, player, &inputs);
}

/// The delay `ScheduleCraft` waits before the next craft of the batch: the remaining global
/// cooldown when it is longer than the skill's `CooldownTime`.
pub fn next_craft_delay_ms(server: &GameServer, player: &PlayerRef, skill_id: u32) -> Option<i64> {
    let skill = server
        .game_data
        .systems
        .economy
        .craft
        .skills
        .get(&skill_id)?;
    let gcd = player.data().unit.global_cooldown_ms;
    let to_gcd = gcd - crate::world::now_ms();
    Some(if to_gcd > i64::from(skill.cooldown_time) {
        to_gcd
    } else {
        i64::from(skill.cooldown_time)
    })
}

/// `CharacterCraft.ScheduleCraft()`: a [`CraftTask`] after [`next_craft_delay_ms`].
fn schedule_craft(server: &GameServer, player: &PlayerRef) {
    let state = server.systems.economy.craft.state(player.character_id);
    let Some(craft) = state
        .current_craft
        .and_then(|id| server.game_data.systems.economy.craft.craft(id))
    else {
        // The batch was cancelled meanwhile: nothing to schedule.
        return;
    };
    let Some(delay) = next_craft_delay_ms(server, player, craft.skill_id) else {
        // C#: skillTemplate.CooldownTime on a null template -> NullReferenceException
        tracing::error!("ScheduleCraft: unknown skill {}", craft.skill_id);
        abort_craft(server, player, skill_result::INVALID_SKILL);
        return;
    };
    let task = CraftTask {
        player: player.clone(),
        craft_id: craft.id,
        obj_id: state.doodad_id,
        count: state.count,
    };
    let (Some(server), Ok(rt)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
        tracing::error!("ScheduleCraft: no server handle / runtime");
        abort_craft(server, player, skill_result::FAILURE);
        return;
    };
    rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay.max(0) as u64)).await;
        task.execute(&server);
    });
}

/// `CraftTask(character, craftId, objId, count)`.
#[derive(Debug, Clone)]
pub struct CraftTask {
    pub player: PlayerRef,
    pub craft_id: u32,
    pub obj_id: u32,
    pub count: i32,
}

impl CraftTask {
    /// `CraftTask.Execute`.
    pub fn execute(&self, server: &GameServer) {
        if self.count > 0 {
            craft(server, &self.player, self.craft_id, self.count, self.obj_id);
        }
    }
}

/// `CharacterCraft.CancelCraft()`.
pub fn cancel_craft(server: &GameServer, player: &PlayerRef) {
    let mgr = &server.systems.economy.craft;
    mgr.update(player.character_id, |s| {
        s.is_crafting = false;
        s.current_craft = None;
        s.count = 0;
        s.doodad_id = 0;
    });
    // Also cancel the related skill ? I don't think this really does anything for crafts, but
    // can't hurt I guess
    if let Some(h) = mgr.hooks.get() {
        h.cancel_craft_skill(server, player);
    }
    // Might want to send a packet here, I think there is a packet when crafting fails. Not
    // sure yet.
}

/// `CharacterCraft.FreeRegrade(baseGrade)`: 5% chance of one grade more (unless the grade
/// already equals `ItemManager.MaxGradeValue`, the highest `grade_order`).
pub fn free_regrade(server: &GameServer, base_grade: i32) -> i32 {
    free_regrade_with(server, base_grade, rand::random::<u32>() % 20)
}

/// [`free_regrade`] with the roll (`Random.Shared.Next(0, 20)`) given.
pub fn free_regrade_with(server: &GameServer, base_grade: i32, lucky_roll: u32) -> i32 {
    let mut grade = base_grade;
    let max_grade = server
        .game_data
        .items
        .grades
        .values()
        .map(|g| g.grade_order)
        .max()
        .unwrap_or(0);
    // Check grade is not already max
    if grade != max_grade {
        // 5% chance
        if lucky_roll < 1 {
            grade += 1;
        }
    }
    grade
}

// -------------------------------------------------------------------------------------------
// Packets
// -------------------------------------------------------------------------------------------

pub fn register(reg: &mut PacketRegistry) {
    // Not registered in the 1.2 `GameNetwork` (no opcode): `CSSetCraftingPayPacket`.
    reg.register::<CSExecuteCraft>(LEVEL_PLAIN, cs::CSExecuteCraft);
}

/// `CSExecuteCraft`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSExecuteCraft {
    pub craft_id: u32,
    pub obj_id: u32,
    pub count: i32,
}

impl CSExecuteCraft {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            craft_id: s.read_u32()?,
            obj_id: s.read_bc()?,
            count: s.read_i32()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        tracing::debug!(
            "CSExecuteCraft, craftId : {} , objId : {}, count : {}",
            self.craft_id,
            self.obj_id,
            self.count
        );
        let Some(p) = active(conn) else { return };
        craft(server, &p, self.craft_id, self.count, self.obj_id);
    }
}
sync_client_packet!(CSExecuteCraft);

/// `CSSetCraftingPayPacket` (only logged in C#).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSetCraftingPayPacket {
    pub obj_id: u32,
    pub money_amount: i32,
}

impl CSSetCraftingPayPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            money_amount: s.read_i32()?,
        })
    }

    fn run(self, _server: &GameServer, _conn: &mut GameConnection) {
        tracing::warn!(
            "SetCraftingPay, ObjId: {}, MoneyAmount: {}",
            self.obj_id,
            self.money_amount
        );
    }
}
sync_client_packet!(CSSetCraftingPayPacket);

/// `SCCraftFailedPacket(id, craftId, count)` (never sent by the 1.2 C#; "TODO needs fixing").
/// The client reads `u32` type, `i32` count and `count` item template ids (not craft ids; client
/// RE), so `item_id` is an item template id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCCraftFailedPacket {
    pub id: u32,
    pub item_id: u32,
    pub count: i32,
}
impl SCCraftFailedPacket {
    /// Entry cap of the 1.2 client's reader.
    pub const MAX_ENTRIES: i32 = 20;
}
packet_def!(
    SCCraftFailedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCCraftFailedPacket
);
impl Marshal for SCCraftFailedPacket {
    fn write(&self, s: &mut PacketStream) {
        // The 1.2 client reads at most 20 entries (OnCraftFailed 391e0840 -> 394d2370 clamps the
        // loop to twenty; Bevy RE notes), so the count never exceeds the entries written.
        let count = self.count.clamp(0, Self::MAX_ENTRIES);
        s.write_u32(self.id).write_i32(count);
        for _ in 0..count {
            s.write_u32(self.item_id);
        }
    }
}
