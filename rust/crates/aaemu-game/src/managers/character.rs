//! The lobby half of `Core/Managers/UnitManagers/CharacterManager`: `Create`,
//! `SetDeleteCharacter`, `SetRestoreCharacter`, `CheckForDeletedCharacters` (+ the
//! `CharacterDeleteTask` scheduling) and `ItemContainer.AcquireDefaultItem`, which character
//! creation uses for the starting supplies.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::Row;

use crate::data::character_templates::{CharacterTemplate, EquipItemsTemplate};
use crate::data::items::{bind_type, ItemTemplate};
use crate::db::get_account_details;
use crate::db_character::{
    check_for_deleted_characters, is_character_pending_deletion, save_edited_character_locked,
    save_new_character_locked, set_delete_times,
};
use crate::error::{GameError, Result};
use crate::managers::name::{normalize_name, CharacterCreateError};
use crate::models::char_data::{ability, ActionSlotType, CharacterAbilities, CharacterActability};
use crate::models::character::{Character, Inventory, Transform};
use crate::models::container::{equip_slot, slot_type, ContainerRef};
use crate::models::item::Item;
use crate::models::unit_model::UnitCustomModelParams;
use crate::net::connection::GameConnection;
use crate::packets::world::{
    SCCancelCharacterDeleteResponsePacket, SCCharacterCreationFailedPacket,
    SCDeleteCharacterResponsePacket, SCErrorMsgPacket,
};
use crate::server::GameServer;
use crate::world::unit_state::{create_character_response, edit_character_response};

/// The number of characters this account can create. The client is told the same two parts:
/// the login server advertises the base slots (`ACJoinResponse.afs` / `ACAuthResponse.slot_count`)
/// and `CSListCharacterPacket` sends only the purchased (`accounts.extra_character_slots`) count
/// in `SCGetSlotCount`; the client adds them (`FUN_394ccef0`), so creation must stop at their sum.
pub fn character_slot_limit(base: u8, extra_slots: u8) -> u8 {
    aaemu_commons::character_slots::character_slot_limit(base, extra_slots)
}

/// Whether an account with `count` undeleted characters may create one more. The limit is the
/// configured base plus the account's purchased slots; a failed extra-slots read returns 0
/// (`db::extra_character_slots`), so the decision falls back to the base.
pub fn may_create_character(base: u8, extra_slots: u8, count: i64) -> bool {
    count < i64::from(character_slot_limit(base, extra_slots))
}

// Native 1.2 x2game12.c:391136-391163 (HandleErrorMessage, lobby errors).
const ERROR_CHARACTER_EXPEDITION_OWNER: i16 = 333;
const ERROR_CANNOT_DELETE_CHAR_WHILE_ON_PLAY: i16 = 575;

/// The data `CSCreateCharacterPacket` carries.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateCharacterRequest {
    pub name: String,
    pub race: u8,
    pub gender: u8,
    pub body_items: [u32; 7],
    pub model_params: UnitCustomModelParams,
    pub ability1: u8,
    pub ability2: u8,
    pub ability3: u8,
    pub level: u8,
}

/// `ItemContainer.AcquireDefaultItem(ItemTaskType.Invalid, templateId, amount, grade)`.
pub fn acquire_default_item(
    server: &GameServer,
    container: &ContainerRef,
    template_id: u32,
    amount: i32,
    grade: i32,
) -> Result<bool> {
    acquire_default_item_recorded(
        server,
        container,
        template_id,
        amount,
        grade,
        &mut Vec::new(),
    )
}

fn create_starting_item(
    server: &GameServer,
    template_id: u32,
    amount: i32,
    grade: u8,
    now: i64,
    created_items: &mut Vec<(u64, bool)>,
) -> Option<Item> {
    let (item, canceled_deletion) = server.items.create_with_deletion(
        template_id,
        amount,
        grade,
        server.config.id,
        &server.game_data.items,
        &server.ids.item,
        now,
    )?;
    created_items.push((item.id, canceled_deletion));
    Some(item)
}

