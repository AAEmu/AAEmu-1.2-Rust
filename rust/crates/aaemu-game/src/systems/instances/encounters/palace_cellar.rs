//! Palace Cellar (Sal temple): Melkina, Tushan, the flame braziers, the boulder and the doors.

// ---- keys

/// Palace Cellar's world template.
pub const WORLD: &str = "instance_sal_temple";
/// Palace Cellar's zone.
pub const ZONE: u32 = 240;

// ---- tables

/// The raised passages have another walkable floor only a few meters underneath.
pub(super) fn raised_floor_drop(zone_id: u32) -> Option<f32> {
    (zone_id == 240).then_some(2.5)
}

/// Akmit (cinema 9, `id_240_01`) reaches the players of the room through an area skill that
/// needs a playback cue.
pub(super) fn plays_cinema_directly(cinema_id: u32) -> bool {
    cinema_id == 9
}

/// The Cellar flame follows the brazier's phase, even when the bucket is used
/// beside a different prop. Phase changes must remove the previous fire area.
pub(super) fn is_flame_clout(template_id: u32, buff_id: u32) -> bool {
    matches!(template_id, 4109 | 4741) && buff_id == 2783
}

/// The burning boulder that rolls down the Cellar ramp.
pub const ROLLING_BOULDER: u32 = 42;

/// Both rooms' no-attack areas accept Melkina's original open cue.
pub(super) fn doodad_hit_skill(
    skill_id: u32,
    doodad_template_id: impl FnOnce() -> u32,
) -> Option<u32> {
    (skill_id == 20615 && doodad_template_id() == 5947).then_some(17422)
}

/// Tushan's reward is tied to his death. Require neither another unit within
/// the 2 m plot shape nor a height lookup that can place it beneath the floor.
pub(super) fn plot_targets_caster(skill_id: u32, event_id: u32) -> bool {
    skill_id == 20615 && event_id == 5347
}

/// The Sal temple's pools are indoor water volumes.
pub(super) fn has_indoor_pools(template_name: &str) -> bool {
    template_name == WORLD
}

/// The Palace Cellar doors reuse the closed mesh for the open animation.
pub(super) fn door_reuses_closed_mesh(template_id: u32, func_group: u32) -> bool {
    matches!(
        (template_id, func_group),
        (4104, 9965 | 10027) | (4106, 10005 | 10006) | (4128, 10018 | 10019) | (4577, 11228)
    )
}

// ---- hooks

use crate::models::buff::Buff;
use crate::server::GameServer;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::geometry::{self, world_of};
use crate::systems::skills::water::character_water_probe;
use crate::world::{PlayerRef, WorldInstance, WorldObject};

const PALACE_CELLAR: &str = "instance_sal_temple";
pub(super) const AKMIT_ROAR_BUFF: u32 = 2574;

/// Route scripted door hits to the boss's own gate, even if it died in the sunken part
/// of the room. The gates and their no-attack areas share templates. Melkina's
/// pair is at Y 250, and Tushan's is at Y 328–331.
fn route_palace_cellar_gate(
    server: &GameServer,
    caster: &WorldObject,
    skill_id: u32,
    units: &mut Vec<WorldObject>,
) {
    if !matches!(skill_id, 17421..=17423 | 20615) {
        return;
    }
    let Some(npc) = caster.as_npc() else { return };
    let north = match npc.data().unit.template_id {
        9809 => false, // Melkina
        9796 => true,  // Tushan
        _ => return,
    };
    let Some(world) = world_of(server, caster).filter(|w| w.template.name == "instance_sal_temple")
    else {
        return;
    };
    // Door cues have ten target slots. Nearby jars, NPCs and other props can
    // fill them before the gate, especially when the boss dies far inside.
    // Melkina's chest effect targets the caster once and needs no area targets.
    units.clear();
    units.extend(
        world
            .all_doodads()
            .into_iter()
            .filter(|doodad| {
                let d = doodad.data();
                matches!(d.template_id, 4577 | 5947) && (d.transform.y > 300.0) == north
            })
            .map(WorldObject::from),
    )
}

/// Melkina's (10073) exit skill 16600 opens the barrier (4580).
pub(super) fn on_apply_effects(server: &GameServer, t: &SkillTemplate, caster: &WorldObject) {
    if t.id == 16600
        && caster
            .as_npc()
            .is_some_and(|n| n.data().unit.template_id == 10073)
    {
        if let Some(world) =
            world_of(server, caster).filter(|w| w.template.name == "instance_sal_temple")
        {
            if let Some(barrier) = world
                .all_doodads()
                .into_iter()
                .find(|d| d.data().template_id == 4580)
            {
                // Melkina's exit completes the barrier's timed sequence immediately.
                crate::systems::npcs::doodad::manager::do_change_phase(
                    server,
                    &world,
                    &barrier,
                    Some(caster),
                    12282,
                );
            }
        }
    }
}

/// Contact with the pool puts out Akmit's fire even while the player's head is above water.
pub(super) fn water_extinguishes_akmit_roar(server: &GameServer, obj: &WorldObject) -> bool {
    obj.as_player().is_some()
        && geometry::world_of(server, obj).is_some_and(|world| {
            world.template.name == PALACE_CELLAR
                && crate::systems::ai::physics::is_water(
                    server,
                    world.id,
                    character_water_probe(server, &world, obj),
                )
                .is_some()
        })
}

/// Akmit's Roar is not added to a player in the pool.
pub(super) fn blocks_buff(server: &GameServer, owner: &WorldObject, buff: &Buff) -> bool {
    buff.template.id == AKMIT_ROAR_BUFF && water_extinguishes_akmit_roar(server, owner)
}

/// The pool removes Akmit's Roar.
pub(super) fn on_character_water(
    server: &GameServer,
    world: &WorldInstance,
    p: &PlayerRef,
    obj: &WorldObject,
) {
    if world.template.name == PALACE_CELLAR && p.data().unit.buffs.check_buff(AKMIT_ROAR_BUFF) {
        crate::systems::skills::buffs::remove_buff(server, obj, AKMIT_ROAR_BUFF);
    }
}

/// Palace Cellar's encounter hooks.
pub struct PalaceCellar;

impl super::hooks::Encounter for PalaceCellar {
    fn on_apply_effects(
        &self,
        server: &GameServer,
        t: &SkillTemplate,
        caster: &WorldObject,
    ) -> bool {
        on_apply_effects(server, t, caster);
        false
    }

    fn area_targets(
        &self,
        server: &GameServer,
        caster: &WorldObject,
        t: &SkillTemplate,
        units: &mut Vec<WorldObject>,
    ) {
        route_palace_cellar_gate(server, caster, t.id, units)
    }

    fn blocks_buff(&self, server: &GameServer, owner: &WorldObject, buff: &Buff) -> bool {
        blocks_buff(server, owner, buff)
    }

    fn on_character_water(
        &self,
        server: &GameServer,
        world: &WorldInstance,
        p: &PlayerRef,
        obj: &WorldObject,
    ) {
        on_character_water(server, world, p, obj)
    }
}
