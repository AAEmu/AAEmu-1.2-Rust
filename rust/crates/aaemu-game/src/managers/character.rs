//! The lobby half of `Core/Managers/UnitManagers/CharacterManager`: `Create`,
//! `SetDeleteCharacter`, `SetRestoreCharacter`, `CheckForDeletedCharacters` (+ the
//! `CharacterDeleteTask` scheduling) and `ItemContainer.AcquireDefaultItem`, which character
//! creation uses for the starting supplies.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::data::character_templates::EquipItemsTemplate;
use crate::data::items::{bind_type, ItemTemplate};
use crate::db::get_account_details;
use crate::db_character::{
    check_for_deleted_characters, is_character_pending_deletion, save_new_character_locked,
    set_delete_times,
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
use crate::world::unit_state::create_character_response;

/// Matches `AcAuthResponse.slot_count` in aaemu-login (base slots, without paid slots).
pub const MAX_CHARACTERS_PER_ACCOUNT: i64 = 6;

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
    let mut c = Character::new(id, conn.account_id(), req.model_params.clone());
    c.unit.name = name.clone();
    c.race = req.race;
    c.gender = req.gender;
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
    c.unit.level = req.level;
    c.unit.faction_id = template.faction_id;
    c.faction_name = String::new();
    c.access_level = access_level;
    c.labor_power = account.labor;
    c.labor_power_modified = crate::db::unix(account.last_updated);
    c.num_inventory_slots = template.num_inventory_slot;
    c.num_bank_slots = template.num_bank_slot;
    c.inventory = Inventory::new(
        id,
        c.num_inventory_slots,
        c.num_bank_slots,
        &server.items,
        &server.ids.container,
    );
    let now = Utc::now().timestamp();
    c.created = now;
    c.updated = now;
    c.ability1 = req.ability1;
    c.ability2 = req.ability2;
    c.ability3 = req.ability3;
    c.return_district_id = template.return_district_id;
    c.resurrection_district_id = template.resurrection_district_id;

    let items = data
        .characters
        .ability_items
        .get(&req.ability1)
        .cloned()
        .ok_or_else(|| GameError::Protocol(format!("no ability items for {}", req.ability1)))?;
    // Every equipped piece fires `EquipmentContainer.OnEnterContainer` ->
    // `UpdateGearBonuses(item, null)` (gear bonuses and buffs: weapon wield, armor grade, item
    // sets). C# keeps this `Character` object, buffs and buff index counter included, in
    // `Connection.Characters` until it is selected, so the gear buffs exist before the client
    // ever sees the character and are not announced again on select.
    let inv = c.inventory.clone();
    let detached =
        crate::net::connection::ConnectionHandle::detached(conn.id, conn.ip, conn.account_id());
    let player = crate::world::Player::new(c, detached.clone());
    let mut gear = |item: &Item| {
        crate::systems::skills::stats::update_gear_bonuses(server, &player, Some(item), None);
    };
    equip_pack(
        server,
        &inv,
        &items.items,
        &mut gear,
        &mut creation.created_items,
    );
    for (i, &body) in req.body_items.iter().enumerate() {
        let item = if body == 0 && template.items[i] > 0 {
            template.items[i]
        } else {
            body
        };
        if let Some(item) = set_equip_item_template(
            server,
            &inv,
            item,
            i as i32 + 19,
            0,
            &mut creation.created_items,
        ) {
            gear(&item);
        }
    }
    let mut c = player.data().clone();
    drop(player);
    // Nobody is in the world to receive these (`Character.Connection` is null in C#).
    let _ = c.unit.buffs.take_events();

    let mut slot = 10usize;
    let bag = c.inventory.bag().clone();
    for s in &items.supplies {
        acquire_default_item_recorded(
            server,
            &bag,
            s.id,
            s.amount,
            i32::from(s.grade),
            &mut creation.created_items,
        )?;
        c.set_action(slot, ActionSlotType::ItemType, u64::from(s.id));
        slot += 1;
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
            &mut creation.created_items,
        )?;
        c.set_action(slot, ActionSlotType::ItemType, u64::from(s.id));
        slot += 1;
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

    // Use persisted rows, not a connection's stale/empty lobby list. Pending deletions still
    // occupy slots until deleted=1. Keep the save lock through the subsequent commit; the next
    // create sees this one, and a failed create leaves neither a slot nor an inventory behind.
    let slots = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM characters WHERE account_id = ? AND deleted = 0",
    )
    .bind(conn.account_id())
    .fetch_one(&server.db)
    .await;
    match slots {
        Ok(count) if count >= MAX_CHARACTERS_PER_ACCOUNT => {
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