fn acquire_default_item_recorded(
    server: &GameServer,
    container: &ContainerRef,
    template_id: u32,
    mut amount: i32,
    grade: i32,
    created_items: &mut Vec<(u64, bool)>,
) -> Result<bool> {
    if amount <= 0 {
        return Ok(true);
    }
    let data = &server.game_data.items;
    let mut c = container.lock().expect("container poisoned");
    let (current, units) = c.items_by_template(template_id, grade);
    let Some(template) = data.template(template_id) else {
        return Ok(false);
    };
    let free = current.len() as i32 * template.max_count - units
        + c.free_slot_count() * template.max_count;
    if amount > free {
        return Ok(false);
    }
    let mut grade = grade;
    if template.fixed_grade >= 0 && !template.gradable {
        grade = template.fixed_grade;
    }
    if grade == -1 {
        grade = template.fixed_grade;
    }
    if grade < 0 {
        grade = 0;
    }
    let mut added = false;
    if c.slot_type != slot_type::MAIL && c.slot_type != slot_type::AUCTION {
        let mut ids = current.clone();
        ids.sort_by_key(|id| c.get_item_by_id(*id).map_or(0, |i| i.slot));
        for id in ids {
            let Some(item) = c.items.iter_mut().find(|i| i.id == id) else {
                continue;
            };
            let max = data.template(item.template_id).map_or(0, |t| t.max_count);
            let space = max - item.count;
            if space > 0 {
                let add = space.min(amount);
                item.count += add;
                item.dirty = true;
                amount -= add;
                added = true;
            }
            if amount < 0 {
                break;
            }
        }
    }
    let now = Utc::now().timestamp();
    let container_slot_type = c.slot_type;
    // Lock order: the item store before a container (see ItemManager::container_for_character).
    drop(c);
    while amount > 0 {
        let add = amount.min(template.max_count);
        let Some(mut item) =
            create_starting_item(server, template_id, add, grade as u8, now, created_items)
        else {
            tracing::error!(
                "Failed to add item with ID {template_id}, possible duplicate entries!"
            );
            return Ok(false);
        };
        amount -= add;
        let pref = if matches!(
            template.kind,
            crate::data::items::TemplateKind::Backpack { .. }
        ) && container_slot_type == slot_type::EQUIPMENT
        {
            equip_slot::BACKPACK
        } else {
            -1
        };
        apply_new_item_timers(&mut item, template, now);
        let res = container
            .lock()
            .expect("container poisoned")
            .add_new_item(item, pref, data);
        match res {
            Ok(true) => added = true,
            _ => {
                return Err(GameError::Protocol(
                    "AcquireDefaultItem(); Unable to add new items".into(),
                ))
            }
        }
    }
    container
        .lock()
        .expect("container poisoned")
        .update_free_slot_count();
    Ok(added)
}

/// The lifetime / charge setup `AcquireDefaultItemEx` does for each new item
/// (`SetItemExpirationTime`, `SetItemOnlineExpirationTime`, `ChargeCount` / `ChargeStartTime`).
fn apply_new_item_timers(item: &mut crate::models::item::Item, template: &ItemTemplate, now: i64) {
    if template.exp_abs_lifetime > 0 {
        item.expiration_time = now + i64::from(template.exp_abs_lifetime) * 60;
    }
    if template.exp_online_lifetime > 0 {
        item.expiration_online_minutes_left = f64::from(template.exp_online_lifetime);
    }
    if let Some(t) = template.exp_date {
        item.expiration_time = t;
    }
    if let Some(e) = template.equip() {
        if item.class.is_equip() {
            item.charge_count = e.charge_count;
            if e.charge_lifetime > 0 && template.bind_type & bind_type::BIND_ON_UNPACK == 0 {
                item.charge_start_time = now;
            }
        }
    }
}

/// `SetEquipItemTemplate(inventory, templateId, slot, grade)`.
fn set_equip_item_template(
    server: &GameServer,
    inventory: &Inventory,
    template_id: u32,
    slot: i32,
    grade: u8,
    created_items: &mut Vec<(u64, bool)>,
) -> Option<Item> {
    if template_id == 0 {
        return None; // AddOrMoveExistingItem(null) -> false
    }
    let data = &server.game_data.items;
    let Some(mut item) = create_starting_item(
        server,
        template_id,
        1,
        grade,
        Utc::now().timestamp(),
        created_items,
    ) else {
        // Create returned null -> AddOrMoveExistingItem(null).
        return None;
    };
    item.slot_type = slot_type::EQUIPMENT;
    item.slot = slot;
    let added = item.clone();
    let mut eq = inventory.equipment().lock().expect("container poisoned");
    match eq.add_new_item(item, slot, data) {
        Ok(_) => Some(added),
        Err(item) => {
            tracing::warn!(
                "SetEquipItemTemplate: item {} ({template_id}) refused in slot {slot}",
                item.id
            );
            None
        }
    }
}

fn equip_pack(
    server: &GameServer,
    inv: &Inventory,
    i: &EquipItemsTemplate,
    on_equip: &mut dyn FnMut(&Item),
    created_items: &mut Vec<(u64, bool)>,
) {
    use equip_slot::*;
    for (slot, (id, grade)) in [
        (HEAD, i.headgear),
        (NECK, i.necklace),
        (CHEST, i.shirt),
        (WAIST, i.belt),
        (LEGS, i.pants),
        (HANDS, i.gloves),
        (FEET, i.shoes),
        (ARMS, i.bracelet),
        (BACK, i.back),
        (UNDERSHIRT, i.undershirts),
        (UNDERPANTS, i.underpants),
        (MAINHAND, i.mainhand),
        (OFFHAND, i.offhand),
        (RANGED, i.ranged),
        (MUSICAL, i.musical),
        (COSPLAY, i.cosplay),
    ] {
        if let Some(item) = set_equip_item_template(server, inv, id, slot, grade, created_items) {
            on_equip(&item);
        }
    }
}

