//! The 1.2 NPC interaction reach, shared by the interaction window and the store transactions.
//!
//! The 1.2 client opens and keeps the NPC interaction window only within 3.0 m of the target's
//! shape (`FUN_393bef10` / `FUN_3932a3b0`, both compare against `sqrt(9.0)` through
//! `FUN_39093860` / `FUN_39093800`) but does not gate the packets sent from that window, so the
//! server has to accept the same reach. `system/npcs/npc/packets.rs` (window open) and every
//! store transaction call these helpers; the tolerance on top of the client reach is the
//! [`InteractionConfig::npc_reach_tolerance_meters`](crate::systems::npcs::config::InteractionConfig)
//! slack for movement and latency (INFERRED, default 2.0 m).

use crate::server::GameServer;
use crate::systems::npcs::config::NPC_INTERACTION_REACH_METERS;
use crate::systems::skills::geometry::{distance_to, Target};
use crate::world::{DoodadRef, PlayerRef, WorldObject};

/// How far from the target's shape the 1.2 interaction window stays open: the client's 3.0 m
/// plus the configured movement / latency tolerance.
pub fn interaction_reach(server: &GameServer) -> f32 {
    NPC_INTERACTION_REACH_METERS + server.config.interaction.npc_reach_tolerance_meters
}

/// Shape-edge distance from `player` to `target`, measured exactly like the interaction window
/// check: both scaled actor radii are subtracted. A doodad has no actor model, so its placement
/// radius (`sim_radius`, the radius the warehouse / auction doodad reach uses) stands in for its
/// footprint.
pub fn interaction_distance(server: &GameServer, player: &PlayerRef, target: &WorldObject) -> f32 {
    let distance = distance_to(
        server,
        &Target::Obj(WorldObject::from(player)),
        &Target::Obj(target.clone()),
        true,
    );
    if let WorldObject::Doodad(doodad) = target {
        return (distance - doodad_radius(server, doodad)).max(0.0);
    }
    distance
}

fn doodad_radius(server: &GameServer, doodad: &DoodadRef) -> f32 {
    let template_id = doodad.data().template_id;
    server
        .game_data
        .systems
        .npcs
        .doodad
        .template(template_id)
        .map_or(0.0, crate::systems::npcs::doodad::placement::radius)
}

/// Whether `player` stands within [`interaction_reach`] of `target`'s shape.
pub fn within_interaction_reach(
    server: &GameServer,
    player: &PlayerRef,
    target: &WorldObject,
) -> bool {
    interaction_distance(server, player, target) <= interaction_reach(server)
}
