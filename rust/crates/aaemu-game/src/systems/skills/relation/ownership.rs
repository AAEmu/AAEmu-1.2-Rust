use crate::server::GameServer;
use crate::world::{GimmickRef, PlayerRef, WorldObject};

use super::super::geometry::Target;
use super::faction_of;

/// How many spawner links [`gimmick_owner`] follows. A gimmick can be spawned by another gimmick
/// (a bomb whose own skill spawns fragments) and object ids are reused as soon as they are
/// released, so two live gimmicks can name each other: the chain is bounded, not trusted.
const GIMMICK_OWNER_HOPS: usize = 4;

/// The character a gimmick acts for: the owner of the unit that spawned it
/// (`Gimmick.SpawnerUnitId`, set from the caster by `SpawnGimmickEffect`), resolved like a mate /
/// slave owner. `None` when the spawner is gone or is a doodad / NPC with no owning character.
fn gimmick_owner(server: &GameServer, gimmick: &GimmickRef) -> Option<PlayerRef> {
    let mut spawner = gimmick.data().spawner_unit_id;
    for _ in 0..GIMMICK_OWNER_HOPS {
        if spawner == 0 {
            return None;
        }
        match server.world.object(spawner)? {
            WorldObject::Gimmick(g) => spawner = g.data().spawner_unit_id,
            other => return owner_character(server, &other),
        }
    }
    None
}

/// `GetOwnerCharacter()`: the character itself, the owner of a mate / slave, the owner of the unit
/// that spawned a gimmick, else `None`.
///
/// **Inferred (no 1.2 server evidence):** the gimmick arm. The 1.2 C# cast a gimmick's skill from
/// its spawner unit, so every rule keyed on the character applied to it; the gimmick is now the
/// cast source (its skills need a gimmick source, see `skills/validation.rs`), and this mapping
/// keeps those rules (Peace, no-PvP, siege hostility, tagging, crime evidence) on the thrower.
pub fn owner_character(server: &GameServer, o: &WorldObject) -> Option<PlayerRef> {
    match o {
        WorldObject::Player(p) => Some(p.clone()),
        WorldObject::Gimmick(g) => gimmick_owner(server, g),
        WorldObject::Mate(m) => {
            let owner = m.data().owner_obj_id;
            (owner > 0)
                .then(|| server.world.object(owner))
                .flatten()
                .and_then(|o| owner_character(server, &o))
        }
        WorldObject::Slave(s) => {
            let (summoner, owner) = {
                let d = s.data();
                (d.summoner_obj_id, d.owner_obj_id)
            };
            let id = if summoner != 0 { summoner } else { owner };
            if id == 0 {
                return world_slave_driver(server, s);
            }
            server
                .world
                .object(id)
                .and_then(|o| owner_character(server, &o))
        }
        _ => None,
    }
}

/// The character a hit or kill by `unit` is credited to (tagging, kill experience, loot, quest
/// events, PvP honor): the player itself, or for a gimmick the character it acts for. A mate or
/// slave keeps its own credit rules, so this is not [`owner_character`]. The death packet and
/// every other broadcast keep naming the unit that really dealt the blow.
pub fn credited_player(server: &GameServer, unit: &WorldObject) -> Option<PlayerRef> {
    match unit {
        WorldObject::Player(p) => Some(p.clone()),
        WorldObject::Gimmick(g) => gimmick_owner(server, g),
        _ => None,
    }
}

/// The character in the gunner / driver seat of a slave nobody owns: a world cannon placed by a
/// `slave_spawns` entry, such as the Bloodhand Base Cannon (slave 57, faction 2 "Neutral") on
/// the Marooned Isle rampart that "Rout the Bloodhands" (2325) sends players to. Such a slave
/// acts for that character (its faction, owner and kill credit); `None` for a summoned or owned
/// slave. Inferred (no 1.2 server evidence): the C# had no owner for these slaves, so a manned
/// world cannon stayed Neutral and could damage nothing.
pub fn world_slave_driver(
    server: &GameServer,
    slave: &crate::world::SlaveRef,
) -> Option<PlayerRef> {
    let driver = {
        let d = slave.data();
        if d.summoner_obj_id != 0 || d.owner_obj_id != 0 {
            return None;
        }
        d.attached_characters
            .iter()
            .find(|(point, _)| *point == crate::models::unit::attach_point::DRIVER)
            .map(|(_, id)| *id)
    }?;
    server.world.character_by_obj_id(driver)
}

/// The faction `me` attacks with: a manned world slave fights with its gunner's faction
/// ([`world_slave_driver`]); every other unit with its own.
pub(super) fn acting_faction(server: &GameServer, me: &Target) -> u32 {
    if let Some(driver) = me
        .as_obj()
        .and_then(WorldObject::as_slave)
        .and_then(|s| world_slave_driver(server, s))
    {
        return driver.data().unit.faction_id;
    }
    faction_of(me)
}
