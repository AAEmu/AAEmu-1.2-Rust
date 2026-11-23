use crate::server::GameServer;
use crate::systems::skills::types::{SkillCastTarget, SkillCaster};
use crate::world::{PlayerRef, WorldObject};

use super::super::item_effects;

// ---------------------------------------------------------------------------------------------
// economy's own slots: MailHousingHooks, CraftHooks, ItemEffectHooks
// ---------------------------------------------------------------------------------------------

/// The economy hooks that compose the other subsystems directly (housing, npcs, skills,
/// quests).
pub struct EconomyComposite;

impl super::mail::MailHousingHooks for EconomyComposite {
    fn house_exists(&self, server: &GameServer, house_id: u32) -> bool {
        crate::systems::housing::house_by_id(server, house_id).is_some()
    }

    fn house_owner_id(&self, server: &GameServer, house_id: u32) -> Option<u32> {
        crate::systems::housing::house_by_id(server, house_id).map(|h| h.data().unit.owner_id)
    }

    fn pay_weekly_tax(&self, server: &GameServer, house_id: u32) -> bool {
        crate::systems::housing::pay_weekly_tax(server, house_id)
    }

    fn pay_weekly_tax_for(&self, server: &GameServer, house_id: u32, payer_id: u32) -> bool {
        crate::systems::housing::house_by_id(server, house_id).is_some_and(|h| {
            crate::systems::housing::manager::pay_weekly_tax_for(server, &h, Some(payer_id))
        })
    }
}

impl super::craft::CraftHooks for EconomyComposite {
    fn start_craft_cast(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        skill_id: u32,
        doodad_obj_id: u32,
        cast_time_multiplier: f32,
    ) -> u8 {
        use crate::systems::skills::state::SkillRun;
        let Some(t) = server.game_data.systems.skills.skill(skill_id).cloned() else {
            tracing::error!("Craft: unknown skill {skill_id}");
            return crate::systems::skills::types::skill_result::INVALID_SKILL;
        };
        let skill = SkillRun::new(t, 1);
        skill.state().cast_time_multiplier = cast_time_multiplier;
        let me = WorldObject::from(player);
        let mut target = SkillCastTarget::Doodad(doodad_obj_id);
        crate::systems::skills::skill::use_skill(
            server,
            &skill,
            &me,
            SkillCaster::Unit(player.obj_id),
            &mut target,
            None,
            false,
        )
        .0
    }

    fn cancel_craft_skill(&self, server: &GameServer, player: &PlayerRef) {
        let me = WorldObject::from(player);
        let task = me
            .with_unit(|u| u.unit().skill_state.skill_task.clone())
            .flatten();
        if let Some(t) = task {
            t.skill.state().cancelled = true;
        }
        crate::systems::skills::skill::interrupt_skills(server, &me);
    }

    fn account_house_zone_ids(&self, server: &GameServer, account_id: u32) -> Vec<u32> {
        server
            .systems
            .housing
            .manager
            .by_account(account_id)
            .iter()
            .map(|h| h.data().unit.transform.zone_id)
            .collect()
    }

    fn house_actability_bonus(
        &self,
        server: &GameServer,
        x: f32,
        y: f32,
        actability_group_id: i32,
    ) -> u32 {
        crate::systems::housing::act_ability_bonus_at(server, actability_group_id, x, y)
    }

    fn house_actability_bonus_in_world(
        &self,
        server: &GameServer,
        instance_id: u32,
        x: f32,
        y: f32,
        actability_group_id: i32,
    ) -> u32 {
        crate::systems::housing::act_ability_bonus_at_in_world(
            server,
            instance_id,
            actability_group_id,
            x,
            y,
        )
    }

    fn on_craft(&self, server: &GameServer, player: &PlayerRef, craft_id: u32) {
        crate::systems::quests::events::on_craft(server, player, craft_id);
    }
}

impl item_effects::ItemEffectHooks for EconomyComposite {
    fn in_public_farm(&self, server: &GameServer, player: &PlayerRef) -> bool {
        let me = WorldObject::from(player);
        let Some(world) = server.world.world_of(&me) else {
            return false;
        };
        let p = world.world_pos_rot(&me);
        crate::systems::npcs::public_farm::in_public_farm(server, &world, (p.x, p.y, p.z))
    }

    fn house_at(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        x: f32,
        y: f32,
    ) -> Option<item_effects::HouseAtLocation> {
        let instance_id = player.data().unit.transform.instance_id;
        let house = crate::systems::housing::house_at_location_in_world(server, instance_id, x, y)?;
        let house_id = house.data().id;
        Some(item_effects::HouseAtLocation {
            house_id,
            house_obj_id: house.obj_id,
            allowed_to_interact: crate::systems::housing::allowed_to_interact(
                server, &house, player,
            ),
        })
    }

    fn spawn_backpack_doodad(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        spawn: &item_effects::BackpackDoodadSpawn,
    ) -> bool {
        use crate::systems::npcs::doodad::manager;
        let me = WorldObject::from(player);
        let Some(world) = server.world.world_of(&me) else {
            return false;
        };
        let Some(doodad) =
            manager::create(server, &world, 0, spawn.doodad_template_id, Some(&me), true)
        else {
            tracing::warn!(
                "Doodad {}, from BackpackDoodadId could not be created",
                spawn.doodad_template_id
            );
            return false;
        };
        let zone_id = player.data().unit.transform.zone_id;
        {
            let mut d = doodad.data();
            d.is_persistent = true;
            d.transform.instance_id = world.id;
            d.transform.zone_id = zone_id;
            let [rx, ry, rz] = spawn.rot;
            d.transform
                .set_position(spawn.x, spawn.y, spawn.z, rx, ry, rz);
            d.attach_point = crate::models::unit::attach_point::NONE;
            d.item_id = spawn.item_id;
            d.item_template_id = spawn.item_template_id;
            d.ucc_id = spawn.ucc_id;
            d.scale = 1.0;
            d.plant_time = chrono::Utc::now().timestamp();
        }
        if let Some(h) = spawn.house {
            if let Some(house) = world
                .house(h.house_obj_id)
                .filter(|house| house.data().id == h.house_id)
            {
                {
                    let mut d = doodad.data();
                    d.owner_db_id = h.house_id;
                    d.owner_type = crate::models::doodad::doodad_owner_type::HOUSING;
                    d.parent_obj_id = h.house_obj_id;
                }
                crate::systems::housing::manager::mark_client_parent(server, &doodad);
                // BackpackDoodadSpawn supplies world coordinates. Save stores the local
                // transform, so convert before parenting just as player crop placement does.
                crate::systems::npcs::doodad::funcs::world::attach_keeping_world(
                    &world,
                    &WorldObject::from(&house),
                    &WorldObject::from(&doodad),
                );
            }
        }
        manager::init_doodad(server, &world, &doodad);
        manager::spawn(server, &world, &doodad);
        crate::systems::npcs::doodad::db::save(server, &doodad);
        true
    }
}
