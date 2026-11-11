//! Expedition membership for the siege-participation tests (`seals_tests`, `siege_area_tests`,
//! `hostility_tests`): "participant" is a roster entry **and** a current member of the side's
//! live expedition (`participants::participant_side`), so a fixture has to put its characters
//! into the expedition manager as well as set `unit.expedition_id`.

use crate::data::world_data::SystemFaction;
use crate::server::GameServer;
use crate::systems::social::expedition::{member_from_character, Expedition};
use crate::world::PlayerRef;

/// Makes `player` a member of `expedition_id`, creating the expedition with `player` as its
/// leader when it does not exist yet. The character's `unit.expedition_id` follows, as it does
/// in the guild code.
pub(super) fn join_expedition(server: &GameServer, expedition_id: u32, player: &PlayerRef) {
    let member = member_from_character(
        server,
        expedition_id,
        player,
        !server.expeditions.exists(expedition_id),
    );
    if server.expeditions.exists(expedition_id) {
        server
            .expeditions
            .with_mut(expedition_id, |e| e.members.push(member));
    } else {
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: expedition_id,
                owner_id: player.character_id,
                ..Default::default()
            },
            members: vec![member],
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
    }
    player.data().unit.expedition_id = expedition_id;
}

/// The member leaves (or is kicked from) the expedition: the manager forgets it and the unit's
/// expedition is cleared, exactly what `expedition::leave` / `kick` do. A siege roster entry of
/// the character is left alone: nothing removes it.
pub(super) fn leave_expedition(server: &GameServer, expedition_id: u32, player: &PlayerRef) {
    let character_id = player.character_id;
    server.expeditions.with_mut(expedition_id, |e| {
        e.members.retain(|m| m.character_id != character_id)
    });
    player.data().unit.expedition_id = 0;
}

/// The expedition is dismissed (`is_disbanded`); its members' units keep their id here, which
/// is the worst case for a rule that only trusts the unit.
pub(super) fn disband_expedition(server: &GameServer, expedition_id: u32) {
    server
        .expeditions
        .with_mut(expedition_id, |e| e.is_disbanded = true);
}