/// Declared after the save lock so rejected assets disappear before saves or ID reuse.
struct NewCharacter<'a> {
    server: &'a GameServer,
    id: u32,
    created_items: Vec<(u64, bool)>,
    committed: bool,
}

impl Drop for NewCharacter<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.server.items.discard_new_character(
                self.id,
                &self.created_items,
                &self.server.ids.item,
                &self.server.ids.container,
            );
            self.server.names.remove_character_id(self.id);
            self.server.ids.character.release_id(self.id);
        }
    }
}

/// The inputs shared by `CharacterManager.Create` and the lobby character edit. Both build the
/// character from a `CSCreateCharacterPacket` body, so every create-time rule (race/gender
/// template, abilities, starting kit, skills) applies to the edit too.
struct BuildInputs<'a> {
    server: &'a GameServer,
    /// Already validated and normalized by the caller.
    name: &'a str,
    request: &'a CreateCharacterRequest,
    template: &'a CharacterTemplate,
    id: u32,
    account_id: u32,
    connection_id: u32,
    ip: std::net::SocketAddr,
    access_level: i32,
    /// The create request carries the level; the edit keeps the stored one.
    level: u8,
    /// `None` takes the build time (create); an edit keeps the character's creation time.
    created: Option<i64>,
    labor_power: i16,
    labor_power_modified: i64,
    /// The persisted chat mute (`characters.chat_restricted_until`); a new character has none.
    chat_restricted_until: i64,
    /// `Inventory::new` (create, the persistent containers) or `Inventory::standalone` (edit,
    /// so a rejected edit leaves the persistent containers untouched).
    inventory: Inventory,
}

/// Whether the client-chosen body item `item` (`CSCreateCharacter.body_items[slot]`) is a body
/// part of the chosen race / gender model for that slot (`item_body_parts.slot_type_id - 23`)
/// that is not NPC-only; any other item id would be created for free.
fn body_item_allowed(
    server: &GameServer,
    template: &crate::data::character_templates::CharacterTemplate,
    slot: usize,
    item: u32,
) -> bool {
    server.game_data.items.body_part(item).is_some_and(|b| {
        b.model_id == template.model_id && b.slot_type_id == 23 + slot as u32 && !b.npc_only
    })
}

