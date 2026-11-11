//! Character session logic that spans several managers: selecting a character
//! (`CSSelectCharacterPacket`), spawning (`CSSpawnCharacterPacket`, `CSNotifyInGamePacket`,
//! `CSNotifyInGameCompletedPacket`, `Character.Spawn`), zone changes (`Character.OnZoneChange`),
//! movement (`CSMoveUnitPacket`), chat (`CSSendChatMessagePacket`), leaving the world
//! (`EnterWorldManager.Leave` / `LeaveWorldTask`) and disconnects
//! (`GameConnection.OnDisconnect` / `SaveAndRemoveFromWorld`).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::db_character::{
    add_buff_by_id, apply_active_buffs, fetch_active_buffs, load_character_details,
    offline_blocked_infos, offline_friend_infos, SavedBuffRow,
};
use crate::error::Result;
use crate::managers::chat::chat_type;
use crate::managers::expedition::MemberRefresh;
use crate::managers::friend::format_friend;
use crate::models::buff::{Buff, SkillCaster, BUFF_LOGGED_ON};
use crate::models::char_data::VisualOptions;
use crate::models::char_data::{BlockedInfo, FriendInfo};
use crate::models::character::{Character, DEFAULT_INSTANCE_ID};
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::movement::{MoveKind, MoveType, MoveTypeActorFlags};
use crate::models::transform::Transform;
use crate::net::connection::{ConnCommand, GameConnection, GameState};
use crate::packets::proxy::ChangeStatePacket;
use crate::packets::world::*;
use crate::server::GameServer;
use crate::systems::skills::packets::SCCooldownsPacket;
use crate::systems::skills::packets_special::SCTeleportUnitPacket;
use crate::systems::social::util::{send_chat_failure, ChatFailure};
use crate::world::{broadcast, unit_state, Player, PlayerRef, WorldInstance, WorldObject};

#[path = "zone_access.rs"]
mod zone_access;
pub use zone_access::{zone_access_allowed, ZoneAccessState, CLOSED_ZONE};
#[cfg(test)]
#[path = "zone_access_tests.rs"]
mod zone_access_tests;

