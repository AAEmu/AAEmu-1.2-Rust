use super::{
    apply_tower_state, broadcast_snapshot, calendar, clear_national_monument_on_owner_change,
    equip_slot, is_expedition_leader, is_live_object, slot_type, task_type, tower_identity,
    CastAction, DominionError, EffectArgs, GameServer, Inv, ItemTxn, SkillCastTarget, SkillCaster,
    Utc, WorldObject, DECLARATION_SKILL, PURIFYING_ARCHEUM,
};

/// Initial unowned claim, only from the accepted original 20-second skill and exact carried
/// Purifying Archeum. Ownership assignment and pack consumption share one authority operation.
pub fn declare_effect(server: &GameServer, args: &EffectArgs<'_>) {
    let (Some(caster), Some(source), Some(tower)) = (
        args.caster.as_ref(),
        args.source.skill.as_ref(),
        args.target.as_obj().and_then(WorldObject::as_house),
    ) else {
        return;
    };
    let Some(player) = caster.as_player() else {
        return;
    };
    let Some(canonical) = server.game_data.systems.skills.skill(DECLARATION_SKILL) else {
        return;
    };
    let (accepted, target_id) = {
        let state = source.state();
        (
            state.generation != 0 && !state.cancelled,
            state.initial_target,
        )
    };
    if !std::sync::Arc::ptr_eq(canonical, &source.template)
        || source.id != DECLARATION_SKILL
        || args.source.buff.is_some()
        || args.source.is_trigger
        || !accepted
        || target_id != tower.obj_id
        || !matches!(args.cast, CastAction::Skill { skill_id: DECLARATION_SKILL, tl_id } if tl_id == source.tl_id())
        || !matches!(args.caster_caster, SkillCaster::Unit(id) if id == player.obj_id)
        || !matches!(args.target_caster, SkillCastTarget::Unit(id) if id == tower.obj_id)
    {
        return;
    }
    let _items = server.systems.economy.operation_scope();
    let operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    let obj = WorldObject::from(tower);
    let Some((zone, identity)) = tower_identity(server, tower) else {
        return;
    };
    // Original 1.2 lodestones have a single authored claim action at build step 0
    // (housing139/step395/model893/skill13661), before becoming their main model.
    let Some(template) = server
        .game_data
        .systems
        .housing
        .template(tower.data().unit.template_id)
    else {
        return;
    };
    let initial_claim = template.build_steps.len() == 1
        && template.build_steps.get(&0).is_some_and(|step| {
            step.step == 0 && step.skill_id == DECLARATION_SKILL && step.num_actions == 1
        });
    let at_initial_action = {
        let h = tower.data();
        h.current_step == 0 && h.base_action == 0 && h.num_action == 0 && h.unit.owner_id == 0
    };
    if !initial_claim || !at_initial_action {
        return;
    }
    if !server
        .systems
        .housing
        .manager
        .by_id(identity.house_id)
        .is_some_and(|h| std::sync::Arc::ptr_eq(&h, tower))
        || !is_live_object(server, caster)
        || !is_live_object(server, &obj)
        || caster.transform().instance_id != identity.instance_id
        || tower.data().unit.hp <= 0
        || !server
            .game_data
            .systems
            .housing
            .castles
            .guard_tower_settings
            .contains_key(&identity.guard_tower_setting_id)
    {
        return;
    }
    let (expedition, alive) = {
        let c = player.data();
        (
            c.unit.expedition_id,
            c.unit.hp > 0 && !c.skill_char.skill_cancelled,
        )
    };
    if !alive || !is_expedition_leader(server, player, expedition) {
        return;
    }
    let Some(world) = server.world.world_of(caster) else {
        return;
    };
    let c = world.world_pos_rot(caster);
    let t = world.world_pos_rot(&obj);
    let distance2 = (c.x - t.x).powi(2) + (c.y - t.y).powi(2) + (c.z - t.z).powi(2);
    let range = canonical.max_range as f32;
    if !distance2.is_finite() || !range.is_finite() || range <= 0.0 || distance2 > range * range {
        return;
    }
    // Auroria claims follow the era profile (`World.Profile`), independently of
    // `Housing.SiegeCalendar`: the Trion launch era has no northern continent, while a
    // calendar-off Auroria server still allows claims for GM-scheduled sieges. 662 is the
    // existing "not yet a region where a territory can be proclaimed" refusal.
    if !server.config.auroria_claims_enabled() {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_CLOSED,
        );
        return;
    }
    // WP-S6 claim gates. The 1.2 client has no claim validator; it only renders the server's
    // error id, so the ids below are its error table (FUN_3956b860 in x2game.dll.c) and the
    // order is this server policy. The authored `open_*` window uses the real wall clock: the
    // GM test clock is restricted to the calendar view.
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    let window = calendar::SiegeCalendar::from_castles(&server.game_data.systems.housing.castles)
        .claim_window_at(zone, Utc::now().timestamp(), &settings.timezone);
    match window {
        calendar::ClaimWindow::Closed => {
            // 662 DOMINION_CLOSED: "not yet a region where a territory can be proclaimed".
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_CLOSED,
            );
            return;
        }
        calendar::ClaimWindow::NotOpen => {
            // 582 DOMINION_NOT_DECLARE_TIME: the window text is parameterized by open_*.
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_NOT_DECLARE_TIME,
            );
            return;
        }
        calendar::ClaimWindow::Open => {}
    }
    let persisted_faction = crate::db_character::persisted_faction(&player.data());
    let pirate = server
        .expeditions
        .get(expedition)
        .is_some_and(|e| e.faction.mother_id == crate::data::world_data::factions::PIRATE)
        || persisted_faction == crate::data::world_data::factions::PIRATE;
    if pirate {
        // 351 DOMINION_NOT_AVAILABLE_FOR_PIRATES: "Pirates cannot proclaim a territory".
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NOT_AVAILABLE_FOR_PIRATES,
        );
        return;
    }
    let claim_limit = server.config.housing.dominion_claim_limit;
    let mut effects = None;
    let previous = server.systems.housing.dominions.get(zone);
    let claim = server.systems.housing.dominions.declare_unowned_with_limit(
        zone,
        identity,
        expedition,
        Utc::now().timestamp(),
        claim_limit,
        || {
            let mut txn = ItemTxn::begin(server);
            let inv = Inv::of(player);
            let Some(equipment) = inv.get(slot_type::EQUIPMENT) else {
                return false;
            };
            let mut equipment = equipment.lock().expect("equipment poisoned");
            let pack = equipment
                .get_item_by_slot(equip_slot::BACKPACK)
                .filter(|item| {
                    item.template_id == PURIFYING_ARCHEUM
                        && item.count == 1
                        && item.owner_id == u64::from(player.character_id)
                        && equipment.owner_id == player.character_id
                })
                .map(|item| item.id);
            let consumed = pack.is_some_and(|id| {
                equipment.consume_item(
                    task_type::SKILL_REAGENTS,
                    PURIFYING_ARCHEUM,
                    1,
                    Some(id),
                    &mut txn.env,
                ) == 1
            });
            drop(equipment);
            effects = Some(txn.into_fx());
            consumed
        },
    );
    // Special50 has no separate CraftEffect. Advance only after the authority and exact
    // pack consumption succeeded, while other housing operations remain serialized.
    if claim.is_ok() {
        crate::systems::housing::manager::add_build_action(server, tower);
    }
    drop(operation);
    if let Err(DominionError::TooManyOwned) = &claim {
        // 441 TOO_MANY_OWNED_DOMINIONS: the pack was not consumed (checked under the
        // authority lock before the callback).
        super::util::send_error(
            player,
            crate::systems::economy::error_message::TOO_MANY_OWNED_DOMINIONS,
        );
    }
    if let Some(effects) = effects {
        crate::systems::economy::fx::flush(server, effects);
    }
    if let Ok(state) = claim {
        let progress = {
            let h = tower.data();
            crate::systems::housing::packets::SCHouseBuildProgressPacket {
                tl: h.unit.tl_id,
                model_id: h.unit.model_id,
                all_step: h.all_action,
                cur_step: h.all_action,
            }
        };
        // Native 1.2 clamps UnitPoints against its current construction model's HP cap.
        // Install the finished model before sending the tower's completed health.
        crate::world::broadcast(&world, tower, &progress, true);
        apply_tower_state(server, &state);
        for doodad in server.systems.housing.manager.attached(tower.obj_id) {
            crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
        }
        broadcast_snapshot(server, &state, true);
        if let Some(previous) = previous {
            clear_national_monument_on_owner_change(server, &previous, &state);
        }
    }
}