/// Builds a lobby character exactly as `CharacterManager.Create` does: position/faction from the
/// race/gender template, the ability's starting gear, the body items, the supplies, the
/// actabilities and the starting skills, with the gear buffs applied.
fn build_lobby_character(
    inputs: BuildInputs<'_>,
    created_items: &mut Vec<(u64, bool)>,
) -> Result<Character> {
    let BuildInputs {
        server,
        name,
        request,
        template,
        id,
        account_id,
        connection_id,
        ip,
        access_level,
        level,
        created,
        labor_power,
        labor_power_modified,
        chat_restricted_until,
        inventory,
    } = inputs;
    let data = &server.game_data;
    let mut c = Character::new(id, account_id, request.model_params.clone());
    c.unit.name = name.to_string();
    c.race = request.race;
    c.gender = request.gender;
    let sp = template.spawn_position;
    c.unit.transform = Transform {
        instance_id: crate::models::character::DEFAULT_INSTANCE_ID,
        world_id: data.worlds.default_world_template_id(),
        zone_id: sp.zone_id,
        x: sp.x,
        y: sp.y,
        z: sp.z,
        rot_x: sp.roll,
        rot_y: sp.pitch,
        rot_z: sp.yaw,
        parent_obj_id: 0,
    };
    c.unit.level = level;
    c.unit.faction_id = template.faction_id;
    c.faction_name = String::new();
    c.access_level = access_level;
    c.labor_power = labor_power;
    c.labor_power_modified = labor_power_modified;
    c.chat_restricted_until = chat_restricted_until;
    c.num_inventory_slots = template.num_inventory_slot;
    c.num_bank_slots = template.num_bank_slot;
    c.inventory = inventory;
    let now = Utc::now().timestamp();
    c.created = created.unwrap_or(now);
    c.updated = now;
    // A new (or lobby-edited) character starts with its first ability only; the second and
    // third are earned in play, whatever the client sent.
    c.ability1 = request.ability1;
    c.ability2 = ability::NONE;
    c.ability3 = ability::NONE;
    c.return_district_id = template.return_district_id;
    c.resurrection_district_id = template.resurrection_district_id;

    let items = data
        .characters
        .ability_items
        .get(&request.ability1)
        .cloned()
        .ok_or_else(|| GameError::Protocol(format!("no ability items for {}", request.ability1)))?;
    // Every equipped piece fires `EquipmentContainer.OnEnterContainer` ->
    // `UpdateGearBonuses(item, null)` (gear bonuses and buffs: weapon wield, armor grade, item
    // sets). C# keeps this `Character` object, buffs and buff index counter included, in
    // `Connection.Characters` until it is selected, so the gear buffs exist before the client
    // ever sees the character and are not announced again on select.
    let inv = c.inventory.clone();
    let detached =
        crate::net::connection::ConnectionHandle::detached(connection_id, ip, account_id);
    let player = crate::world::Player::new(c, detached.clone());
    let mut gear = |item: &Item| {
        crate::systems::skills::stats::update_gear_bonuses(server, &player, Some(item), None);
    };
    equip_pack(server, &inv, &items.items, &mut gear, created_items);
    for (i, &body) in request.body_items.iter().enumerate() {
        let body = if body != 0 && !body_item_allowed(server, template, i, body) {
            tracing::warn!(
                "Character {name}: body item {body} is not a creation body part of slot {i} for                  race {} gender {}; using the template default",
                request.race,
                request.gender
            );
            0
        } else {
            body
        };
        let item = if body == 0 && template.items[i] > 0 {
            template.items[i]
        } else {
            body
        };
        if let Some(item) =
            set_equip_item_template(server, &inv, item, i as i32 + 19, 0, created_items)
        {
            gear(&item);
        }
    }
    let mut c = player.data().clone();
    drop(player);
    // Nobody is in the world to receive these (`Character.Connection` is null in C#).
    let _ = c.unit.buffs.take_events();

    let bag = c.inventory.bag().clone();
    // `default_action_bar_actions` (1.2 data: 18791 -> 10, 18792 -> 11, 4045 -> 12) is loaded
    // by the client for its own action-bar reset. Place those supplies at the authored slots;
    // every other supply takes the next free slot from 13 on. Deviation from C#: it assigned
    // every supply sequentially from slot 10 in table order (4045 -> 10, 18791 -> 11,
    // 18792 -> 12, 417 -> 13).
    let default_bar: HashSet<u32> = data
        .characters
        .default_action_bar_actions
        .iter()
        .map(|&(item_id, _)| item_id)
        .collect();
    let default_slots: HashSet<usize> = data
        .characters
        .default_action_bar_actions
        .iter()
        .map(|&(_, slot)| usize::from(slot))
        .collect();
    let mut next_free = 13usize;
    let mut place_other_supply = |c: &mut Character, item_id: u32| {
        if default_bar.contains(&item_id) {
            return;
        }
        while default_slots.contains(&next_free)
            || c.slots
                .get(next_free)
                .is_some_and(|s| s.kind != ActionSlotType::None)
        {
            next_free += 1;
        }
        c.set_action(next_free, ActionSlotType::ItemType, u64::from(item_id));
        next_free += 1;
    };
    for s in &items.supplies {
        acquire_default_item_recorded(
            server,
            &bag,
            s.id,
            s.amount,
            i32::from(s.grade),
            created_items,
        )?;
        place_other_supply(&mut c, s.id);
    }
    let general = data
        .characters
        .ability_items
        .get(&0)
        .cloned()
        .ok_or_else(|| GameError::Protocol("no ability items for ability 0".into()))?;
    for s in &general.supplies {
        acquire_default_item_recorded(
            server,
            &bag,
            s.id,
            s.amount,
            i32::from(s.grade),
            created_items,
        )?;
        place_other_supply(&mut c, s.id);
    }
    for &(item_id, slot) in &data.characters.default_action_bar_actions {
        c.set_action(
            usize::from(slot),
            ActionSlotType::ItemType,
            u64::from(item_id),
        );
    }

    c.abilities = CharacterAbilities::default();
    c.abilities.set_ability(c.ability1, 0);
    c.actability = CharacterActability::for_new_character(&data.characters);

    initialize_starting_skills(server, &mut c)?;

    // character.Hp = character.MaxHp (the getters include the gear bonuses and buffs).
    let player = crate::world::Player::new(c, detached);
    crate::systems::skills::stats::refresh_cached_stats(
        server,
        &crate::world::WorldObject::from(&player),
    );
    let mut c = player.data().clone();
    drop(player);
    let _ = c.unit.buffs.take_events();
    c.unit.hp = c.unit.max_hp;
    c.unit.mp = c.unit.max_mp;
    c.restore_saved_hp_mp();
    Ok(c)
}