/// `LeaveWorldTargetType`.
pub mod leave_target {
    pub const QUIT_GAME: u8 = 0;
    pub const CHARACTER_SELECT: u8 = 1;
    pub const SERVER_SELECT: u8 = 2;
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// The `Character` fields `SCChatMessagePacket` uses.
pub fn chat_sender(c: &Character, obj_id: u32) -> ChatSender {
    ChatSender {
        faction_id: c.unit.faction_id,
        obj_id,
        id: c.id,
        race: c.race,
        name: c.unit.name.clone(),
        gm: false,
    }
}

/// `Character.SendMessage(type, message)` (system text, no sender).
pub fn send_message(p: &PlayerRef, chat_type: i16, message: &str) {
    p.send(SCChatMessagePacket {
        chat_type,
        sender: None,
        message: message.to_string(),
        ability: 0,
        language_type: 0,
    });
}

/// `Faction.MotherId` of a faction id (0 when the faction is unknown).
fn mother_id(server: &GameServer, faction_id: u32) -> u32 {
    server
        .game_data
        .factions
        .get(faction_id)
        .map_or(0, |f| f.mother_id)
}

/// `Character.IsOnline` setter.
pub fn set_online(server: &GameServer, p: &PlayerRef, value: bool) {
    if !value {
        zone_access::cancel(p);
    }
    if p.data().is_online == value {
        return;
    }
    crate::systems::social::friend::send_status_change(server, p, true, value);
    p.data().is_online = value;
    if value {
        zone_access::update(server, p);
    }
    // !value -> TeamManager.SetOffline
    crate::systems::social::session::on_online_changed(server, p, value);
}

/// `Expedition.SendPacket`: every online member.
fn send_to_expedition(
    server: &GameServer,
    members: &[u32],
    packet: &SCExpeditionMemberStatusChangedPacket,
) {
    for id in members {
        if let Some(p) = server.world.character_by_id(*id) {
            p.send(packet.clone());
        }
    }
}

fn member_refresh(c: &Character) -> MemberRefresh {
    MemberRefresh {
        character_id: c.id,
        name: c.unit.name.clone(),
        level: c.unit.level,
        zone_id: c.unit.transform.zone_id,
        abilities: [c.ability1, c.ability2, c.ability3],
        position: (c.unit.transform.x, c.unit.transform.y, c.unit.transform.z),
    }
}

/// `Expedition.OnCharacterRefresh` / the member part of `OnCharacterLogin`.
fn expedition_refresh(server: &GameServer, p: &PlayerRef) {
    let (exp_id, refresh) = {
        let c = p.data();
        (c.unit.expedition_id, member_refresh(&c))
    };
    if exp_id == 0 {
        return;
    }
    if let Some((member, members)) =
        server
            .expeditions
            .refresh_member(exp_id, &refresh, Utc::now().timestamp())
    {
        send_to_expedition(
            server,
            &members,
            &SCExpeditionMemberStatusChangedPacket { member, flag: 0 },
        );
    }
}

/// Dispatches the `OnZoneChange` calls collected by a visibility update (after the grid lock is
/// released): `Character.OnZoneChange` for players, `Unit.OnZoneChange` (zone group buffs:
/// `systems::UnitHooks::on_unit_zone_change`) for the other units. Doodads have no override.
pub fn handle_zone_changes(server: &GameServer, changes: Vec<crate::world::ZoneChange>) {
    for z in changes {
        if let Some(p) = z.object.as_player() {
            on_zone_change(server, p, z.old_zone, z.new_zone);
        } else if z.object.is_unit() {
            // Unit.OnZoneChange (Mate / Slave overrides call it first too).
            if let Some(h) = server.systems.hooks.unit.get() {
                h.on_unit_zone_change(server, &z.object, z.old_zone, z.new_zone);
            }
            crate::systems::vehicles::on_unit_zone_change(
                server, &z.object, z.old_zone, z.new_zone,
            );
        }
    }
}

/// `Character.OnZoneChange(lastZoneKey, newZoneKey)`.
pub fn on_zone_change(server: &GameServer, p: &PlayerRef, last_zone: u32, new_zone: u32) {
    crate::systems::instances::tower_def::send_zone_list(server, p);
    // base.OnZoneChange (Unit: zone group buffs) -> the skills subsystem's hook.
    if let Some(h) = server.systems.hooks.unit.get() {
        h.on_unit_zone_change(server, &WorldObject::from(p), last_zone, new_zone);
    }
    let zones = &server.game_data.zones;
    let last = zones.zone_by_key(last_zone).cloned();
    let new = zones.zone_by_key(new_zone).cloned();
    let last_group = last.as_ref().map_or(0, |z| z.group_id as i16);
    let new_group = new.as_ref().map_or(0, |z| z.group_id as i16);
    if last_group != 0 {
        zone_chat(server, last_zone).leave(p);
    }
    let online = p.data().is_online;
    if new_group != 0 && online {
        zone_chat(server, new_zone).join(p);
    }
    if new.is_some() {
        expedition_refresh(server, p);
    }
    match new {
        Some(z) if !z.closed => {}
        Some(z) => {
            // Send extra info to player if we are still in a real but unreleased zone
            p.send(SCChatMessagePacket {
                chat_type: chat_type::SYSTEM,
                sender: None,
                message: format!(
                    "|cFFFF0000You have entered a closed zone ({} - {})!\nPlease leave immediately!|r",
                    z.zone_key, z.name
                ),
                ability: 0,
                language_type: 0,
            });
            tracing::warn!("{} entered closed zone {}", p.data().unit.name, z.zone_key);
        }
        None => {
            tracing::warn!("{} entered unknown zone {new_zone}", p.data().unit.name);
        }
    }
    zone_access::update(server, p);
}

/// The Rust equivalent of C# `character.Transform = new Transform(character, null, zoneId,
/// instanceId, x, y, z, ...)`: applies a server-forced teleport destination to the server's
/// copy, moves the character's object table entry to the destination instance, and fires
/// `Character.OnZoneChange` for the zone it leaves (`Unit.OnZoneChange` zone-group buffs such as
/// the open-sea turbulence buff 7743, zone chat, closed-zone handling).
///
/// The arrival is filed by the caller like in the C#: `CSInstanceLoadedPacket` after an
/// `SCLoadInstancePacket`, or [`file_teleport_arrival`] for a same-instance teleport. A missing
/// destination instance is logged and only the transform is applied (test / data fixtures).
pub fn teleport_character(server: &GameServer, p: &PlayerRef, destination: Transform) -> bool {
    let world = server
        .world
        .worlds()
        .into_iter()
        .find(|w| w.id == destination.instance_id);
    if world.is_none() {
        tracing::warn!(
            "Teleport of '{}' to unknown world instance {}",
            p.data().unit.name,
            destination.instance_id
        );
    }
    // A teleport ends a running trade when it is issued (see `trade::on_player_teleported`: the
    // client's own cancel on a load is not relied on). A plain zone-group crossing on foot does
    // not come through here.
    crate::systems::economy::trade::on_player_teleported(server, p);
    // A rider, a bound seat or a doodad bond would stay half attached on the server once the
    // transform loses its parent (the client is moved off it): dismount first, as the GM
    // teleport and the trial do (`Character.ForceDismount`, `AttachUnitReason.PrefabChanged`).
    // A player only standing on a deck has nothing to release and is just detached below.
    crate::systems::vehicles::force_dismount(
        server,
        p,
        crate::systems::vehicles::slaves::attach_reason::PREFAB_CHANGED,
    );
    let obj = WorldObject::from(p);
    let old = p.data().unit.transform;
    // A server-forced move to another instance is a leave: an item-paid dungeon visit starts its
    // `indun_zones.restore_item_time` re-entry window (exit doors / disconnect / kicks note it
    // themselves; this covers resurrection to an open-world return portal and the like).
    if old.instance_id != destination.instance_id {
        if let Some(dungeon) = server.systems.instances.dungeon_of_world(old.instance_id) {
            server.systems.instances.indun.note_entry_item_leave(
                p.character_id,
                dungeon.zone_group_id(),
                dungeon.indun_zone.restore_item_time,
            );
        }
    }
    // The C# `new Transform(character, null, ...)` has no parent.
    if let Some(old_world) = server.world.world_of(&obj) {
        old_world.detach_child(&obj);
    }
    {
        let mut c = p.data();
        let t = &mut c.unit.transform;
        t.instance_id = destination.instance_id;
        t.world_id = world
            .as_ref()
            .map_or(destination.world_id, |w| w.template.id);
        t.zone_id = destination.zone_id;
        t.set_position(
            destination.x,
            destination.y,
            destination.z,
            destination.rot_x,
            destination.rot_y,
            destination.rot_z,
        );
        t.parent_obj_id = 0;
    }
    // The object tables move with the transform so lookups find the character in the instance
    // it is in. The old instance's region grid lets go when the arrival is filed
    // (`instance_loaded` / `add_visible_object`), like the C# `AddVisibleObject` does.
    if old.instance_id != destination.instance_id {
        if let Some(old_world) = server
            .world
            .worlds()
            .into_iter()
            .find(|w| w.id == old.instance_id)
        {
            old_world.remove_object(p.obj_id);
        }
        if let Some(world) = world {
            world.add_object(p);
        }
    }
    if old.zone_id != destination.zone_id {
        on_zone_change(server, p, old.zone_id, destination.zone_id);
    }
    true
}

/// Files a teleported character into the destination region and dispatches the zone changes a
/// region move causes (`CSInstanceLoadedPacket` / `CSTeleportEndedPacket`). A character whose
/// instance load is still pending has no region yet; its arrival is filed by
/// [`crate::systems::instances::worlds::instance_loaded`] instead.
pub fn file_teleport_arrival(server: &GameServer, p: &PlayerRef) {
    if p.region().is_none() {
        return;
    }
    let instance_id = p.data().unit.transform.instance_id;
    let Some(world) = server.world.world(instance_id) else {
        return;
    };
    let changes = world.add_visible_object(p, &server.view_ctx());
    handle_zone_changes(server, changes);
}

/// A server-driven resurrection displacement (temple / statue, `Escape`): applies the position
/// like [`teleport_character`] and files the arrival. The 1.2 client does not answer
/// `SCCharacterResurrectedPacket` with `CSTeleportEnded`, so the server cannot wait for a
/// movement report to learn the destination zone.
pub fn resurrect_at(server: &GameServer, p: &PlayerRef, destination: Transform) {
    let Some(world) = server.world.world(destination.instance_id) else {
        return;
    };
    let mut destination = destination;
    // The zone the server lands in is the one at the coordinates; the caller's zone id is only
    // a fallback for an out-of-bounds position.
    let derived = world.zone_id_at(destination.x, destination.y);
    if derived > 0 {
        destination.zone_id = derived;
    }
    teleport_character(server, p, destination);
    file_teleport_arrival(server, p);
}

/// Horizontal / vertical distance a `CSTeleportEnded` position may differ from the destination
/// the server sent: the client resolves the ground height itself, so it may land a little off.
pub const TELEPORT_ACK_HORIZONTAL_TOLERANCE: f32 = 10.0;
pub const TELEPORT_ACK_VERTICAL_TOLERANCE: f32 = 30.0;

/// Locks the character's position (`DisabledSetPosition`), records the destination and sends the
/// `SCTeleportUnit`. Every server-forced teleport of a player goes through here (or through
/// [`send_teleport_unit_ending_trade`], which adds the trade hook), so [`teleport_ended`] can
/// check the client's acknowledgement against what the server asked for.
pub fn send_teleport_unit(p: &PlayerRef, packet: SCTeleportUnitPacket) {
    begin_teleport(p, packet.x, packet.y, packet.z);
    p.send(packet);
}

/// The lock + destination half of [`send_teleport_unit`], for teleports the client is told about
/// by another packet (`SCNaviTeleport`).
pub fn begin_teleport(p: &PlayerRef, x: f32, y: f32, z: f32) {
    let mut c = p.data();
    c.set_disabled_set_position(true);
    c.pending_teleport = Some((x, y, z));
}

/// [`send_teleport_unit`] for a teleport the server issues to a player that may be trading: the
/// trade ends first ([`begin_teleport_ending_trade`]). The two plain entry points above keep
/// their signatures (CONTRIBUTING-PORT section 3: no signature change in `world/*`); the
/// server-side senders call this one.
pub fn send_teleport_unit_ending_trade(
    server: &GameServer,
    p: &PlayerRef,
    packet: SCTeleportUnitPacket,
) {
    crate::systems::economy::trade::on_player_teleported(server, p);
    send_teleport_unit(p, packet);
}

/// [`begin_teleport`] that also ends the player's running trade. The trade ends when the
/// teleport is issued, not when its acknowledgement arrives: a same-instance portal / navi sign /
/// GM move reaches the client without [`teleport_character`], and a client that never reports the
/// arrival (or is carried by a vehicle) must not keep both players marked as trading. A second
/// hook call for a load that also goes through [`teleport_character`] is a no-op once the trade
/// is gone.
pub fn begin_teleport_ending_trade(server: &GameServer, p: &PlayerRef, x: f32, y: f32, z: f32) {
    crate::systems::economy::trade::on_player_teleported(server, p);
    begin_teleport(p, x, y, z);
}

/// `CSTeleportEndedPacket`: the client acknowledges an `SCTeleportUnit`. The server copy follows
/// the destination it recorded when it sent the teleport (the client's own coordinates are only
/// used when they are within tolerance of it, because the client knows the ground height), and
/// the region membership is re-evaluated, so a teleport out of a zone group (open sea) removes
/// that group's buffs even when no movement packet follows.
///
/// Nothing is applied when no teleport is pending, and the client's coordinates are never taken
/// when the server recorded no destination. The reported position is in world coordinates, so a
/// character that stood on another object (a ship deck, a moving gimmick) is detached first;
/// otherwise the new world position would be composed with the old parent and land in the
/// parent's region / zone. Returns whether the acknowledgement was accepted.
pub fn teleport_ended(server: &GameServer, p: &PlayerRef, x: f32, y: f32, z: f32) -> bool {
    let (locked, pending) = {
        let c = p.data();
        (c.disabled_set_position, c.pending_teleport)
    };
    if !locked {
        tracing::debug!(
            "Ignoring CSTeleportEnded of '{}': no teleport pending",
            p.data().unit.name
        );
        return false;
    }
    // Every `SCTeleportUnit` path ends here. Some same-world senders (the same-instance portal
    // branch, the NPC teleporter, dominion headquarters, instant games, GM `/move all` / nudge /
    // test height / spheres) do not call `force_dismount`, so release a transfer seat here too.
    crate::systems::vehicles::transfers::unboard_all(
        server,
        p,
        crate::systems::vehicles::slaves::attach_reason::PREFAB_CHANGED,
    );
    let Some((filed_instance, _, _)) = p.region() else {
        // An instance load is pending: `instance_loaded` files the arrival and lifts the lock.
        return true;
    };
    if filed_instance != p.data().unit.transform.instance_id {
        return true;
    }
    let (nx, ny, nz) = match pending {
        Some((dx, dy, dz)) => {
            let near = x.is_finite()
                && y.is_finite()
                && z.is_finite()
                && (x - dx).hypot(y - dy) <= TELEPORT_ACK_HORIZONTAL_TOLERANCE
                && (z - dz).abs() <= TELEPORT_ACK_VERTICAL_TOLERANCE;
            if near {
                (x, y, z)
            } else {
                tracing::debug!(
                    "CSTeleportEnded of '{}' reports ({x}, {y}, {z}) far from the destination ({dx}, {dy}, {dz}); using the destination",
                    p.data().unit.name
                );
                (dx, dy, dz)
            }
        }
        None => {
            tracing::warn!(
                "'{}' acknowledged a teleport without a recorded destination; keeping the server position",
                p.data().unit.name
            );
            let t = p.data().unit.transform;
            (t.x, t.y, t.z)
        }
    };
    let obj = WorldObject::from(p);
    if obj.transform().parent_obj_id != 0 {
        match server.world.world_of(&obj) {
            Some(w) => w.detach_child(&obj),
            None => p.data().unit.transform.parent_obj_id = 0,
        }
    }
    {
        let mut c = p.data();
        if pending.is_some() {
            c.unit.transform.set_xyz(nx, ny, nz);
        }
        c.set_disabled_set_position(false);
    }
    file_teleport_arrival(server, p);
    true
}

/// `ChatManager.GetZoneChat(zoneKey)`.
fn zone_chat(server: &GameServer, zone_key: u32) -> crate::managers::chat::ChannelRef {
    let zones = &server.game_data.zones;
    let group = zones.zone_by_key(zone_key).map_or(0, |z| z.group_id);
    let name = zones.group(group).map(|g| g.name.clone());
    server
        .chat
        .zone_chat(group, name.as_deref(), &server.ids.chat)
}

/// `Portal.Write` offsets (`ZoneManager.GetZoneOriginCell`).
fn portal_wire(server: &GameServer, p: &crate::data::world_data::Portal) -> PortalWire {
    let (ox, oy) = if p.zone_id != 0 {
        server
            .game_data
            .worlds
            .get_world_template_by_zone_key(p.zone_id)
            .and_then(|w| w.xml_world_zones().get(&p.zone_id))
            .map_or((0.0, 0.0), |z| (z.origin_x as f32, z.origin_y as f32))
    } else {
        (0.0, 0.0)
    };
    PortalWire {
        id: p.id,
        name: p.name.clone(),
        zone_id: p.zone_id,
        off_x: p.x - ox * 1024.0,
        off_y: p.y - oy * 1024.0,
        z: p.z,
        z_rot: p.z_rot,
    }
}

/// `Inventory.SendFragmentedInventory`.
fn send_fragmented_inventory(
    conn: &mut GameConnection,
    slot: u8,
    num_items: u8,
    bag: Vec<Option<Item>>,
) {
    if !bag.len().is_multiple_of(10) {
        tracing::warn!(
            "SendFragmentedInventory: Inventory Size not a multiple of 10 ({num_items})"
        );
    }
    if bag.len() != usize::from(num_items) {
        tracing::warn!(
            "SendFragmentedInventory: Inventory Size Mismatch; expected {num_items} got {}",
            bag.len()
        );
    }
    for (i, chunk) in bag.chunks(10).enumerate() {
        conn.send(&SCCharacterInvenContentsPacket {
            slot_type: slot,
            num_chunks: 1,
            start_chunk_idx: i as u8,
            items: chunk.to_vec(),
        });
    }
}

/// What the select path reads from MySQL besides `Character.Load()`: the offline friend /
/// blocked infos and the saved buffs (`LoadActiveBuffs`).
#[derive(Debug, Clone, Default)]
pub struct SelectDbData {
    pub offline_friends: Vec<FriendInfo>,
    pub offline_blocked: Vec<BlockedInfo>,
    pub saved_buffs: Vec<SavedBuffRow>,
}

/// Releases the character reservation taken by the caller unless the character actually entered
/// the world. A reservation that another path (or an earlier entry of the same connection)
/// already owned is left alone: `EnterWorldManager::reserve_character` reports that with
/// `Ok(false)`.
struct CharacterReservation<'a> {
    server: &'a GameServer,
    character_id: u32,
    connection_id: u32,
    armed: bool,
}

