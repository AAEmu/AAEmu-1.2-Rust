//! Red Dragon Egg summon.

// ---- keys

pub(crate) const RED_DRAGON_EGG_SKILL: u32 = 23_150;

// ---- tables

pub(super) fn is_red_dragon_egg_skill(skill_id: u32) -> bool {
    skill_id == RED_DRAGON_EGG_SKILL
}

// ---- hooks

use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::types::skill_result;
use crate::systems::skills::unit_reqs::skill_result_for_key;
use crate::systems::skills::validation::fail;
use crate::world::WorldObject;

/// The egg's authored skill uses Dancing Goddess's Hunt as an effect-level
/// cost. The bow is required for the whole cast, including its spawn effect.
pub(super) fn check_red_dragon_egg(
    server: &GameServer,
    caster: &WorldObject,
    target: &Target,
) -> Result<(), (u8, u32)> {
    const RED_DRAGON_EGG: u32 = 7056;
    const READY_PHASE: u32 = 18_936;
    const DANCING_GODDESS_HUNT: u32 = 29_301;

    let Some(player) = caster.as_player() else {
        return fail("skill_invalid_source");
    };
    let ready = target
        .as_obj()
        .and_then(WorldObject::as_doodad)
        .is_some_and(|egg| {
            let egg = egg.data();
            egg.template_id == RED_DRAGON_EGG && egg.func_group_id == READY_PHASE
        });
    if !ready {
        return fail("skill_invalid_target");
    }
    let items = server.systems.skills.ext.items();
    if items.item_count(server, player, DANCING_GODDESS_HUNT) > 0 {
        return Ok(());
    }
    let result = if Inv::of(player).items_count(DANCING_GODDESS_HUNT, -1) > 0 {
        skill_result::ITEM_LOCKED
    } else {
        skill_result_for_key("skill_need_reagent")
    };
    Err((result, DANCING_GODDESS_HUNT))
}

pub(super) struct RedDragon;

impl super::hooks::Encounter for RedDragon {
    fn effects_ready(
        &self,
        server: &GameServer,
        t: &SkillTemplate,
        caster: &WorldObject,
        target: &Target,
    ) -> Option<Result<(), (u8, u32)>> {
        is_red_dragon_egg_skill(t.id).then(|| check_red_dragon_egg(server, caster, target))
    }
}