/// `CharacterManager.Create`.
pub async fn create(
    server: &GameServer,
    conn: &mut GameConnection,
    req: CreateCharacterRequest,
) -> Result<()> {
    let name = normalize_name(&req.name);
    let data = &server.game_data;
    // Reject unavailable and malformed race/gender pairs before allocating names or assets.
    let Some(template) = data
        .characters
        .template(req.race, req.gender)
        .filter(|t| t.creatable)
        .cloned()
    else {
        conn.send(&SCCharacterCreationFailedPacket {
            reason: CharacterCreateError::Failed as u8,
        });
        return Ok(());
    };
    // The same lock covers name validation, setup, slot counting and the SQL commit. In-flight
    // creates on distinct connections cannot both claim the last slot or the same name. Drop
    // (including cancellation) releases it after NewCharacter has rolled back its assets.
    let _transaction = server.save.transaction_lock().await;
    let pending = if server.names.name_exists(&name) {
        is_character_pending_deletion(&server.db, &name).await?
    } else {
        false
    };
    let code = server.names.validate_character_name_with_blocks(
        &name,
        &data.characters.blocked_names,
        || pending,
    );
    if code != CharacterCreateError::Ok {
        conn.send(&SCCharacterCreationFailedPacket { reason: code as u8 });
        return Ok(());
    }
    // NOTE: This is purely a warning to log potential cheaters
    if req.ability2 != ability::NONE || req.ability3 != ability::NONE {
        tracing::error!(
            "User tried to make a new character that has 2nd and/or 3rd ability already set. Account {}, Name {name}, Class {}, {}, {}",
            conn.account_id(),
            req.ability1,
            req.ability2,
            req.ability3
        );
    }
    let account = get_account_details(&server.db, &server.config, conn.account_id()).await;
    let mut access_level = server.config.account.access_level_default;
    if server.names.no_names_registered() {
        access_level = server
            .config
            .account
            .access_level_first_character
            .max(access_level);
    }
    // Inventory::new registers containers immediately. Exclude world/departure snapshots
    // throughout setup, SQL commit and failure cleanup, including setup errors below.
    let id = server
        .ids
        .character
        .get_next_id()
        .map_err(|e| GameError::Protocol(e.to_string()))?;
    let mut creation = NewCharacter {
        server,
        id,
        created_items: Vec::new(),
        committed: false,
    };
    server.names.add_character(id, &name, conn.account_id());
    let inventory = Inventory::new(
        id,
        template.num_inventory_slot,
        template.num_bank_slot,
        &server.items,
        &server.ids.container,
    );
    let mut c = build_lobby_character(
        BuildInputs {
            server,
            name: &name,
            request: &req,
            template: &template,
            id,
            account_id: conn.account_id(),
            connection_id: conn.id,
            ip: conn.ip,
            access_level,
            // The client's level field is ignored: every new character starts at level 1.
            level: 1,
            created: None,
            labor_power: account.labor,
            labor_power_modified: crate::db::unix(account.last_updated),
            chat_restricted_until: 0,
            inventory,
        },
        &mut creation.created_items,
    )?;

    // Use persisted rows, not a connection's stale/empty lobby list. Pending deletions still
    // occupy slots until deleted=1. Keep the save lock through the subsequent commit; the next
    // create sees this one, and a failed create leaves neither a slot nor an inventory behind.
    let base = server.config.characters.max_per_account;
    let extra_slots = crate::db::extra_character_slots(&server.db, conn.account_id()).await;
    let slots = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM characters WHERE account_id = ? AND deleted = 0",
    )
    .bind(conn.account_id())
    .fetch_one(&server.db)
    .await;
    match slots {
        Ok(count) if !may_create_character(base, extra_slots, count) => {
            conn.send(&SCCharacterCreationFailedPacket {
                reason: CharacterCreateError::WorldLimit as u8,
            });
            return Ok(());
        }
        Err(e) => {
            tracing::error!(
                "Cannot count character slots for account {}: {e}",
                conn.account_id()
            );
            conn.send(&SCCharacterCreationFailedPacket {
                reason: CharacterCreateError::Failed as u8,
            });
            return Ok(());
        }
        Ok(_) => {}
    }
    if save_new_character_locked(server, &mut c).await {
        creation.committed = true;
        let resp = create_character_response(&c);
        conn.characters.push(c);
        conn.send(&resp);
    } else {
        // There is no actual response for internal DB saving error for the client.
        conn.send(&SCCharacterCreationFailedPacket {
            reason: CharacterCreateError::Failed as u8,
        });
    }
    Ok(())
}

/// Releases the item ids an edit built until the edit commits. Unlike [`NewCharacter`] it never
/// touches the character id, the name or the (already existing) persistent containers.
struct EditedCharacter<'a> {
    server: &'a GameServer,
    created_items: Vec<(u64, bool)>,
    committed: bool,
}

impl Drop for EditedCharacter<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.server
                .items
                .discard_created_items(&self.created_items, &self.server.ids.item);
        }
    }
}

