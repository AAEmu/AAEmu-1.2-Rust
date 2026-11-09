//! Siege larceny window (`ui_texts` 3334 `in_siege_warning`).
//!
//! Evidence (grade A, `D:/aaemu-audits/gap-audit-2026-11-02/research/siege-larceny-web.md` and
//! `round6-trial-larceny-2.md` 2):
//! * Korean: "공성측이 수호탑을 점령하고 있는 동안에는, 농작물과 설치물을 타인이 사용할 수
//!   있습니다." = while the attacking side occupies the Archeum Lodestone (수호탑), others may
//!   use the crops and installations.
//! * English (`localized_texts` 172569): "If a guard tower is seized by opposing forces,
//!   opponents can loot any crops, animals, or trees on those lands."
//!
//! The Korean says "anyone else" (타인), the English says "opponents": a wording conflict. The
//! server emulates the EU client, so [`SiegeLarcenyMode::Opponents`] is the default; `Anyone`
//! implements the Korean reading. No 1.2 table implements the rule, so the rule set below is
//! server logic and INFERRED from the text only (see `PORTING.md`):
//! * the window is open only while the territory's siege is active **and** its lodestone is
//!   destroyed (`DominionState::tower_destroyed`) — the closest state to "occupied";
//! * scope: the territory's zone group, and only farm/garden doodads on private land
//!   (`doodad_owner_type::HOUSING`), classified by the 1.2 `doodad_groups` names:
//!   group 2 "벌채 - 나무" (trees), 3 "채집 - 약초" (herb / flower garden crops),
//!   5 "축산 - 가축" (livestock), 12 "농사 - 작물" (crops) and 40 "농사 - 해양 작물" (marine
//!   crops). Houses, furniture, coffers and storage are not farm groups and keep their own
//!   gates;
//! * `Anyone` allows any non-owner; `Opponents` uses the existing faction relation (the same
//!   relation `can_attack` uses) between the player and the house's owner faction, which
//!   `update_owned_housing_faction` keeps synced with the owner character;
//! * `DoodadFuncRecoverItem` stays behind the normal house permission: the window grants use /
//!   harvest of produce, not picking up the installation itself (barns, racks, coops, young
//!   animals and flower beds are recoverable items only for the owner / a permitted player);
//! * evidence is suppressed inside the window by default (`Housing.SiegeLarcenyCrime = false`);
//!   an explicit `true` turns the normal footprint evidence back on.
//!
//! Client: no server packet change. The 1.2 client shows `inSiegeWarning` from its own
//! `X2House:IsSiegePeriod()` (Lua `housing/maintain_window.alb`, `housing/material.alb`) and the
//! actual interaction gate is the server's (`SCErrorMsg` `interaction_permission_deny` 232), so
//! the client warning is broader than the server window and cannot narrow it.

use crate::models::doodad::doodad_owner_type;
use crate::server::GameServer;
use crate::systems::housing::config::SiegeLarcenyMode;
use crate::systems::skills::relation::faction_relation;
use crate::systems::skills::types::RelationState;
use crate::systems::social::nation::faction_exists;
use crate::world::{DoodadRef, HouseRef, PlayerRef};

/// `doodad_groups` ids of the 1.2 farm / garden doodads named by the larceny text. Group 3
/// (`채집 - 약초`) holds the plantable herbs and flowers (rose, lily, cotton, lavender,
/// mushroom, ginseng, sunflower, ...); the other `item_spawn_doodads` groups are mining nodes,
/// crafting benches, quest/level/test props, doors, housing furniture and the territory statue.
pub const LARCENY_DOODAD_GROUP_IDS: [u32; 5] = [2, 3, 5, 12, 40];

/// The configured larceny policy (`Housing.SiegeLarceny`).
pub fn mode(server: &GameServer) -> SiegeLarcenyMode {
    server.config.housing.siege_larceny
}

/// Whether the normal theft evidence is kept inside the window
/// (`Housing.SiegeLarcenyCrime`).
pub fn crime_enabled(server: &GameServer) -> bool {
    server.config.housing.siege_larceny_crime
}