impl<'a> CharacterReservation<'a> {
    fn new(server: &'a GameServer, character_id: u32, connection_id: u32, taken: bool) -> Self {
        Self {
            server,
            character_id,
            connection_id,
            armed: taken,
        }
    }

    fn keep(&mut self) {
        self.armed = false;
    }
}

impl Drop for CharacterReservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.server
                .enter_world
                .release_character(self.character_id, self.connection_id);
        }
    }
}

/// `CSSelectCharacterPacket.Read`: the MySQL part, then [`select_loaded`].
pub async fn select_character(
    server: &GameServer,
    conn: &mut GameConnection,
    character_id: u32,
) -> Result<()> {
    if server.is_shutting_down() {
        refuse_during_shutdown(conn);
        return Ok(());
    }
    // During the pre-select period the client's Start button edits the character instead of
    // entering the world (character_select.alb); a forged client must not enter either. The
    // period is temporary and ends when the login stops advertising it.
    if server.config.characters.pre_select_period {
        tracing::warn!(
            "Character select for {character_id} refused: the character pre-select period is on (Characters:PreSelectPeriod)"
        );
        return Ok(());
    }
    let Some(idx) = conn.characters.iter().position(|c| c.id == character_id) else {
        tracing::error!(
            "Character {character_id} not found in list of loaded characters of this account {}",
            conn.account_id()
        );
        return Ok(());
    };
    let character_name = conn.characters[idx].unit.name.clone();
    // A second select while this session's character is still in the world (a double click on
    // "enter") must not reload state from MySQL over the live character, nor strand it.
    if conn.active_char.as_ref().is_some_and(|p| {
        server
            .world
            .character_by_obj_id(p.obj_id)
            .is_some_and(|live| Arc::ptr_eq(&live, p))
    }) {
        tracing::warn!(
            "Character select for '{character_name}' ({character_id}) ignored: session {} already has a character in the world",
            conn.id
        );
        return Ok(());
    }
    // Reserve the character before any MySQL access. The owner keeps it until its departure save
    // has finished, so a second connection of the account can neither enter the character twice
    // nor load its row while the previous session is still saving it (stale load).
    let taken = match server.enter_world.reserve_character(character_id, conn.id) {
        Ok(taken) => taken,
        Err(owner) => {
            tracing::warn!(
                "Character select refused for '{character_name}' ({character_id}): the character is already in the world on session {owner}"
            );
            return Err(crate::GameError::Protocol(format!(
                "character {character_id} is already in the world (session {owner})"
            )));
        }
    };
    let mut reservation = CharacterReservation::new(server, character_id, conn.id, taken);
    let _transaction = server.save.transaction_lock().await;
    if let Some(departing) = server.save.pending_departure(character_id) {
        if !server.save.save_departing_locked(server, &departing).await
            || server.save.pending_departure(character_id).is_some()
        {
            return Err(crate::GameError::Protocol(
                "Previous character save is pending".into(),
            ));
        }
    }
    // A sibling character's departure may still hold unflushed labor (`accounts.labor` is
    // account-level): save it too, or the character being selected would load a stale labor.
    if !server
        .save
        .flush_account_labor_locked(server, conn.account_id())
        .await
    {
        return Err(crate::GameError::Protocol(
            "Previous character save is pending".into(),
        ));
    }
    // Character selection may follow a logout that committed after the lobby list loaded.
    // Read its current wallet/core row inside the same save transaction ordering barrier.
    let Some((mut c, _)) =
        crate::db_character::load_character(server, character_id, conn.account_id()).await?
    else {
        return Ok(());
    };
    conn.characters[idx] = c.clone();
    // Force player into main_world when coming from character select
    c.unit.transform.instance_id = DEFAULT_INSTANCE_ID;
    load_character_details(server, &mut c).await?;
    // Account-wide attributes (`account_attributes`, economy): loaded once per character select.
    crate::systems::economy::account_attributes::load(server, c.account_id).await;
    // Character.Load(): Mates.Load (vehicles subsystem)
    crate::systems::vehicles::load_character(server, c.id).await;
    let offline_friend_ids: Vec<u32> = c
        .friends
        .iter()
        .map(|f| f.friend_id)
        .filter(|id| server.world.character_by_id(*id).is_none())
        .collect();
    let offline_blocked_ids: Vec<u32> = c
        .blocked
        .iter()
        .map(|b| b.1)
        .filter(|id| server.world.character_by_id(*id).is_none())
        .collect();
    let db = SelectDbData {
        offline_friends: offline_friend_infos(&server.db, &offline_friend_ids).await?,
        offline_blocked: offline_blocked_infos(&server.db, &offline_blocked_ids).await?,
        saved_buffs: fetch_active_buffs(server, c.id).await,
    };
    select_loaded(server, conn, c, db)?;
    reservation.keep();
    Ok(())
}

/// Login sync of the character's running skill / shared-tag cooldowns. Native `OnCooldowns`
/// (`391d43c0` -> `39899d40`) appends the entries to the local character's cooldown manager, so
/// this is what makes a cooldown that survived a relog visible (and counted down) again. An
/// empty list is not sent: it is a no-op, not a reset.
pub fn send_running_cooldowns(conn: &mut GameConnection, c: &Character, now_ms: i64) {
    let cooldowns = SCCooldownsPacket::from_cooldowns(&c.unit.cooldowns, now_ms);
    if !cooldowns.is_empty() {
        conn.send(&cooldowns);
    }
}