/// `CSEditCharacter` (0x023): the lobby character edit of the pre-select period.
///
/// The 1.2 client sends the create body with a leading character id while the login server
/// advertises the pre-select period (`ACJoinResponse.afs` bit 16, UI gate in
/// `character_select.alb` and `character_create*.alb`). The edit UI fixes race and gender (only
/// the character's own race button is enabled, `character_create_step1_view.alb`), sends the
/// edited name from the name field, the customizing blob and the body items of the preview unit,
/// and the ability the player picked. The build therefore re-runs character creation for the
/// same identity: only a character that never entered the world can be edited, because the
/// rebuild replaces its starting kit, skills and action slots.
pub async fn edit(
    server: &GameServer,
    conn: &mut GameConnection,
    character_id: u32,
    req: CreateCharacterRequest,
) -> Result<()> {
    // The client only offers the edit during the period; a forged packet outside it is refused.
    // The failure reply is UNKNOWN (the 1.2 handler `FUN_391eb910` has a success path only, and
    // a zero character id is its only no-op), so nothing is sent (capture-pending).
    if !server.config.characters.pre_select_period {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: the pre-select period is off (Characters:PreSelectPeriod)"
        );
        return Ok(());
    }
    // The client only offers the edit at character select; a session that is in the world (or
    // leaving it) must not rewrite its own row from under the live character.
    if conn.state != crate::net::connection::GameState::Lobby || conn.active_char.is_some() {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: session {} is not in the lobby",
            conn.id
        );
        return Ok(());
    }
    let data = &server.game_data;
    let Some(template) = data
        .characters
        .template(req.race, req.gender)
        .filter(|t| t.creatable)
        .cloned()
    else {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: race {} gender {} is not a creatable pair",
            req.race,
            req.gender
        );
        return Ok(());
    };
    // Same lock as create: name checks, the rebuilt starting kit and the SQL commit are one
    // critical section, so no create can take the edited name or race between the check and the
    // write.
    let _transaction = server.save.transaction_lock().await;
    let Some(index) = conn
        .characters
        .iter()
        .position(|c| c.id == character_id && c.account_id == conn.account_id())
    else {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: not an undeleted character of account {}",
            conn.account_id()
        );
        return Ok(());
    };
    let stored = &conn.characters[index];
    let stored_race = stored.race;
    let stored_gender = stored.gender;
    let stored_level = stored.unit.level;
    let old_name = stored.unit.name.clone();
    let access_level = stored.access_level;
    let created = stored.created;
    let labor_power = stored.labor_power;
    let labor_power_modified = stored.labor_power_modified;
    // A select/enter in flight owns the character and its row must not be rewritten under it; a
    // previous departure save still has the old row in flight.
    if server.enter_world.character_owner(character_id).is_some() {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: the character is selected or entering the world"
        );
        return Ok(());
    }
    if server.save.pending_departure(character_id).is_some() {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: the character's departure save is pending"
        );
        return Ok(());
    }
    // The cached lobby list can predate a select/departure; the persisted row is authoritative
    // (as `CharacterManager.Select` does for the same fields). Deleted rows are gone, and a
    // character that entered the world keeps its progress.
    let row = sqlx::query(concat!(
        "SELECT CAST(online_time AS SIGNED) AS online_time, ",
        "CAST(UNIX_TIMESTAMP(leave_time) AS SIGNED) AS leave_time, ",
        "CAST(UNIX_TIMESTAMP(chat_restricted_until) AS SIGNED) AS chat_restricted_until, ",
        "CAST(UNIX_TIMESTAMP(delete_request_time) AS SIGNED) AS delete_request_time, ",
        "CAST(UNIX_TIMESTAMP(delete_time) AS SIGNED) AS delete_time ",
        "FROM characters WHERE id = ? AND account_id = ? AND deleted = 0"
    ))
    .bind(character_id)
    .bind(conn.account_id())
    .fetch_optional(&server.db)
    .await;
    let row = match row {
        Ok(row) => row,
        Err(e) => {
            tracing::error!("CSEditCharacter for {character_id}: cannot read the row: {e}");
            return Ok(());
        }
    };
    let Some(row) = row else {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: no undeleted row for account {}",
            conn.account_id()
        );
        return Ok(());
    };
    let online_time: i64 = row.try_get("online_time")?;
    let leave_time: i64 = row.try_get::<Option<i64>, _>("leave_time")?.unwrap_or(0);
    // A GM may have muted the character while it was offline; the row, not the stale lobby copy,
    // is authoritative. `UNIX_TIMESTAMP` of the minimum date is NULL.
    let chat_restricted_until: i64 = row
        .try_get::<Option<i64>, _>("chat_restricted_until")?
        .unwrap_or(0);
    let delete_request_time: i64 = row
        .try_get::<Option<i64>, _>("delete_request_time")?
        .unwrap_or(0);
    let delete_time: i64 = row.try_get::<Option<i64>, _>("delete_time")?.unwrap_or(0);
    if delete_request_time != 0 || delete_time != 0 {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: the character is being deleted"
        );
        return Ok(());
    }
    if online_time != 0 || leave_time != 0 {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: the character has entered the world (play time {online_time}, last leave {leave_time})"
        );
        return Ok(());
    }
    // The edit UI cannot change race or gender (`GetEditUnitRace/Gender` seed read-only buttons
    // and only the character's own race stays enabled), so a different pair is a forged or
    // stale packet; applying it would be a race change.
    if req.race != stored_race || req.gender != stored_gender {
        tracing::warn!(
            "CSEditCharacter for {character_id} ignored: race/gender {} {} differs from the stored {} {}",
            req.race,
            req.gender,
            stored_race,
            stored_gender
        );
        return Ok(());
    }
    // The name may change: the edit UI pre-fills the existing name in the editable field and the
    // client validates it (`XlIsValidName` in the 1.2 edit builder). Re-run create's rules when
    // it does; an unchanged name is already registered to this character.
    let name = normalize_name(&req.name);
    if name != normalize_name(&old_name) {
        let pending = if server.names.name_exists(&name) {
            is_character_pending_deletion(&server.db, &name).await?
        } else {
            false
        };
        let code = server.names.validate_character_name_with_blocks(
            &name,
            &data.characters.blocked_names,
            || pending,
        );
        if code != CharacterCreateError::Ok {
            tracing::warn!(
                "CSEditCharacter for {character_id} ignored: name {name:?} refused with reason {}",
                code as u8
            );
            return Ok(());
        }
    }

    // Build the replacement with a standalone inventory so a rejected or failed edit leaves the
    // character's persistent containers (and their rows) as they are until the commit.
    let mut edited = EditedCharacter {
        server,
        created_items: Vec::new(),
        committed: false,
    };
    let inventory = Inventory::standalone(
        character_id,
        template.num_inventory_slot,
        template.num_bank_slot,
    );
    let mut c = build_lobby_character(
        BuildInputs {
            server,
            name: &name,
            request: &req,
            template: &template,
            id: character_id,
            account_id: conn.account_id(),
            connection_id: conn.id,
            ip: conn.ip,
            access_level,
            // The client sends level 1 for both create and edit; the stored level is what the
            // row must keep (a never-played character is level 1).
            level: stored_level,
            created: Some(created),
            labor_power,
            labor_power_modified,
            chat_restricted_until,
            inventory,
        },
        &mut edited.created_items,
    )?;

    // The persistent containers the character keeps; the save writes the rebuilt items into
    // their ids and the commit replaces the in-memory contents.
    let persistent = Inventory::new(
        character_id,
        c.num_inventory_slots,
        c.num_bank_slots,
        &server.items,
        &server.ids.container,
    );
    let batch = server
        .items
        .prepare_rebuilt_batch(&c.inventory, &persistent);
    if !save_edited_character_locked(server, &mut c, batch).await {
        // The transaction rolled back; the guard releases the rebuilt item ids. Nothing was
        // sent (capture-pending failure reply).
        tracing::error!("CSEditCharacter for {character_id} failed: the database save failed");
        return Ok(());
    }
    edited.committed = true;
    if name != normalize_name(&old_name) {
        server.names.rename_character(character_id, &name);
    }
    for id in server
        .items
        .commit_rebuilt_containers(&c.inventory, &persistent)
    {
        // The rebuilt containers' old rows were deleted by the same transaction; free the ids.
        server.ids.item.release_id(id as u32);
    }
    c.inventory = persistent;
    let resp = edit_character_response(&c);
    conn.characters[index] = c;
    conn.send(&resp);
    Ok(())
}

