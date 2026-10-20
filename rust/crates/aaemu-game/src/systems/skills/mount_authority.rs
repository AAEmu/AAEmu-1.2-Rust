//! Client-side mate cast authority. Skill eligibility and slave seat mapping are separate rules.

use crate::systems::vehicles::data::attach_point;
use crate::world::{MateRef, PlayerRef, SlaveRef, WorldObject};

pub(super) fn may_use_mate(player: &PlayerRef, mate: &MateRef) -> bool {
    let (point, riding, parent) = {
        let player = player.data();
        (
            player.attached_point,
            player.is_riding,
            player.unit.transform.parent_obj_id,
        )
    };
    let mate_data = mate.data();
    if mate_data.owner_obj_id == player.obj_id {
        return true; // Pet commands also work while the owner is unmounted.
    }
    riding
        && point != attach_point::NONE
        && parent == mate.obj_id
        && mate_data
            .passenger(point)
            .is_some_and(|seat| seat.obj_id == player.obj_id)
}

/// Whether `player` summoned `slave` (by object id, or by character id after a relog).
fn summoned_by(slave: &SlaveRef, player: &PlayerRef, character_id: u32) -> bool {
    let data = slave.data();
    data.summoner_obj_id == player.obj_id || (character_id != 0 && data.summoner_id == character_id)
}

/// A slave (ship, cannon, cart, ...) answers the skills of its owner and of the characters
/// attached to it (driver, gunner, passengers); a cannon or a harpoon child also answers the
/// owner of the slave it is mounted on.
pub(super) fn may_use_slave(
    server: &crate::server::GameServer,
    player: &PlayerRef,
    slave: &SlaveRef,
) -> bool {
    let parent = player.data().unit.transform.parent_obj_id;
    let character_id = player.character_id;
    if parent == slave.obj_id || summoned_by(slave, player, character_id) {
        return true;
    }
    let (attached, bearer) = {
        let data = slave.data();
        (
            data.attached_characters
                .iter()
                .any(|(_, obj)| *obj == player.obj_id),
            data.owner_obj_id,
        )
    };
    if attached {
        return true;
    }
    // A child slave mounted on another slave of the player (one level, like the attach tree).
    bearer != 0
        && super::geometry::world_of(server, &WorldObject::from(player))
            .and_then(|w| w.slave(bearer))
            .is_some_and(|bearer| summoned_by(&bearer, player, character_id))
}

/// Who may a `Mount` caster packet of `player` name as the caster: the player itself, a mate
/// the player may use, or a slave the player owns or is attached to. Any other object (NPC,
/// doodad, another player, a foreign slave) is refused.
pub(super) fn may_cast_as(
    server: &crate::server::GameServer,
    player: &PlayerRef,
    caster: &WorldObject,
) -> bool {
    match caster {
        WorldObject::Player(p) => p.obj_id == player.obj_id,
        WorldObject::Mate(mate) => may_use_mate(player, mate),
        WorldObject::Slave(slave) => may_use_slave(server, player, slave),
        _ => false,
    }
}