/// The rest of `CSSelectCharacterPacket.Read` once `Character.Load()` is done (no MySQL access).
pub fn select_loaded(
    server: &GameServer,
    conn: &mut GameConnection,
    mut c: Character,
    db: SelectDbData,
) -> Result<()> {
    // Character.Load(): "Update sync housing factions on login"
    crate::systems::housing::on_character_loaded(server, c.id, c.unit.faction_id);
    // Mates of all instances: despawned below (vehicles::on_character_selected).
    let obj_id = match server.world.used_obj_id(c.id) {
        Some(o) => o,
        None => {
            let o = server
                .ids
                .object
                .get_next_id()
                .map_err(|e| crate::GameError::Protocol(e.to_string()))?;
            server.world.remember_obj_id(c.id, o);
            o
        }
    };
    c.obj_id = obj_id;
    // Character.Load: Mails.GetCurrentMailList (runs before the character is in the world).
    crate::systems::economy::mail::get_current_mail_list_for(server, c.id);
    // The character is not in the world yet, so the call above counted nothing: count the
    // unread mail (also the mail that arrived while offline) for SCCharacterState / SCCountUnreadMail.
    c.unread_mail = crate::systems::economy::mail::count_unread_mail_for(server, c.id);
    let player = Player::new(c, conn.handle());
    // C# `WorldManager.TryAddCharacter`; unlike the C# the result is checked: an occupied world
    // entry means the character is already active, and continuing would give two live copies
    // (with two saves racing over the same row). Refuse instead and let the caller close the
    // connection; the caller's reservation guard releases the reservation.
    if !server.world.try_add_character(&player) {
        tracing::warn!(
            "Enter world refused for '{}' (character {}): the world entry is already registered",
            player.data().unit.name,
            player.character_id
        );
        return Err(crate::GameError::Protocol(format!(
            "character {} already has a world entry",
            player.character_id
        )));
    }
    // The wallet just loaded from the database is the committed one (departure saves skip a
    // character whose wallet still equals it).
    server.save.seed_committed_money(&player);
    conn.active_char = Some(player.clone());
    conn.spawn_handled = false;
    conn.in_game_notified = false;
    crate::systems::quests::on_character_selected(server, &player);
    // Mates of every instance despawned, pending vehicle despawn task aborted.
    crate::systems::vehicles::on_character_selected(server, &player);

    let data = &server.game_data;
    let snapshot = player.data().clone();
    let bounty_money = server.systems.social.bounties.active_total(snapshot.id);
    conn.send(&unit_state::character_state(&snapshot, bounty_money));
    conn.send(&SCCharacterGamePointsPacket {
        honor_point: snapshot.honor_point,
        vocation_point: snapshot.vocation_point,
    });
    // Inventory.Send
    conn.send(&SCCharacterInvenInitPacket {
        num_inven_slots: u32::from(snapshot.num_inventory_slots),
        num_bank_slots: snapshot.num_bank_slots as u32,
    });
    let bag = snapshot
        .inventory
        .bag()
        .lock()
        .expect("bag")
        .slotted_items();
    send_fragmented_inventory(
        conn,
        slot_type::INVENTORY,
        snapshot.num_inventory_slots,
        bag,
    );
    crate::systems::economy::item_timers::send_initial_item_expiration_timers(
        &player,
        slot_type::INVENTORY,
        now_ms(),
    );
    let bank = snapshot
        .inventory
        .warehouse()
        .lock()
        .expect("bank")
        .slotted_items();
    send_fragmented_inventory(conn, slot_type::BANK, snapshot.num_bank_slots as u8, bank);
    // SetInitialItemExpirationTimers (bank, then equipment; the bag's ran after its contents)
    for st in [slot_type::BANK, slot_type::EQUIPMENT] {
        crate::systems::economy::item_timers::send_initial_item_expiration_timers(
            &player,
            st,
            now_ms(),
        );
    }
    conn.send(&SCActionSlotsPacket {
        slots: snapshot.slots.clone(),
    });
    // Quests.Send / SendCompleted
    let now = now_ms();
    if snapshot.quests.active.len() <= 20 {
        conn.send(&SCQuestsPacket {
            quests: snapshot.quests.active.clone(),
            now_ms: now,
        });
    } else {
        for chunk in snapshot.quests.active.chunks(20) {
            conn.send(&SCQuestsPacket {
                quests: chunk.to_vec(),
                now_ms: now,
            });
        }
    }
    if snapshot.quests.completed.len() <= 200 {
        conn.send(&SCCompletedQuestsPacket {
            quests: snapshot.quests.completed.clone(),
        });
    } else {
        for chunk in snapshot.quests.completed.chunks(200) {
            conn.send(&SCCompletedQuestsPacket {
                quests: chunk.to_vec(),
            });
        }
    }
    conn.send(&SCActabilityPacket {
        last: true,
        actabilities: snapshot.actability.actabilities.clone(),
    });
    conn.send(&SCCountUnreadMailPacket {
        count: snapshot.unread_mail,
    });
    for chunk in snapshot.appellations.appellations.chunks(512) {
        conn.send(&SCAppellationsPacket {
            appellations: chunk
                .iter()
                .map(|&a| (a, a == snapshot.appellations.active))
                .collect(),
        });
    }
    // Portals.Send
    if !snapshot.portals.private_portals.is_empty() {
        conn.send(&SCCharacterPortalsPacket {
            portals: snapshot
                .portals
                .private_portals
                .iter()
                .map(|p| portal_wire(server, p))
                .collect(),
        });
    }
    if !snapshot.portals.district_portals.is_empty() {
        conn.send(&SCCharacterReturnDistrictsPacket {
            portals: snapshot
                .portals
                .district_portals
                .iter()
                .map(|p| portal_wire(server, p))
                .collect(),
            return_district_id: data
                .portals
                .district_return_point(snapshot.return_district_id, snapshot.unit.faction_id),
        });
    }
    // Friends.Send / Blocked.Send (online infos first, then the DB rows)
    if !snapshot.friends.is_empty() {
        let mut infos = Vec::new();
        for f in &snapshot.friends {
            if let Some(o) = server.world.character_by_id(f.friend_id) {
                let c = o.data();
                infos.push(format_friend(&c, c.in_party));
            }
        }
        infos.extend(db.offline_friends.iter().cloned());
        conn.send(&SCFriendsPacket { friends: infos });
    }
    if !snapshot.blocked.is_empty() {
        let mut infos = Vec::new();
        for (_, b) in &snapshot.blocked {
            if let Some(o) = server.world.character_by_id(*b) {
                let c = o.data();
                infos.push(BlockedInfo {
                    character_id: c.id,
                    name: c.unit.name.clone(),
                });
            }
        }
        infos.extend(db.offline_blocked.iter().cloned());
        conn.send(&SCBlockedUsersPacket { blocked: infos });
    }
    // Houses (SCMyHousePacket)
    crate::systems::housing::send_my_houses(server, conn, snapshot.id);
    for conflict in &server.systems.social.conflicts.conflicts() {
        conn.send(&SCConflictZoneStatePacket {
            zone_group_id: conflict.zone_group_id,
            state: conflict.current_state,
            end_time: conflict.next_state_time,
        });
    }
    // Running skill / tag cooldowns restored from `character_cooldowns`.
    send_running_cooldowns(conn, &snapshot, now_ms());
    // FactionManager.SendFactions / SendRelations
    if data.factions.factions.is_empty() {
        conn.send(&SCFactionListEmptyPacket);
    } else {
        for chunk in data.factions.factions.chunks(20) {
            conn.send(&SCFactionListPacket {
                factions: chunk.to_vec(),
            });
        }
    }
    if data.factions.relations.is_empty() {
        conn.send(&SCFactionRelationListEmptyPacket);
    } else {
        for chunk in data.factions.relations.chunks(200) {
            conn.send(&SCFactionRelationListPacket {
                relations: chunk.to_vec(),
            });
        }
    }
    // ExpeditionManager.SendExpeditions
    for chunk in server.expeditions.faction_chunks() {
        conn.send(&SCFactionListPacket { factions: chunk });
    }
    // Player nations: their faction records and relation rows (NationManager).
    crate::systems::social::nation::send_login_lists(server, conn);
    conn.send(&SCExpeditionRolePolicyListPacket {
        policies: Vec::new(),
    });
    if snapshot.unit.expedition_id != 0 {
        if let Some(e) = server.expeditions.get(snapshot.unit.expedition_id) {
            conn.send(&SCExpeditionRolePolicyListPacket {
                policies: e.policies.clone(),
            });
            let total = e.members.len() as u32;
            for chunk in e.members.chunks(20) {
                conn.send(&SCExpeditionMemberListPacket {
                    total,
                    id: e.faction.id,
                    members: chunk.to_vec(),
                });
            }
        }
    }
    for key in [1u16, 2, 5] {
        conn.send(&SCResponseUIDataPacket {
            character_id: snapshot.id,
            ui_data_type: key,
            ui_data: snapshot.get_option(key),
        });
    }

    {
        let now = now_ms();
        let mut c = player.data();
        add_buff_by_id(server, &mut c, BUFF_LOGGED_ON, now);
        if let Some(t) = data.characters.template(c.race, c.gender) {
            for &buff in &t.buffs {
                match data.skills.buff(buff) {
                    Some(bt) => {
                        let caster = SkillCaster {
                            kind: 0,
                            obj_id: c.obj_id,
                        };
                        let mut b = Buff::new(bt.clone(), caster, Some((c.id, c.unit.level)), now);
                        b.passive = true;
                        c.unit.buffs.add_buff(b, 0, 0, now);
                    }
                    None => tracing::error!("Template buff {buff} has no template"),
                }
            }
        }
    }
    let crime_faction = {
        let mut live = player.data();
        apply_active_buffs(server, &mut live, &db.saved_buffs, now_ms());
        // WP-B: the premium grade buffs (`pcbang_buffs` rows with `active = 't'` for the
        // account's grade) are part of the login state, like the saved buffs above.
        crate::systems::services::premium::apply_login_buffs(server, &mut live);
        let change = live.check_wanted_threshold(server, now_ms());
        // UpdateGearBonuses(null, null): gear bonuses are a later phase.
        live.restore_saved_hp_mp();
        live.breath = 60_000; // LungCapacity
        change
    };
    if !crime_faction.is_empty() {
        // `Character.CheckWantedThreshold` -> `Character.SetFaction(Pirate)` / pardon: the DB
        // load only restored the wanted buffs; the faction change itself needs the live player
        // and runs here, after the data lock is released (broadcast, houses, guild / party).
        // A pirate whose infamy reached 0 while offline is pardoned here, mirroring the pirate
        // conversion `bridges.rs` leaves to this select path.
        crate::systems::social::faction::apply_crime_faction_change(server, &player, crime_faction);
    }
    crate::systems::skills::on_character_loaded(server, &player); // buffs, gear bonuses, exact MaxHp/MaxMp
    let zone = player.data().unit.transform.zone_id;
    on_zone_change(server, &player, 0, zone);
    Ok(())
}