/// Excludes a concurrent select until the deletion request is committed. Never release an
/// existing session's reservation, including one owned by this very connection.
struct DeleteReservation<'a> {
    server: &'a GameServer,
    character_id: u32,
    connection_id: u32,
}

impl Drop for DeleteReservation<'_> {
    fn drop(&mut self) {
        self.server
            .enter_world
            .release_character(self.character_id, self.connection_id);
    }
}

/// Learn the selected ability's starters and make its first spell the primary shortcut.
fn initialize_starting_skills(server: &GameServer, c: &mut Character) -> Result<()> {
    let data = &server.game_data;
    for ds in &data.skills.default_skills {
        if !ds.add_to_slot {
            continue;
        }
        c.set_action(
            usize::from(ds.slot),
            ActionSlotType::Spell,
            u64::from(ds.skill_id),
        );
    }
    let mut slot = 1usize;
    let start = data
        .skills
        .start_ability_skills(c.ability1)
        .ok_or_else(|| {
            GameError::Protocol(format!("no start skills for ability {}", c.ability1))
        })?;
    for (index, &skill_id) in start.iter().enumerate() {
        while c
            .slots
            .get(slot)
            .is_some_and(|s| s.kind != ActionSlotType::None)
        {
            slot += 1;
        }
        if slot >= c.slots.len() {
            return Err(GameError::Protocol("no free starting skill slot".into()));
        }
        // Skills.AddSkill(template, 1, false): the level comes from the ability level.
        let ab = crate::db_character::ab_level(
            server,
            c,
            data.skills.skill(skill_id).map_or(0, |t| t.ability_id),
        );
        c.skills.add_skill_template(&data.skills, skill_id, ab);
        c.set_action(slot, ActionSlotType::Spell, u64::from(skill_id));
        // Shared attacks keep their shortcuts, except the action on key 1 moves to the
        // starter's free slot. This also works when authored defaults fill the first bar.
        if index == 0 {
            c.slots.swap(1, slot);
        }
        slot += 1;
    }
    Ok(())
}

