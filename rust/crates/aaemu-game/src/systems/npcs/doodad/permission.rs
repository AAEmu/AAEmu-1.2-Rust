//! Doodad func permission (`doodad_funcs.perm_id`, `DoodadFuncPermission`) enforcement for
//! `Doodad.Use`.
//!
//! The C# only read `Doodad.FuncPermission` in `CharacterCraft` and its enum values were
//! guesses (the file carries `// TODO: complete and verify enums`). The 1.2 client, however,
//! filters every doodad interaction entry by the function's `perm` in `FUN_393b3300` (called
//! from the interaction-list builder `FUN_393bd290` as
//! `FUN_393b3300(func->perm, doodad, ...)`), so a modified client can bypass the local filter
//! and send the skill directly. This module reproduces the client's switch on the server.
//!
//! Decoded switch (`fun_393b3300`, all values measued against the 1.2 `compact.sqlite3`):
//!
//! | perm | client check | 1.2 data |
//! |---|---|---|
//! | 0 | `true` (no check) | 13 228 rows: public interactions, all water (13625) and most uproot (13789) rows |
//! | 1 | owner character only (`doodad->owner == local character id`) | beds / fences / livestock care / siege builder tables |
//! | 2 | owner or family (`FUN_39302aa0` looks the owner id up in the 0xf0-byte family member map filled by `FUN_39300340`) | one uproot row (QA crop, `doodad_almighties` 901) |
//! | 3 | owner or same expedition (`FUN_392f8af0`; `DAT_39d44be8` is the expedition id, cf. `FUN_392fa4b0`'s leave-guild dialog) | one `DoodadFuncRecoverItem` row (siege declaration backpack) |
//! | 4 | owner or same team (`FUN_393b3280`: raid list, else the 5-slot party block of `FUN_39308e40`) | two event buff rows (New Year tables) |
//! | 5 | owner or same raid (`FUN_39308dd0` scans the 50 raid slots) | backpack storage box, fish-boat crane / tank |
//! | 6 | same account (`FUN_393b32c0`) | farm workbench, cushions, auction cushion, meeting-plaza fireplace |
//! | 7 | the doodad zone's owning faction is my nation (`FUN_39053360` + `FUN_392f80d0`) | territory statues, QA "this zone is my nation" bench |
//! | 8 | I have a house in the doodad's zone (`FUN_3932aaf0`) | QA "I have a house in this zone" bench + its uproot row |
//! | other | `false` (client default) | none |
//!
//! The owner is the doodad's *owning character*: direct `CHARACTER` owner, the house owner for
//! `HOUSING` doodads and the slave owner for `SLAVE` doodads (the client derives the same id
//! from the parent object). Public / system doodads have no owner, so every rule except 0
//! refuses them.
//!
//! `perm = 0` is deliberately **not** an owner rule: the 1.2 data gives player-planted crop
//! groups (e.g. `doodad_func_groups` 15684 potato seed bundle, 4367 oats) perm 0 on both the
//! water (13625) and the uproot (13789) rows. On a house garden the separate house permission
//! (`funcs::skills::use_`) remains the gate; on a public farm the guard time remains the gate.
//! `perm` only adds the relationship checks the client performs locally.

use crate::models::doodad::doodad_owner_type;
use crate::server::GameServer;
use crate::systems::npcs::doodad::templates::DoodadFunc;
use crate::world::{DoodadRef, PlayerRef, WorldInstance};

pub use crate::systems::npcs::doodad::templates::doodad_func_permission;

/// `Doodad.GetOwnerCharacter()` as an id, for offline owners too (the client compares ids, not
/// live objects). `None` for system / public doodads.
pub fn owner_character_id(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
) -> Option<u32> {
    let (owner_type, owner_id, owner_obj_id, owner_db_id) = {
        let d = doodad.data();
        (d.owner_type, d.owner_id, d.owner_obj_id, d.owner_db_id)
    };
    let id = match owner_type {
        doodad_owner_type::CHARACTER => owner_id,
        doodad_owner_type::HOUSING => {
            crate::systems::npcs::hooks::house_by_db_id(server, owner_db_id)
                .map(|house| {
                    let h = house.data();
                    if h.unit.owner_id != 0 {
                        h.unit.owner_id
                    } else {
                        owner_id
                    }
                })
                .unwrap_or(owner_id)
        }
        // `SlaveManager` stores the summoner's character id in `OwnerId` (and the summoner's
        // obj id in `OwnerObjId`), so the character id is authoritative; the C#
        // `GetOwnerCharacter` looked up `GetSlaveByObjId(OwnerObjId)`, which cannot match the
        // summoner's obj id. Fall back to a live slave owner for rows that only carry the obj.
        doodad_owner_type::SLAVE => {
            if owner_id != 0 {
                owner_id
            } else {
                world
                    .slave(owner_obj_id)
                    .map(|slave| slave.data().owner_obj_id)
                    .and_then(|owner_obj_id| server.world.character_by_obj_id(owner_obj_id))
                    .map(|player| player.character_id)
                    .unwrap_or(0)
            }
        }
        _ => 0,
    };
    (id != 0).then_some(id)
}