/// `CSSpawnCharacterPacket.Read`.
pub fn spawn_character(server: &GameServer, conn: &mut GameConnection, visual: VisualOptions) {
    if server.is_shutting_down() {
        refuse_during_shutdown(conn);
        return;
    }
    let Some(player) = conn.active_char.clone() else {
        tracing::error!(
            "CSSpawnCharacterPacket: ActiveChar is null for account {} — no character was selected. Disconnecting.",
            conn.account_id()
        );
        conn.shutdown();
        return;
    };
    // Deviation from the C#: a replayed CSSpawnCharacter would subscribe to the time manager
    // again; it is ignored until the next character select.
    if std::mem::replace(&mut conn.spawn_handled, true) {
        tracing::debug!("session {}: CSSpawnCharacter replay ignored", conn.id);
        return;
    }
    conn.state = GameState::World;
    let state = {
        let mut c = player.data();
        c.visual_options = Some(visual);
        unit_state::unit_state(&mut c, &server.game_data, now_ms())
    };
    // Writing the unit state force-adds the patron / auction-license buffs; their
    // SCBuffCreatedPacket goes out first (C# broadcasts them from inside the packet's Write).
    crate::systems::dispatch_buff_events(server, &WorldObject::from(&player));
    conn.send(&state);
    let sub = server.time.subscribe_with(
        &player.conn,
        Some(storm_time_of_day_resolver(server, &player)),
    );
    player.subscribers().push(sub);
    tracing::info!("CSSpawnCharacterPacket");
}

/// `new TimeOfDayObserver(character)`: each push asks
/// `StormShipLogic.ResolveClientTimeOfDayHours(owner)` (weak handles: the subscription does not
/// keep the server or the character alive).
pub fn storm_time_of_day_resolver(
    server: &GameServer,
    player: &PlayerRef,
) -> crate::managers::time::TimeOfDayResolver {
    let server = server.self_arc().map(|s| Arc::downgrade(&s));
    let player = Arc::downgrade(player);
    Arc::new(move || {
        let server = server.as_ref()?.upgrade()?;
        let player = player.upgrade()?;
        crate::systems::ai::physics::forces::resolve_client_time_of_day_for(&server, &player)
    })
}

/// The world instance a player is in (`ParentWorld`).
fn parent_world(server: &GameServer, p: &PlayerRef) -> Option<Arc<WorldInstance>> {
    let inst = p.data().unit.transform.instance_id;
    server.world.world(inst)
}

/// `Character.Spawn()` (`GameObject.Spawn`: `ParentWorld.AddObject` + `Show`).
pub fn spawn(server: &GameServer, p: &PlayerRef) {
    let Some(world) = parent_world(server, p) else {
        tracing::error!("Tried to spawn a object without a owning parent world");
        return;
    };
    world.add_object(p);
    server.world.try_add_character(p);
    let changes = world.show(p, &server.view_ctx());
    handle_zone_changes(server, changes);
    // The entry `SCUnitState` exchange is done: tell the viewers this character's bounty total
    // and this client the totals of the wanted characters it now sees (WP-SL3a).
    crate::systems::social::bounty::on_character_entered(server, p);
}

/// `CSNotifyInGamePacket.Execute`.
pub fn notify_in_game(server: &GameServer, conn: &mut GameConnection) {
    let Some(p) = conn.active_char.clone() else {
        tracing::error!("CSNotifyInGamePacket without an active character");
        return;
    };
    // Deviation from the C#: a replayed CSNotifyInGame would re-run the join hooks.
    if std::mem::replace(&mut conn.in_game_notified, true) {
        tracing::debug!("session {}: CSNotifyInGame replay ignored", conn.id);
        return;
    }
    set_online(server, &p, true);
    spawn(server, &p);
    let (zone, race, faction, expedition, name, id) = {
        let c = p.data();
        (
            c.unit.transform.zone_id,
            c.race,
            c.unit.faction_id,
            c.unit.expedition_id,
            c.unit.name.clone(),
            c.id,
        )
    };
    zone_chat(server, zone).join(&p); // shout, trade, lfg
    server.chat.nation_chat(race).join(&p);
    server
        .chat
        .faction_chat(mother_id(server, faction))
        .join(&p);
    // TODO: Maybe move to spawn character? (C#)
    crate::systems::social::session::on_notify_in_game_team(server, &p);
    if expedition != 0 {
        expedition_refresh(server, &p);
        if let Some(e) = server.expeditions.get(expedition) {
            server
                .chat
                .guild_chat(e.faction.id, &e.faction.name, &server.ids.chat)
                .join(&p);
        }
        // A granted guild rename is still pending: show the leader the dialog again.
        crate::systems::social::expedition::prompt_rename_if_allowed(server, &p);
    }
    // Connection.ActiveChar.UpdateGearBonuses(null, null)
    crate::systems::skills::stats::update_gear_bonuses(server, &p, None, None);
    crate::systems::dispatch_buff_events(server, &WorldObject::from(&p));
    crate::systems::social::session::on_notify_in_game_trial(server, &p);
    crate::systems::social::session::on_notify_in_game_achievements(server, &p);
    crate::systems::npcs::schedule_item_session::on_enter(server, &p);
    crate::systems::npcs::schedule_events::on_enter(server, &p);
    tracing::info!("NotifyInGame: {name} ({id})");
}

/// `CSNotifyInGameCompletedPacket` (`WorldManager.OnPlayerJoin`).
pub fn notify_in_game_completed(server: &GameServer, conn: &mut GameConnection) {
    let Some(p) = conn.active_char.clone() else {
        tracing::error!("CSNotifyInGameCompletedPacket without an active character");
        return;
    };
    p.send(SCOnOffSnowPacket {
        on: server
            .world
            .is_snowing
            .load(std::sync::atomic::Ordering::Relaxed),
    });
    // Family stuff
    crate::systems::social::session::on_player_join(server, &p);
    // Quest evaluations parked since the select (the client can handle them now).
    crate::systems::quests::on_in_game_completed(server, p.character_id);
    let c = p.data();
    tracing::info!(
        "NotifyInGameCompleted SubZoneId {}, {} ({})",
        c.sub_zone_id,
        c.unit.name,
        c.id
    );
}

/// `MathUtil.ConvertDirectionToRadian(sbyte)`.
fn direction_to_radian(d: i8) -> f32 {
    crate::models::movement::convert_direction_to_radian(d) as f32
}

