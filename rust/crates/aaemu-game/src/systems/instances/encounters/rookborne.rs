//! Rookborne's Amed cannon.

// ---- keys

const CONTROLLER_ID: u32 = 6678;

// ---- tables

pub(super) fn is_amed_cannon_controller(controller_id: u32) -> bool {
    controller_id == CONTROLLER_ID
}

// ---- hooks

use crate::server::GameServer;
use crate::systems::skills::data::SkillControllerTemplate;
use crate::systems::skills::geometry;
use crate::world::{PlayerRef, WorldObject};

fn on_player_skill_controller(
    server: &GameServer,
    t: &SkillControllerTemplate,
    caster: &WorldObject,
    player: &PlayerRef,
) {
    // Amed's cannon starts the player's native launch with its first floating
    // controller. Release the cannon seat so the subsequent flight can replicate.
    if is_amed_cannon_controller(t.id) {
        let seated_in_cannon = geometry::world_of(server, caster)
            .and_then(|world| {
                crate::systems::vehicles::slaves::get_is_mounted(&world, player.obj_id)
            })
            .is_some_and(|(slave, _)| slave.data().unit.template_id == 99);
        if seated_in_cannon {
            crate::systems::vehicles::force_dismount(
                server,
                player,
                crate::systems::vehicles::slaves::attach_reason::NONE,
            );
            crate::world::move_guard::grant_quest_launch(server, player);
        }
    }
}

pub(super) struct Rookborne;

impl super::hooks::Encounter for Rookborne {
    fn on_player_skill_controller(
        &self,
        server: &GameServer,
        t: &SkillControllerTemplate,
        caster: &WorldObject,
        player: &PlayerRef,
    ) {
        on_player_skill_controller(server, t, caster, player);
    }
}