/// A farm / garden doodad of the 1.2 data (trees, herbs/flowers, livestock, crops, marine
/// crops).
pub fn is_larceny_doodad(server: &GameServer, doodad: &DoodadRef) -> bool {
    let template_id = doodad.data().template_id;
    server
        .game_data
        .systems
        .npcs
        .doodad
        .template(template_id)
        .is_some_and(|template| LARCENY_DOODAD_GROUP_IDS.contains(&template.group_id))
}

/// The house a doodad is attached to (private land), if any.
pub fn house_of_doodad(server: &GameServer, doodad: &DoodadRef) -> Option<HouseRef> {
    let (owner_type, owner_db_id) = {
        let d = doodad.data();
        (d.owner_type, d.owner_db_id)
    };
    if owner_type != doodad_owner_type::HOUSING || owner_db_id == 0 {
        return None;
    }
    crate::systems::housing::house_by_id(server, owner_db_id)
}

fn zone_group_of_house(server: &GameServer, house: &HouseRef) -> Option<u32> {
    let zone_id = house.data().unit.transform.zone_id;
    server
        .game_data
        .zones
        .zone_by_key(zone_id)
        .map(|zone| zone.group_id)
}

/// Whether the territory's siege has destroyed its lodestone (the window is open).
pub fn window_open(server: &GameServer, zone_group_id: u32) -> bool {
    mode(server) != SiegeLarcenyMode::Off
        && server
            .systems
            .housing
            .dominions
            .siege_larceny_window_open(zone_group_id, chrono::Utc::now().timestamp())
}

/// Whether the window is open for the house's territory.
pub fn window_open_for_house(server: &GameServer, house: &HouseRef) -> bool {
    zone_group_of_house(server, house).is_some_and(|group| window_open(server, group))
}

/// Whether a house-attached farm doodad is inside an open window (the territory only).
pub fn covers_doodad(server: &GameServer, doodad: &DoodadRef) -> bool {
    is_larceny_doodad(server, doodad)
        && house_of_doodad(server, doodad)
            .is_some_and(|house| window_open_for_house(server, &house))
}

/// Whether the window lets `player` use the house's farm doodad. The caller has already found
/// the normal house permission denied.
pub fn window_allows_house_doodad(
    server: &GameServer,
    house: &HouseRef,
    doodad: &DoodadRef,
    player: &PlayerRef,
) -> bool {
    if !is_larceny_doodad(server, doodad) || !window_open_for_house(server, house) {
        return false;
    }
    let (owner_id, house_faction) = {
        let h = house.data();
        (h.unit.owner_id, h.unit.faction_id)
    };
    if owner_id == 0 || owner_id == player.character_id {
        return false;
    }
    match mode(server) {
        SiegeLarcenyMode::Off => false,
        // The Korean wording: anyone other than the owner.
        SiegeLarcenyMode::Anyone => true,
        // The English wording: hostile to the owner. The house faction mirrors the owner
        // character (`update_owned_housing_faction`), and this is the relation `can_attack`
        // consults. Unknown / zero factions are not opponents.
        SiegeLarcenyMode::Opponents => {
            let player_faction = player.data().unit.faction_id;
            player_faction != 0
                && house_faction != 0
                && faction_exists(server, player_faction)
                && faction_exists(server, house_faction)
                && faction_relation(server, player_faction, house_faction) == RelationState::Hostile
        }
    }
}

/// The shared `Doodad.Use` gate for house-attached doodads. Only the farm groups named by the
/// larceny text are touched here, so houses, furniture, doors and coffers keep their own
/// permission checks. A covered doodad is allowed by the normal house permission or by the
/// window; every other doodad is allowed here and gated by its own func.
pub fn house_doodad_use_allowed(
    server: &GameServer,
    doodad: &DoodadRef,
    player: &PlayerRef,
) -> bool {
    let Some(house) = house_of_doodad(server, doodad) else {
        return true;
    };
    if !is_larceny_doodad(server, doodad) {
        return true;
    }
    crate::systems::housing::allowed_to_interact(server, &house, player)
        || window_allows_house_doodad(server, &house, doodad, player)
}

/// Whether the normal theft evidence must be suppressed for this doodad. `false` (the default
/// `Housing.SiegeLarcenyCrime`) suppresses it inside the window; `true` keeps it.
pub fn suppresses_theft_evidence(server: &GameServer, doodad: &DoodadRef) -> bool {
    !crime_enabled(server) && covers_doodad(server, doodad)
}