/// `CSMoveUnitPacket.Execute` (characters, mates, slaves / vehicles, any base unit).
pub async fn move_unit(server: &GameServer, conn: &mut GameConnection, obj_id: u32, mt: MoveType) {
    let Some(me) = conn.active_char.clone() else {
        return;
    };
    {
        let c = me.data();
        if c.disabled_set_position {
            // No movement is analysed meanwhile: the speed baseline would go stale.
            server
                .systems
                .services
                .sus
                .reset_analyze_player_delta_movement(c.id);
            return;
        }
    }
    let Some(world) = parent_world(server, &me) else {
        return;
    };
    // `ParentWorld.GetBaseUnit(objId)`.
    let target = if obj_id == me.obj_id {
        Some(WorldObject::from(&me))
    } else {
        world.object(obj_id)
    };
    let Some(target) = target else {
        // Benign: the client keeps sending a few moves for an object (typically its mount) the
        // server just despawned (summon toggle, dungeon entry, death).
        tracing::debug!("Invalid target {obj_id} from {}", me.data().unit.name);
        return;
    };
    match &mt.kind {
        MoveKind::ShipRequest(r) => {
            crate::systems::vehicles::movement::ship_request(server, &me, &target, r);
        }
        MoveKind::Vehicle(v) => {
            crate::systems::vehicles::movement::vehicle_move(server, &world, &me, &target, &mt, v);
        }
        MoveKind::Unit(u) => {
            // Only the authenticated character or its owned mate can submit unit motion.
            // Resolve authority before buff removal, attachment changes or replication.
            let controlled = match &target {
                WorldObject::Player(player) => Arc::ptr_eq(player, &me),
                WorldObject::Mate(mate) => mate.data().owner_obj_id == me.obj_id,
                _ => false,
            };
            if !controlled {
                return;
            }
            // The server position is the baseline: a move beyond the unit's speed budget is
            // refused before it has any side effect. A riding / seat-bound player's own moves
            // are ignored below, so they are not judged.
            let ignored = target.as_player().is_some_and(|p| {
                p.data().is_riding
                    || crate::systems::vehicles::movement::player_bound_to_slave(&world, p)
            });
            if !ignored
                && !crate::world::move_guard::guard_unit_move(server, &world, &me, &target, &mt, u)
            {
                return;
            }
            // Its moving Pets, handle Pet XP for moving
            if let Some(mate) = target.as_mate() {
                crate::systems::vehicles::movement::on_mate_unit_move(server, &me, mate, u);
            }
            if let Some(player) = target.as_player() {
                // RemoveEffects (BuffRemoveOn.Move on a moving velocity): skills subsystem.
                crate::systems::skills::on_move_request(server, player, u.vel_x, u.vel_y, u.vel_z);
                if crate::systems::vehicles::movement::player_riding(server, &me, player)
                    || crate::systems::vehicles::movement::player_bound_to_slave(&world, player)
                {
                    // The C# finalized the rider's child transform with the mount / slave, which
                    // kept its speed baseline fresh; reseed it on the next own movement instead.
                    let id = player.data().id;
                    server
                        .systems
                        .services
                        .sus
                        .reset_analyze_player_delta_movement(id);
                    return;
                }
                // SetPlayerMoved: MOTD on the first movement after loading.
                let first_move = {
                    let mut c = player.data();
                    let first = !c.finished_loading;
                    c.finished_loading = true;
                    first
                };
                if first_move && Arc::ptr_eq(player, &me) {
                    send_message(&me, chat_type::SYSTEM, &server.config.world.motd);
                }
            }
            // The way back from a closed zone is the position from before this move. Record it
            // while the transform and its parent still agree: `standing_on` below attaches the
            // supporting object before the client's position in that object's frame is applied,
            // and until then the old world position reads as an offset in the new frame (far
            // outside the world: `GetZoneId: Coordinates out of bounds`, and a position outside
            // the world kept as the "safe" one).
            if let Some(player) = target.as_player() {
                zone_access::remember_open_position(server, player);
            }
            // Standing on objects (ship decks).
            crate::systems::vehicles::movement::standing_on(&world, &target, &mt, u);
            // If ActorFlag 0x40 is no longer set, it means we're no longer climbing/holding
            // onto something.
            let is_sticky = u
                .actor_flags
                .contains(MoveTypeActorFlags::HANGING_FROM_OBJECT);
            if target.core().sticky_parent() != 0
                && !is_sticky
                && !crate::systems::vehicles::movement::is_boarded_on_transfer(&world, &target)
            {
                world.set_sticky_parent(&target, None);
            }
            let old_pos = {
                let t = target.transform();
                (t.x, t.y, t.z)
            };
            // An actor standing on an object sends its primary position in the
            // supporting object's frame and the auxiliary position in world
            // coordinates, whatever the object kind (gcFlags 1 unit, 2 gimmick,
            // 4 doodad). If that object is unknown or has disappeared, use the
            // world backup rather than placing the character's small offset at
            // the world origin (under the sea).
            let missing_parent = target.transform().parent_obj_id == 0
                && u.actor_flags
                    .contains(MoveTypeActorFlags::STANDING_ON_OBJECT);
            let (x, y, z, rx, ry, rz) = if missing_parent {
                (
                    u.x2,
                    u.y2,
                    u.z2,
                    u.rotation_x2,
                    u.rotation_y2,
                    u.rotation_z2,
                )
            } else {
                (u.x, u.y, u.z, u.rotation_x, u.rotation_y, u.rotation_z)
            };
            target.with_transform(|t| {
                t.set_position(
                    x,
                    y,
                    z,
                    direction_to_radian(rx),
                    direction_to_radian(ry),
                    direction_to_radian(rz),
                )
            });
            let packet = SCOneUnitMovementPacket {
                obj_id,
                move_type: mt.clone(),
            };
            broadcast(&world, &target, &packet, true);
            // FinalizeTransform -> AddVisibleObject (region change) -> SetPosition. A player
            // moved to another instance (SCLoadInstancePacket) leaves the old region grid first.
            server
                .world
                .leave_other_instance(&crate::world::WorldObject::from(&target), world.id);
            let changes = world.add_visible_object(&target, &server.view_ctx());
            handle_zone_changes(server, changes);
            if let Some(player) = target.as_player() {
                zone_access::update(server, player);
            }
            // FinalizeTransform: every 5 s the movement speed analysis (SusManager)
            let new_pos = {
                let t = target.transform();
                (t.x, t.y, t.z)
            };
            if new_pos != old_pos {
                server
                    .systems
                    .services
                    .sus
                    .finalize_transform(server, &target);
            }
            // the zone's view of the move (systems::ai::collision): ZWUnitFell gate / cap and
            // the movement verifier; the client fallVel without the collision service
            let fall_vel = crate::systems::ai::collision::movement::on_unit_moved(
                server, &world, &target, mt.time, u,
            );
            crate::systems::skills::on_actor_move(server, &target, u.actor_flags, fall_vel);
            match target.as_player() {
                // Character.SetPosition (BuffRemoveOn.Move when moved) + DoFallDamage(FallVel).
                Some(player) => crate::systems::skills::on_moved(server, player, old_pos, fall_vel),
                None => {
                    if fall_vel > 0 && target.is_unit() {
                        crate::systems::vehicles::do_fall_damage(server, &target, fall_vel);
                    }
                }
            }
        }
        _ => {
            tracing::warn!(
                "Unknown MoveType: {:?} by {} for {}",
                mt.move_type(),
                me.data().unit.name,
                target.with_base_unit(|b| b.name().to_string())
            );
        }
    }
}