/// Whether `player` may run a func with `perm` on `doodad` (`FUN_393b3300`).
pub fn allows(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    player: &PlayerRef,
    perm: u32,
) -> bool {
    use doodad_func_permission as p;
    let owner = owner_character_id(server, world, doodad);
    let me = player.character_id;
    match perm {
        p::ANY => true,
        p::OWNER => owner == Some(me),
        // `FUN_393b3300` cases 2..5 return true before the relationship check when the owner
        // is the local character.
        p::FAMILY => owner.is_some_and(|o| o == me || in_same_family(server, player, o)),
        p::EXPEDITION => owner.is_some_and(|o| o == me || in_same_expedition(server, player, o)),
        p::TEAM => {
            owner.is_some_and(|o| o == me || server.systems.social.teams.are_team_members(me, o))
        }
        p::RAID => owner.is_some_and(|o| {
            o == me
                || server
                    .systems
                    .social
                    .teams
                    .active_team_by_unit(me)
                    .is_some_and(|team| !team.is_party && team.is_member(o))
        }),
        p::SAME_ACCOUNT => owner.is_some_and(|o| o == me || same_account(server, player, o)),
        // Cases 7 and 8 ignore the doodad owner: they gate on the zone only.
        p::NATION => nation_owns_zone(server, world, doodad, player),
        p::ZONE_RESIDENT => zone_resident(server, world, doodad, player),
        // `FUN_393b3300`'s default arm returns false for unknown values.
        _ => false,
    }
}

/// [`allows`] for one `doodad_funcs` row (`DoodadFunc.PermId`).
pub fn allows_func(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    player: &PlayerRef,
    func: &DoodadFunc,
) -> bool {
    allows(server, world, doodad, player, func.perm_id)
}

fn in_same_family(server: &GameServer, player: &PlayerRef, owner: u32) -> bool {
    let family = player.data().family;
    family != 0
        && server
            .systems
            .social
            .families
            .family(family)
            .is_some_and(|f| f.members.iter().any(|m| m.id == owner))
}

fn in_same_expedition(server: &GameServer, player: &PlayerRef, owner: u32) -> bool {
    let expedition = player.data().unit.expedition_id;
    expedition != 0
        && server
            .expeditions
            .get(expedition)
            .is_some_and(|e| e.members.iter().any(|m| m.character_id == owner))
}

fn same_account(server: &GameServer, player: &PlayerRef, owner: u32) -> bool {
    let account = player.data().account_id;
    account != 0 && account == server.names.get_character_account(owner)
}

/// `FUN_393b3300` case 7: `FUN_39053360(zone, my nation, 0)` compares the zone's owning faction
/// (`zones.faction_id`, 45 shipped zones) with the local nation (`DAT_39d44b94`). The client
/// compares top-level factions, so a player-founded nation resolves through its mother faction.
fn nation_owns_zone(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    player: &PlayerRef,
) -> bool {
    use crate::systems::social::nation::top_level_faction;
    let zone_id = zone_id_of(world, doodad);
    let zone_faction = server
        .game_data
        .zones
        .zone_by_key(zone_id)
        .map_or(0, |zone| zone.faction_id);
    if zone_faction == 0 {
        return false;
    }
    let mine = top_level_faction(server, player.data().unit.faction_id);
    let theirs = top_level_faction(server, zone_faction);
    mine.is_some() && mine == theirs
}

/// `FUN_393b3300` case 8 / `FUN_3932aaf0`: one of the player's houses is in the doodad's zone.
fn zone_resident(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    player: &PlayerRef,
) -> bool {
    let zone_id = zone_id_of(world, doodad);
    let account = player.data().account_id;
    zone_id != 0
        && account != 0
        && server
            .systems
            .housing
            .manager
            .by_account(account)
            .iter()
            .any(|house| house.data().unit.transform.zone_id == zone_id)
}

fn zone_id_of(world: &WorldInstance, doodad: &DoodadRef) -> u32 {
    let (zone_id, x, y) = {
        let d = doodad.data();
        (d.transform.zone_id, d.transform.x, d.transform.y)
    };
    if zone_id != 0 {
        return zone_id;
    }
    world.zone_id_at(x, y)
}
