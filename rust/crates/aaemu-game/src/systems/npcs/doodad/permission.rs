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
//! Decoded switch (`fun_393b3300`, all values measured against the 1.2 `compact.sqlite3`):
//!
//! | perm | client check | 1.2 data |
//! |---|---|---|
//! | 0 | `true` (no check) | 13 228 rows: public interactions, all water (13625) and most uproot (13789) rows |
//! | 1 | owner character only (`doodad->owner == local character id`) | beds / fences / livestock care / siege builder tables |
//! | 2 | owner or family (`FUN_39302aa0` looks the owner id up in the 0xf0-byte family member map filled by `FUN_39300340`) | one uproot row (QA crop, `doodad_almighties` 901) |
//! | 3 | expedition member with the role right (`FUN_392f8af0`: leader `0xff`, else the role policy's right at `+0x90`) **and** the doodad's expedition (`doodad +0x350`) is mine or 0; no owner shortcut | one `DoodadFuncRecoverItem` row (siege camp 4307) |
//! | 4 | owner or same team (`FUN_393b3280`: raid list, else the 5-slot party block of `FUN_39308e40`) | two event buff rows (New Year tables) |
//! | 5 | owner or same raid (`FUN_39308dd0` scans the 50 raid slots) | backpack storage box, fish-boat crane / tank |
//! | 6 | same account (`FUN_393b32c0`) | farm workbench, cushions, auction cushion, meeting-plaza fireplace |
//! | 7 | the doodad's zone **group** is owned by my player-founded nation (`FUN_39053360`: `FUN_392fa6b0` requires a player nation; the dominion owner (`FUN_392f3510`/`FUN_3983a730`) resolves through `FUN_392f8180`) | territory statues, QA "this zone is my nation" bench |
//! | 8 | I have a house in the doodad's zone **group** (`FUN_3932aaf0` compares `FUN_397a6c60`, the `zones.group_id` field) | QA "I have a house in this zone" bench + its uproot row |
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
    // perm 0 is nearly every use (13 228 rows) and cases 7/8 do not read the owner: resolve
    // the owner (a house lookup for HOUSING doodads) only for the rules that compare it.
    match perm {
        p::ANY => return true,
        p::NATION => return nation_owns_zone(server, world, doodad, player),
        p::ZONE_RESIDENT => return zone_resident(server, world, doodad, player),
        _ => {}
    }
    let Some(owner) = owner_character_id(server, world, doodad) else {
        return false;
    };
    let me = player.character_id;
    match perm {
        p::OWNER => owner == me,
        // `FUN_393b3300` cases 2, 4 and 5 return true before the relationship check when the
        // owner is the local character; case 3 has no such shortcut.
        p::FAMILY => owner == me || in_same_family(server, player, owner),
        p::EXPEDITION => expedition_allows(server, doodad, player),
        p::TEAM => owner == me || server.systems.social.teams.are_team_members(me, owner),
        p::RAID => {
            owner == me
                || server
                    .systems
                    .social
                    .teams
                    .active_team_by_unit(me)
                    .is_some_and(|team| !team.is_party && team.is_member(owner))
        }
        p::SAME_ACCOUNT => owner == me || same_account(server, player, owner),
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

/// `Role` of the expedition leader (`ExpeditionMember.Role`).
const EXPEDITION_LEADER_ROLE: u8 = 0xff;

/// `FUN_392f8af0` plus `FUN_393b3300` case 3.
///
/// `FUN_392f8af0` looks the local character's role policy up by `(expedition id, role)`
/// (`FUN_398a40c0`, the map filled by `FUN_398a4ee0`) and passes when the role is `0xff` or the
/// policy byte at `+0x90` is set. `FUN_392faa00` maps the policy to the `expedition_management`
/// role-policy columns: basis 1 = role (+4), 2 = name (+8), 3 = invite (+0x8a), 4 = expel
/// (+0x8b), 5 = promote (+0x8c), 6 = join_siege (+0x91), 7 = **siege_master (+0x90)**,
/// 8 = dominion_declare (+0x89), 9 = chat (+0x8e), 10 = manager_chat (+0x8f) — the ALB column
/// constants carry the names. So +0x90 is `siege_master`, not the neighbouring join_siege.
///
/// Case 3 then compares the doodad's expedition (`doodad +0x350`) with the local expedition id,
/// or accepts the sentinel. It has **no owner shortcut**: the owner is not read.
fn expedition_allows(server: &GameServer, doodad: &DoodadRef, player: &PlayerRef) -> bool {
    let expedition_id = player.data().unit.expedition_id;
    if expedition_id == 0 {
        return false;
    }
    let Some(expedition) = server.expeditions.get(expedition_id) else {
        return false;
    };
    let Some(member) = expedition.member(player.character_id) else {
        return false;
    };
    if member.role != EXPEDITION_LEADER_ROLE
        && !expedition
            .policy_by_role(member.role)
            .is_some_and(|policy| policy.siege_master)
    {
        return false;
    }
    match doodad_expedition_id(server, doodad) {
        0 => true,
        id => id == expedition_id,
    }
}

/// The doodad's expedition id (`doodad +0x350` in the client). `Doodad.Write` carries the
/// field as 0 (C# `stream.Write(0); // family TODO`), so it is 0 for ordinary doodads. The only
/// perm-3 row is the siege camp 4307 recover; the camp is bound to a dominion declaration and
/// that declaration's attacker expedition is its expedition. INFERRED for any other doodad.
fn doodad_expedition_id(server: &GameServer, doodad: &DoodadRef) -> u32 {
    server
        .systems
        .housing
        .dominions
        .all()
        .into_iter()
        .filter_map(|state| state.declaration)
        .find(|declaration| declaration.hq_obj_id == doodad.obj_id)
        .map_or(0, |declaration| declaration.attacker_expedition_id)
}

fn same_account(server: &GameServer, player: &PlayerRef, owner: u32) -> bool {
    let account = player.data().account_id;
    account != 0 && account == server.names.get_character_account(owner)
}

/// `FUN_393b3300` case 7 / `FUN_39053360(zone group, my nation, 0)`:
/// * `FUN_392fa6b0(my nation)` passes only for a player-founded top-level faction (id above the
///   user-faction threshold and `mother == 0`), so a race faction is refused;
/// * `FUN_392f3510` -> `FUN_3983a730` look the zone group's dominion record up;
/// * its owner (`+4`) goes through `FUN_392f8180`, which climbs the `mother` chain to the
///   top-level faction (`FUN_398a37a0`, `+8` per record).
///
/// `zones.faction_id` is not part of the rule: every shipped siege-zone row has 0 there.
fn nation_owns_zone(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    player: &PlayerRef,
) -> bool {
    use crate::systems::social::nation::expedition_top_level_faction;
    let mine = player.data().unit.faction_id;
    if !server.systems.social.nations.is_nation(mine) {
        return false;
    }
    let Some(group) = zone_group_of(server, world, doodad) else {
        return false;
    };
    let Some(state) = server.systems.housing.dominions.get(group) else {
        return false;
    };
    state.owner_expedition_id != 0
        && expedition_top_level_faction(server, state.owner_expedition_id) == Some(mine)
}

/// `FUN_393b3300` case 8 / `FUN_3932aaf0`: one of the player's houses is in the doodad's zone
/// group (`FUN_397a6c60` returns the `zones.group_id` field; the C# `CharacterCraft` used the
/// same zone group).
fn zone_resident(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    player: &PlayerRef,
) -> bool {
    let Some(group) = zone_group_of(server, world, doodad) else {
        return false;
    };
    let account = player.data().account_id;
    account != 0
        && server
            .systems
            .housing
            .manager
            .by_account(account)
            .iter()
            .any(|house| zone_group_of_house(server, house) == Some(group))
}

fn zone_group_of(server: &GameServer, world: &WorldInstance, doodad: &DoodadRef) -> Option<u32> {
    zone_group_of_zone(server, zone_id_of(world, doodad))
}

fn zone_group_of_house(server: &GameServer, house: &crate::world::HouseRef) -> Option<u32> {
    zone_group_of_zone(server, house.data().unit.transform.zone_id)
}

fn zone_group_of_zone(server: &GameServer, zone_id: u32) -> Option<u32> {
    server
        .game_data
        .zones
        .zone_by_key(zone_id)
        .map(|zone| zone.group_id)
        .filter(|group| *group != 0)
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