/// `CSSendChatMessagePacket.Read`.
#[allow(clippy::too_many_arguments)]
pub async fn send_chat_message(
    server: &GameServer,
    conn: &mut GameConnection,
    chat: i16,
    target_name: &str,
    message: &str,
    language_type: u8,
    ability: i32,
) {
    let Some(me) = conn.active_char.clone() else {
        return;
    };
    tracing::debug!("{message}");
    if let Some(cmd) = message.strip_prefix('/') {
        // `CommandManager.Handle(ActiveChar, message.Substring(1).Trim())`: a handled command
        // is not sent as chat.
        let (handled, _) = server
            .systems
            .services
            .commands
            .handle(server, &me, cmd.trim())
            .await;
        if handled {
            return;
        }
    }
    // SL4b: the configured per-type minimum level and the persisted GM mute (`restrict_chat`)
    // are checked before routing. The refusal and the sender identity come from one character
    // lock; the level gate exempts GMs (`access_level >= 100`) and the mute expires itself.
    let (sender, refusal) = {
        let mut c = me.data();
        let refusal = crate::systems::social::chat::chat_refusal(
            &server.config.chat,
            &mut c,
            chat,
            Utc::now().timestamp(),
        );
        (chat_sender(&c, me.obj_id), refusal)
    };
    if let Some(refusal) = refusal {
        crate::systems::social::chat::send_chat_refusal(&me, refusal);
        return;
    }
    // The 1.2 client throttles itself from `SCChatSpamDelayPacket` (`FUN_393C2540`) and shows
    // localized message 0x108 when a message is too fast. Enforce the same limits here so a
    // modified client cannot bypass them.
    if !crate::systems::social::chat::chat_spam_allows(
        &server.config.chat,
        &mut conn.chat_spam_last,
        chat,
        std::time::Instant::now(),
    ) {
        conn.send(&SCErrorMsgPacket::new(
            crate::systems::social::chat::CHAT_SPAM_DELAY_ERROR,
            0,
            true,
        ));
        return;
    }
    let msg = |t: i16, s: &ChatSender| SCChatMessagePacket {
        chat_type: t,
        sender: Some(s.clone()),
        message: message.to_string(),
        ability,
        language_type,
    };
    let error = |e: i16| me.send(SCErrorMsgPacket::new(e, 0, true));
    // Channel-bound failures can use `SCChatFailedPacket(error, channel name)`; the flag is off
    // by default because retail's choice is not captured (`CHAT-FAILED-1`).
    let chat_failure = |e: i16, channel: &str| {
        send_chat_failure(&me, &server.config.chat, ChatFailure::new(e, channel))
    };
    match chat {
        chat_type::WHISPER => {
            let target = server.world.character_by_name(target_name);
            match target {
                Some(t) if t.data().is_online => {
                    // A blocked sender's whisper is dropped silently: the 1.2 client has no
                    // "target has blocked you" error and its own whisper display does not
                    // filter, so only the server can enforce the list (capture-pending, case
                    // `BLOCK-LIST-1`).
                    if crate::systems::social::friend::is_blocked_by(&t, me.character_id) {
                        return;
                    }
                    let (tf, mf) = (
                        mother_id(server, t.data().unit.faction_id),
                        mother_id(server, sender.faction_id),
                    );
                    if tf != mf {
                        error(error_message::CHAT_CANNOT_WHISPER_TO_HOSTILE);
                    } else {
                        t.send(msg(chat_type::WHISPER, &sender));
                        let target_sender = chat_sender(&t.data(), t.obj_id);
                        conn.send(&msg(chat_type::WHISPERED, &target_sender));
                    }
                }
                _ => error(error_message::WHISPER_NO_TARGET),
            }
        }
        chat_type::WHITE => {
            if let Some(world) = parent_world(server, &me) {
                broadcast(&world, &me, &msg(chat, &sender), true);
            }
        }
        chat_type::RAID_LEADER | chat_type::RAID | chat_type::PARTY => {
            crate::systems::social::session::route_chat(
                server,
                &me,
                chat,
                &sender,
                message,
                ability,
                language_type,
            );
        }
        chat_type::TRADE | chat_type::GROUP_FIND | chat_type::SHOUT => {
            let zone = me.data().unit.transform.zone_id;
            zone_chat(server, zone).send_chat_packet(&msg(chat, &sender));
        }
        chat_type::CLAN => {
            let exp = me.data().unit.expedition_id;
            match server.expeditions.get(exp).filter(|_| exp != 0) {
                Some(e) => {
                    server
                        .chat
                        .guild_chat(e.faction.id, &e.faction.name, &server.ids.chat)
                        .send_message(
                            Some(&sender),
                            |_| sender.clone(),
                            message,
                            ability,
                            language_type,
                        );
                }
                None => chat_failure(error_message::CHAT_NOT_IN_EXPEDITION, "Expedition"),
            }
        }
        chat_type::FAMILY => {
            let family = me.data().family;
            if family > 0 {
                server
                    .chat
                    .family_chat(family, &server.ids.chat)
                    .send_message(
                        Some(&sender),
                        |_| sender.clone(),
                        message,
                        ability,
                        language_type,
                    );
            } else {
                chat_failure(error_message::CHAT_NOT_IN_FAMILY, "Family");
            }
        }
        chat_type::REGION => {
            server.chat.nation_chat(sender.race).send_message(
                Some(&sender),
                |_| sender.clone(),
                message,
                ability,
                language_type,
            );
        }
        chat_type::ALLY => {
            server
                .chat
                .faction_chat(mother_id(server, sender.faction_id))
                .send_message(
                    Some(&sender),
                    |_| sender.clone(),
                    message,
                    ability,
                    language_type,
                );
        }
        chat_type::JUDGE => {
            crate::systems::social::session::route_chat(
                server,
                &me,
                chat,
                &sender,
                message,
                ability,
                language_type,
            );
        }
        chat_type::PERFORM => {
            // `chat_commands` row 14 (Korean "연주", English "RP") lets the 1.2 client send type
            // 13, but the client's chat display (`chat_msg_event.alb`, function 20: its channel
            // comparison chain; the constants are in `baselib.alb`) has no branch for it: it
            // logs "Unknown chat channel: 13" and shows nothing. A broadcast would have no
            // visible recipient, so the message is dropped with a warning (no reply to the
            // sender, as for every other unsupported type).
            tracing::warn!(
                "Dropping chat type {} (performance / RP) from {}: the 1.2 client has no display channel for type 13",
                chat,
                sender.name
            );
        }
        other => tracing::warn!("Unsupported chat type {other} from {}", sender.name),
    }
}

/// `EnterWorldManager.Leave` for `QuitGame` / `CharacterSelect`.
pub fn leave(server: &GameServer, conn: &mut GameConnection, target: u8) {
    if conn.state != GameState::World || conn.leave_task.is_some() {
        return;
    }
    if target == leave_target::QUIT_GAME {
        if let Some(p) = &conn.active_char {
            send_message(p, chat_type::SYSTEM, &server.config.world.logout_message);
        }
    }
    let mut logout_time = 10_000; // in ms
    if let Some(p) = &conn.active_char {
        // Make it 5 minutes if you're still in combat
        if p.data().unit.is_in_battle {
            logout_time *= 30;
        }
        // Add 10 minutes if you have a Slave Active
        logout_time += crate::systems::vehicles::leave_extra_logout_ms(server, p);
    }
    conn.send(&SCPrepareLeaveWorldPacket {
        time: logout_time,
        target,
        idle_kick: false,
    });
    let tx = conn.command_sender();
    conn.leave_task = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(logout_time as u64)).await;
        let _ = tx.send(ConnCommand::LeaveWorld(target));
    }));
}

/// `CSCancelLeaveWorldPacket`.
pub fn cancel_leave(conn: &mut GameConnection) {
    let Some(task) = conn.leave_task.take() else {
        return;
    };
    task.abort();
    conn.send(&SCLeaveWorldCanceledPacket);
}

/// `GameObject.Delete()` for a player: hide + remove from the instance.
fn delete(server: &GameServer, p: &PlayerRef) {
    if let Some(world) = parent_world(server, p) {
        world.hide(p);
        world.remove_object(p.obj_id);
    }
}

/// `EnterWorldManager.LeaveWorldTask(connection, target, activeChar)`.
pub async fn leave_world_task(
    server: &GameServer,
    conn: Option<&mut GameConnection>,
    target: u8,
    active: Option<PlayerRef>,
) {
    if let Some(p) = &active {
        {
            let mut c = p.data();
            c.set_disabled_set_position(true);
        }
        set_online(server, p, false);
        // A leaving unit no longer holds a `once_one_*` doodad (cast or seat).
        crate::systems::npcs::doodad::occupancy::release_all_of(server, p.obj_id);
        p.data().leave_time = Utc::now().timestamp();
        crate::systems::quests::on_leave_world(server, p.character_id);
        // Despawn and unmount everybody from owned Mates / Slaves
        crate::systems::vehicles::on_leave_world(server, p);
        crate::systems::npcs::doodad::coffer::close_coffers_opened_by(server, p);
        // Check if still mounted on somebody else's mount and dismount that if needed
        crate::systems::vehicles::force_dismount(
            server,
            p,
            crate::systems::vehicles::slaves::attach_reason::PREFAB_CHANGED,
        );
        // Chat channels, family and guild logout, pending guild invitation and rename
        // offers (shared with a hard disconnect, `on_disconnect`).
        crate::systems::social::session::on_leave_world(server, p);
        crate::systems::services::premium::remove_grade_buffs(server, p);
        delete(server, p);
        // BuyBackItems.Wipe (+ the economy's per-character state)
        crate::systems::economy::on_leave_world(server, p);
        for sub in p.subscribers().drain(..) {
            server.time.unsubscribe(sub);
        }
        server.save.remember_departing(p);
        server.world.try_remove_character(p.obj_id);
    }
    save_and_remove_from_world(server, active.as_ref()).await;
    if let Some(p) = &active {
        // The departure save has finished (or failed into the pending-departure retry in
        // `select_character`): the character may be entered by a new session now.
        server
            .enter_world
            .release_character(p.character_id, p.conn.id);
    }
    if let Some(conn) = conn {
        conn.state = GameState::Lobby;
        conn.leave_task = None;
        if let Some(p) = &active {
            // `Connection.Characters` holds the same object as `ActiveChar` in C#.
            let c = p.data().clone();
            if let Some(slot) = conn.characters.iter_mut().find(|x| x.id == c.id) {
                *slot = c;
            }
        }
        conn.send(&SCLeaveWorldGrantedPacket { target });
        conn.send(&ChangeStatePacket { state: 0 });
        if target == leave_target::QUIT_GAME {
            // The player quit: free the account at once and close the connection (the queued
            // frames are flushed first). Deviation from the C#, which kept the connection and
            // the account registered until the client closed the socket or the dead-connection
            // sweep (30-60 s) did, so a prompt relog was refused ("enter world denied, reason 1").
            crate::systems::services::accounts::end_session(server, conn.account_id(), conn.id)
                .await;
            conn.shutdown();
        }
    }
}