/// `CharacterManager.SetDeleteCharacter`.
pub async fn set_delete_character(
    server: &GameServer,
    conn: &mut GameConnection,
    character_id: u32,
) -> Result<()> {
    if let Some(i) = conn
        .characters
        .iter()
        .position(|c| c.id == character_id && c.account_id == conn.account_id())
    {
        if server.world.character_by_id(character_id).is_some()
            || server.enter_world.reserve_character(character_id, conn.id) != Ok(true)
        {
            conn.send(&SCErrorMsgPacket::new(
                ERROR_CANNOT_DELETE_CHAR_WHILE_ON_PLAY,
                0,
                false,
            ));
            return Ok(());
        }
        let _reservation = DeleteReservation {
            server,
            character_id,
            connection_id: conn.id,
        };
        let _transaction = server.save.transaction_lock().await;
        // The loaded lobby copy can predate an ownership transfer. Use the guild ledger,
        // including guilds absent from that copy's expedition_id.
        if server
            .expeditions
            .all()
            .iter()
            .any(|e| !e.is_disbanded && e.faction.owner_id == character_id)
        {
            conn.send(&SCErrorMsgPacket::new(
                ERROR_CHARACTER_EXPEDITION_OWNER,
                0,
                false,
            ));
            return Ok(());
        }
        let now = Utc::now().timestamp();
        let level = conn.characters[i].unit.level;
        let mut delay = 0;
        for t in &server.config.account.delete_timings {
            if level >= t.level {
                delay = t.delay;
            }
        }
        let (req, del) = (now, now + i64::from(delay) * 60);
        let affected = sqlx::query(
            "UPDATE characters SET delete_request_time = ?, delete_time = ? \
             WHERE id = ? AND account_id = ? AND deleted = 0",
        )
        .bind(crate::db::from_unix(req))
        .bind(crate::db::from_unix(del))
        .bind(character_id)
        .bind(conn.account_id())
        .execute(&server.db)
        .await?;
        if affected.rows_affected() == 1 {
            let c = &mut conn.characters[i];
            c.delete_request_time = req;
            c.delete_time = del;
            conn.send(&SCDeleteCharacterResponsePacket {
                character_id,
                status: 2,
                delete_request_time: req,
                delete_time: del,
            });
        } else {
            conn.send(&SCDeleteCharacterResponsePacket {
                character_id,
                status: 0,
                delete_request_time: 0,
                delete_time: 0,
            });
        }
    } else {
        conn.send(&SCDeleteCharacterResponsePacket {
            character_id,
            status: 0,
            delete_request_time: 0,
            delete_time: 0,
        });
    }
    // Trigger our task queueing
    match server.self_arc() {
        Some(s) => schedule_deleted_characters_check(s).await,
        None => tracing::warn!("CheckForDeletedCharacters skipped: server not shared"),
    }
    Ok(())
}

/// `CharacterManager.SetRestoreCharacter`.
pub async fn set_restore_character(
    server: &GameServer,
    conn: &mut GameConnection,
    character_id: u32,
) -> Result<()> {
    if let Some(c) = conn.characters.iter_mut().find(|c| c.id == character_id) {
        c.delete_request_time = 0;
        c.delete_time = 0;
        conn.send(&SCCancelCharacterDeleteResponsePacket {
            character_id,
            status: 3,
        });
        set_delete_times(&server.db, character_id, 0, 0).await?;
    } else {
        conn.send(&SCCancelCharacterDeleteResponsePacket {
            character_id,
            status: 4,
        });
    }
    Ok(())
}

/// `CheckForDeletedCharacters` + scheduling of the next `CharacterDeleteTask`.
pub async fn schedule_deleted_characters_check(server: Arc<GameServer>) {
    if let Some(next) = run_deleted_characters_check(&server).await {
        tokio::spawn(character_delete_task(server, next));
    }
}

/// One `CheckForDeletedCharacters` pass; returns the next deletion time to wait for.
async fn run_deleted_characters_check(server: &GameServer) -> Option<i64> {
    tracing::debug!("CheckForDeletedCharacters - Begin");
    match check_for_deleted_characters(server).await {
        Ok(Some(t)) => {
            tracing::debug!("CheckForDeletedCharacters - Next delete scheduled at {t}");
            Some(t)
        }
        Ok(None) => {
            tracing::debug!("CheckForDeletedCharacters - No new deletions scheduled");
            None
        }
        Err(e) => {
            tracing::error!("CheckForDeletedCharacters: {e}");
            None
        }
    }
}

/// `CharacterDeleteTask`: waits for the next deletion time and runs the check again.
async fn character_delete_task(server: Arc<GameServer>, mut next: i64) {
    loop {
        let wait = (next - Utc::now().timestamp()).max(0) as u64;
        tokio::time::sleep(Duration::from_secs(wait)).await;
        match run_deleted_characters_check(&server).await {
            Some(n) => next = n,
            None => break,
        }
    }
}

#[cfg(test)]
#[path = "character_tests.rs"]
mod character_tests;

#[cfg(test)]
#[path = "unit_character_manager_tests.rs"]
mod unit_character_manager_tests;
