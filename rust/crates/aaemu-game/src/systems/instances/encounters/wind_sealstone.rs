//! Wind Sealstone quest launch.

// ---- keys

const SKILL_ID: u32 = 17_539;

// ---- tables

pub(super) fn is_wind_sealstone_launch_skill(skill_id: u32) -> bool {
    skill_id == SKILL_ID
}

// ---- hooks

use crate::server::GameServer;
use crate::systems::skills::state::SkillRef;
use crate::world::WorldObject;

fn on_skill_fired(server: &GameServer, skill: &SkillRef, caster: &WorldObject) {
    if is_wind_sealstone_launch_skill(skill.id) && caster.transform().instance_id == 0 {
        if let Some(player) = caster.as_player() {
            // Wind Sealstone's area-sphere requirement was validated before firing.
            crate::world::move_guard::grant_quest_launch(server, player);
        }
    }
}

pub(super) struct WindSealstone;

impl super::hooks::Encounter for WindSealstone {
    fn on_skill_fired(&self, server: &GameServer, skill: &SkillRef, caster: &WorldObject) {
        on_skill_fired(server, skill, caster);
    }
}