/// Graceful server shutdown, on the connection's own task. The shutdown sends this command to
/// every connection, and again every few seconds to those still connected:
///
/// * In the world: the in-world leave without the logout timer (`LeaveWorldTask` to character
///   select: save, `SCLeaveWorldGranted(1)` + `ChangeState(0)`, the connection becomes Lobby).
///   The world list request is NOT sent yet: the client is busy switching to character select
///   (it drops its stream connection and reopens it, then loads the lobby on this same game
///   connection), and an `SCReconnectAuth` in that window did not take the live client to the
///   world list.
/// * In the lobby (already there, or left on an earlier round): the `CSLeaveWorld(ServerSelect)`
///   path: `GLPlayerReconnect` to the login server, which answers `LGPlayerReconnect`; the game
///   then sends `SCReconnectAuth`. Repeated rounds repeat the request.
///
/// Returns whether the character was in the world. A connection that never authenticated (no
/// account id) only has its pending logout cancelled; the shutdown kicks and closes it if it is
/// still connected at the end.
pub async fn return_to_world_list(server: &GameServer, conn: &mut GameConnection) -> bool {
    if let Some(task) = conn.leave_task.take() {
        task.abort();
    }
    let active = conn.active_char.clone();
    // After a leave the connection keeps `active_char` (as in C#), so only a character that is
    // still the live world entry counts as in the world.
    let in_world = active.as_ref().is_some_and(|p| {
        server
            .world
            .character_by_obj_id(p.obj_id)
            .is_some_and(|live| Arc::ptr_eq(&live, p))
    });
    if in_world {
        leave_world_task(
            server,
            Some(&mut *conn),
            leave_target::CHARACTER_SELECT,
            active,
        )
        .await;
        return true;
    }
    if conn.account_id() != 0 {
        tracing::info!("Shutdown: session {} is asked for the world list", conn.id);
        let token = server.new_reconnect_token();
        conn.set_reconnect_token(token);
        server
            .login
            .send(&crate::login::packets::GLPlayerReconnectPacket {
                gs_id: server.config.id,
                account_id: conn.account_id(),
                token,
            });
    }
    false
}

/// Refuses entering the world while the server shuts down: the localized "server maintenance"
/// kick (`SCKicked`, `KickedReason.KickByMaintenance`), then the connection closes.
pub fn refuse_during_shutdown(conn: &mut GameConnection) {
    tracing::info!(
        "Session {}: entering the world refused, the server is shutting down",
        conn.id
    );
    conn.send(&crate::packets::g2c_misc::SCKickedPacket {
        reason: crate::packets::g2c_misc::kicked_reason::KICK_BY_MAINTENANCE,
        msg: crate::systems::services::shutdown::KICK_MESSAGE.into(),
    });
    conn.shutdown();
}

/// `GameConnection.SaveAndRemoveFromWorld(activeChar)`.
pub async fn save_and_remove_from_world(server: &GameServer, active: Option<&PlayerRef>) {
    let Some(p) = active else {
        return;
    };
    zone_access::cancel(p);
    // A duel / battlefield faction is temporary: restore it before the departure save so the
    // character is never persisted with its Red / Blue team or corps faction (RPA-09).
    server
        .systems
        .social
        .duels
        .on_participant_leave(server, p)
        .await;
    crate::systems::social::faction::restore_temporary_faction(server, p);
    server.save.remember_departing(p);
    crate::systems::npcs::schedule_item_session::on_leave(server, p);
    let _transaction = server.save.transaction_lock().await;
    // Remove Radars (buff effect tasks: skills subsystem).
    server.systems.services.radar.unregister(p.character_id);
    delete(server, p);
    if server
        .world
        .character_by_obj_id(p.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, p))
    {
        server.world.try_remove_character(p.obj_id);
    }
    server.save.save_departing_locked(server, p).await;
    // Character.Save: Mates.Save
    crate::systems::vehicles::save_character(server, p.character_id).await;
}

/// The `ActiveChar` part of `GameConnection.OnDisconnect`.
pub async fn on_disconnect(server: &GameServer, conn: &mut GameConnection) {
    let Some(p) = conn.active_char.clone() else {
        // A select may have reserved a character that never entered (failed load / refused
        // select): the connection must not keep it reserved.
        server.enter_world.release_connection(conn.id);
        return;
    };
    zone_access::cancel(&p);
    // A disconnected unit no longer holds a `once_one_*` doodad (cast or seat).
    crate::systems::npcs::doodad::occupancy::release_all_of(server, p.obj_id);
    crate::systems::housing::dominion::runtime::abort_character(server, p.character_id);
    crate::systems::housing::dominion::participants::clear_player(server, &p);
    // WP-B: the premium grade buffs are session state, not saved buffs.
    crate::systems::services::premium::remove_grade_buffs(server, &p);
    crate::systems::economy::on_disconnect(server, &p);
    server.chat.leave_user_channels(&p, &server.ids.chat);
    // A character that still is the live world entry has not been through `leave_world_task`
    // (a connection keeps its `active_char` after a leave): its social logout is still to do.
    let live_now = server
        .world
        .character_by_obj_id(p.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, &p));
    if live_now {
        // The same social logout as `leave_world_task` (the C# disconnect had none: guild and
        // family members kept seeing the character online and its channels kept the dead
        // session): chat channels, family, guild status + `last_leave_time`, invitations.
        p.data().leave_time = Utc::now().timestamp();
        crate::systems::social::session::on_leave_world(server, &p);
    }
    if p.data().is_online {
        set_online(server, &p, false);
    }
    for sub in p.subscribers().drain(..) {
        server.time.unsubscribe(sub);
    }
    // Events.OnDisconnect (dungeon subscriptions, systems::instances).
    crate::systems::instances::events::fire_disconnect(server, &p);
    // Mates / slaves (systems::vehicles). Note (C#): chat channels are not left on a hard
    // disconnect.
    crate::systems::vehicles::on_disconnect(server, &p);
    crate::systems::npcs::doodad::coffer::close_coffers_opened_by(server, &p);
    // A connection keeps its `active_char` after a leave (quit / character select). That
    // character was already saved and removed from the world by `leave_world_task`: running the
    // departure again would write a stale copy over a character that logged in again on a newer
    // connection. Only the departure save is skipped; the cleanups above are not part of the
    // leave path and stay as before.
    let live = server
        .world
        .character_by_obj_id(p.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, &p));
    if !live {
        tracing::debug!(
            "Disconnect of session {}: character {} already left the world, no second save",
            conn.id,
            p.character_id
        );
        server.enter_world.release_connection(conn.id);
        return;
    }
    save_and_remove_from_world(server, Some(&p)).await;
    // The departure save has finished: a new session may enter the character now.
    server.enter_world.release_connection(conn.id);
}

/// Best-effort, synchronous cleanup of a connection that ended (the listener's unwind guard uses
/// it after a panic in [`on_disconnect`]): releases every reservation of the connection **first**
/// and then removes the connection's character from the world, so neither a reserved character
/// nor a ghost world entry survives a failed cleanup. Every step is idempotent with the normal
/// path; a poisoned lock inside the world part only aborts the world removal (the caller wraps
/// this call in `catch_unwind`, the reservation is already gone).
pub fn force_release_connection(server: &GameServer, connection_id: u32) {
    let released = server.enter_world.release_connection(connection_id);
    for character_id in released {
        tracing::warn!(
            "Session {connection_id}: force-released the reservation of character {character_id} after the disconnect cleanup failed"
        );
    }
    for p in server.world.all_characters() {
        if p.conn.id != connection_id {
            continue;
        }
        zone_access::cancel(&p);
        crate::systems::npcs::doodad::occupancy::release_all_of(server, p.obj_id);
        // Drop the server-wide entry first: if the grid removal below panics on a poisoned
        // instance lock, the character must still not be lookable up by id / obj id.
        server.world.try_remove_character(p.obj_id);
        // Keep it in the departure set: the next periodic save (or a select of it) saves it.
        server.save.remember_departing(&p);
        if let Some((instance_id, _, _)) = p.region() {
            if let Some(world) = server.world.world(instance_id) {
                world.delete(&p);
            }
        }
        tracing::warn!(
            "Session {connection_id}: force-removed character {} from the world after the disconnect cleanup failed",
            p.character_id
        );
    }
}

impl Player {
    /// `Character.Subscribers` (time-of-day subscription ids).
    pub fn subscribers(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.subscriptions.lock().expect("subscribers poisoned")
    }
}
