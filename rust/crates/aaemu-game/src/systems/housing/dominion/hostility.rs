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
//! A party is the set of **participants** of one side (`participants::participant_side`): a
//! roster entry whose member still belongs to the side's expedition. A guild member who never
//! joined the roster, or left it, is in no party.
//!
//! The rule sits in the relation engine, `skills::relation::relation_to`
//! (`BaseUnit.GetRelationStateTo`), not in `can_attack`: the skill target filters, the effect
//! loop (`friendly` / `non_friendly` effects), the crime check of the damage effect, the PvP
//! honor and `can_attack` all read the relation from there, so two guilds of one nation are
//! hostile for all of them and not only for the attack permission. `can_attack` keeps its Peace,
//! no-PvP and own-mount refusals ahead of the relation, so the rule can never make an attack
//! possible where those protections refuse it.
//!
//! `relation_to` runs for every effect of every skill, so the rule reads the per-zone
//! [`SiegeAreaCache`] (one small copy, rebuilt once a second) instead of cloning a
//! `DominionState`; it returns before it touches a unit while no siege has an area, and the
//! cheap party check runs before any world / position lookup.

use chrono::Utc;

use crate::server::GameServer;
use crate::systems::housing::dominion::siege_area::SiegeArea;
use crate::systems::housing::dominion::state::SiegeSide;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::relation::owner_character;
use crate::systems::skills::types::RelationState;
use crate::world::PlayerRef;

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

impl SiegeRelation {
    /// The relation state `BaseUnit.GetRelationStateTo` reports for the pair.
    pub fn relation_state(self) -> RelationState {
        match self {
            Self::Friendly => RelationState::Friendly,
            Self::Neutral => RelationState::Neutral,
            Self::Hostile => RelationState::Hostile,
        }
    }
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

/// The siege party of a unit's owner character: its side and expedition when the character is a
/// participant of the siege (`participants::participant_side`, through the snapshot of the last
/// sweep), `None` for anybody else: no roster entry, or a roster entry whose member left the
/// side's expedition since.
fn party(server: &GameServer, area: &SiegeArea, owner: &PlayerRef) -> Option<Party> {
    let expedition_id = owner.data().unit.expedition_id;
    let side = server.systems.housing.siege_areas.participant_side(
        area,
        owner.character_id,
        expedition_id,
    )?;
    Some(Party {
        side,
        expedition_id,
    })
}

/// The siege relation between two units, or `None` when the siege rule does not apply (no
/// running siege, either unit outside the area, or either unit in no siege party). `None`
/// means the caller keeps its normal faction / zone decision.
pub fn relation(server: &GameServer, me: &Target, target: &Target) -> Option<SiegeRelation> {
    // No siege has an area (the usual case): nothing to look at.
    if server.systems.housing.siege_areas.is_empty() {
        return None;
    }
    let me_object = me.as_obj()?;
    let target_object = target.as_obj()?;
    let me_owner = owner_character(server, me_object)?;
    let target_owner = owner_character(server, target_object)?;
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
    let me_party = party(server, &area, &me_owner)?;
    let target_party = party(server, &area, &target_owner)?;
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
