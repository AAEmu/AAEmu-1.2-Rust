//! Siege hostility: who may attack whom inside a running siege's area.
//!
//! Evidence (10.x, `D:/aaemu-audits/gap-audit-2026-11-10/verify-claims-10x.md` claim 2):
//! the common relationship dispatcher (`sub_39CC17B0`, `Z:2790654-2790950`) reaches the siege
//! rule only while the siege period runs and both units lie inside the siege area; the rule
//! (`sub_390E19B0` -> `sub_39CC3BF0` -> `sub_39CC3110`, `Z:362920-362975`, `Z:2792862-2792880`)
//! maps each unit's expedition to a siege party (the defender expedition or one of the
//! attacker expeditions) and yields:
//! * either unit in no party -> no siege relation, the normal faction / zone rules apply;
//! * the same party -> friendly;
//! * two different attacker parties (the 10.x multi-attacker lists) -> neutral;
//! * otherwise -> hostile.
//!
//! The 1.2 siege record has exactly one attacker expedition (`SiegeState::
//! attacker_expedition_id`, the `SCSiegeState`/`SCSiegeDeclared` attacker team), so the
//! neutral branch is only reachable through the pure [`classify`] helper; it is kept because
//! it is part of the evidenced rule. The 10.x enum numbers the running siege 3, the 1.2
//! client numbers it 4 (`siege.md` 1.2); [`SiegeArea::active_at`] follows the same clock
//! choice as the authority's `tick_dual` (calendar / GM test clock for a calendar
//! materialisation, the real clock for a GM schedule).
//!
//! `can_attack` runs for every hit, so the rule reads the per-zone [`SiegeAreaCache`] (one
//! small copy, rebuilt once a second) instead of cloning a `DominionState`; the cheap party
//! check runs before any world / position lookup.
//!
//! The rule sits in `skills::relation::can_attack` after the Peace, no-PvP and own-mount
//! refusals, so it can never make an attack possible where those protections refuse it.

use chrono::Utc;

use crate::server::GameServer;
use crate::systems::housing::dominion::siege_area::SiegeArea;
use crate::systems::housing::dominion::state::SiegeSide;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::relation::owner_character;

/// One unit's siege party: its side and the expedition that side names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Party {
    pub side: SiegeSide,
    pub expedition_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiegeRelation {
    Friendly,
    Neutral,
    Hostile,
}

/// The pure party rule of `sub_39CC3110`.
pub fn classify(me: Party, target: Party) -> SiegeRelation {
    if me.expedition_id == target.expedition_id {
        SiegeRelation::Friendly
    } else if me.side == SiegeSide::Attacker && target.side == SiegeSide::Attacker {
        SiegeRelation::Neutral
    } else {
        SiegeRelation::Hostile
    }
}

fn party(area: &SiegeArea, expedition_id: u32) -> Option<Party> {
    if expedition_id == 0 {
        return None;
    }
    if expedition_id == area.defender_expedition_id {
        Some(Party {
            side: SiegeSide::Defender,
            expedition_id,
        })
    } else if expedition_id == area.attacker_expedition_id {
        Some(Party {
            side: SiegeSide::Attacker,
            expedition_id,
        })
    } else {
        None
    }
}

/// The siege relation between two units, or `None` when the siege rule does not apply (no
/// running siege, either unit outside the area, or either unit in no siege party). `None`
/// means the caller keeps its normal faction / zone decision.
pub fn relation(server: &GameServer, me: &Target, target: &Target) -> Option<SiegeRelation> {
    let me_object = me.as_obj()?;
    let target_object = target.as_obj()?;
    let me_owner = owner_character(server, me_object)?;
    let target_owner = owner_character(server, target_object)?;
    let me_expedition = me_owner.data().unit.expedition_id;
    let target_expedition = target_owner.data().unit.expedition_id;
    let me_transform = me_object.transform();
    let target_transform = target_object.transform();
    if me_transform.instance_id != target_transform.instance_id {
        return None;
    }
    let group = server
        .game_data
        .zones
        .zone_by_key(target_transform.zone_id)?
        .group_id;
    if server
        .game_data
        .zones
        .zone_by_key(me_transform.zone_id)
        .map(|zone| zone.group_id)
        != Some(group)
    {
        return None;
    }
    // Cheap gate first: the cached window and the party mapping, before any world lookup.
    let area = server.systems.housing.siege_areas.get(group)?;
    if !area.active_at(
        Utc::now().timestamp(),
        server.systems.housing.calendar.now(),
    ) {
        return None;
    }
    let me_party = party(&area, me_expedition)?;
    let target_party = party(&area, target_expedition)?;
    // Only a pair that is bound by the siege rule pays for the area / position test.
    let me_world = server.world.world_of(me_object)?;
    let target_world = server.world.world_of(target_object)?;
    let me_pos = me_world.world_pos_rot(me_object);
    let target_pos = target_world.world_pos_rot(target_object);
    if !area.inside(me_transform.instance_id, me_pos.x, me_pos.y)
        || !area.inside(target_transform.instance_id, target_pos.x, target_pos.y)
    {
        return None;
    }
    Some(classify(me_party, target_party))
}

/// `Some(true)` = the siege rule makes the pair attackable, `Some(false)` = it makes them
/// friendly / neutral, `None` = the normal rules still decide.
pub fn siege_can_attack(server: &GameServer, me: &Target, target: &Target) -> Option<bool> {
    relation(server, me, target).map(|relation| relation == SiegeRelation::Hostile)
}
