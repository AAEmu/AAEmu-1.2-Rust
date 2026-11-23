use rand::Rng;

use crate::models::container::{slot_type, ContainerRef};
use crate::models::doodad::doodad_owner_type;
use crate::models::item::ItemClass;
use crate::models::item_task::{task_type, ItemTask};
use crate::packets::world::SCUnitPointsPacket;
use crate::server::GameServer;
use crate::systems::economy::inventory::{self, Inv, ItemTxn};
use crate::systems::npcs::spawn_manager;
use crate::systems::skills::packets::SCUnitDeathPacket;
use crate::systems::skills::types::{BuffEventTriggerKind, KillReason};
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{PlayerRef, SlaveRef, WorldObject};

use super::super::stats;
use super::data::{attach_point, SlaveTemplate};
use super::despawn::audit_retirement;
use super::mounting::unbind_child_riders;
use super::packets::{
    item_task_type, SCItemTaskSuccessPacket, SCMySlavePacket, SCSlaveRemovedPacket,
};
use super::spawn::apply_attach_point_location;
use super::spawners::spawn_manager_of;
use super::{
    attach_reason, broadcast, error, flush_pending_turret_state, get_slave_by_db_id, player_world,
    unbind_slave,
};

// ---------------------------------------------------------------------------------------------
// Repair points
// ---------------------------------------------------------------------------------------------

/// The same exact HP bands govern both repair-point placement and refresh.
pub(crate) fn repair_point_health_band(hp: i32, max_hp: i32) -> i32 {
    if max_hp <= 0 {
        return 0;
    }
    let percent = i64::from(hp.max(0)) * 100 / i64::from(max_hp);
    match percent {
        0..25 => 0,
        25..50 => 25,
        50..75 => 50,
        75..100 => 75,
        _ => 100,
    }
}

/// Check both the direct heal and the repair-point interaction before charging
/// the player for the repair.
pub(crate) fn ship_repair_target_at_full_health(
    server: &GameServer,
    skill_id: u32,
    target: &crate::systems::skills::geometry::Target,
) -> bool {
    if !matches!(skill_id, 13428 | 13429 | 13711 | 14912 | 14913 | 14914) {
        return false;
    }
    let Some(target) = target.as_obj() else {
        return false;
    };
    let slave = target.as_slave().cloned().or_else(|| {
        let doodad = target.as_doodad()?.data();
        if doodad.owner_type != doodad_owner_type::SLAVE {
            return None;
        }
        let (instance, parent) = (doodad.transform.instance_id, doodad.parent_obj_id);
        drop(doodad);
        server.world.world(instance)?.slave(parent)
    });
    slave.is_some_and(|slave| {
        let d = slave.data();
        d.unit.max_hp > 0 && d.unit.hp >= d.unit.max_hp
    })
}

/// `UpdateSlaveRepairPoints(slave)`.
pub fn update_slave_repair_points(server: &GameServer, slave: &SlaveRef) {
    let Some(world) = server.world.world_of(&WorldObject::from(slave)) else {
        return;
    };
    let (hp, max_hp, template_id, attached, owner_obj, summoner_id, db_id, faction) = {
        let d = slave.data();
        (
            d.unit.hp,
            d.unit.max_hp,
            d.unit.template_id,
            d.attached_doodads.clone(),
            d.owner_obj_id,
            d.summoner_id,
            d.id,
            d.unit.faction_id,
        )
    };
    let Some(template) = server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)
    else {
        return;
    };
    if max_hp == 0 {
        tracing::error!("UpdateSlaveRepairPoints: MaxHp 0 (DivideByZeroException)");
        return;
    }
    let repair_points = match repair_point_health_band(hp, max_hp) {
        0 => template.healing_point_doodads.len() as i32,
        25 => template.hp25_doodad_count,
        50 => template.hp50_doodad_count,
        75 => template.hp75_doodad_count,
        _ => 0,
    };
    let mut current = Vec::new();
    let mut unused: Vec<u8> = template
        .healing_point_doodads
        .iter()
        .map(|b| b.attach_point_id)
        .collect();
    for id in &attached {
        let Some(d) = world.doodad(*id) else { continue };
        let ap = d.data().attach_point;
        if !(attach_point::HEAL_POINT0..=attach_point::HEAL_POINT9).contains(&ap) {
            continue;
        }
        current.push(d.clone());
        if let Some(i) = unused.iter().position(|p| *p == ap) {
            unused.remove(i);
        }
    }
    let points_to_add = repair_points - current.len() as i32;
    let mut rng = rand::thread_rng();
    if points_to_add < 0 {
        let spawns = spawn_manager_of(server, world.id);
        for _ in points_to_add..0 {
            if current.is_empty() {
                break;
            }
            let i = rng.gen_range(0..current.len());
            let d = current[i].clone();
            world.hide(&d);
            if let Some(s) = &spawns {
                s.add_despawn(server, WorldObject::from(&d), Some(spawn_manager::now()));
            }
            slave.data().attached_doodads.retain(|x| *x != d.obj_id);
            current.remove(i);
            unused.push(d.data().attach_point);
            crate::systems::npcs::doodad::manager::delete(server, &world, &d);
        }
    }
    if points_to_add > 0 && !unused.is_empty() {
        let mut i_add = 0;
        while i_add < points_to_add && !unused.is_empty() {
            let pick = unused[rng.gen_range(0..unused.len())];
            unused.retain(|p| *p != pick);
            let Some(binding) = template
                .healing_point_doodads
                .iter()
                .find(|b| b.attach_point_id == pick)
            else {
                tracing::error!(
                    "Somehow failed to grab a healing point {pick} for {}",
                    template.id
                );
                return;
            };
            let Some(doodad) = crate::systems::npcs::doodad::manager::create(
                server,
                &world,
                0,
                binding.doodad_id,
                None,
                true,
            ) else {
                i_add += 1;
                continue;
            };
            {
                let mut d = doodad.data();
                d.owner_obj_id = owner_obj;
                d.parent_obj_id = slave.obj_id;
                d.attach_point = pick;
                d.owner_id = summoner_id;
                d.plant_time = chrono::Utc::now().timestamp();
                d.owner_type = doodad_owner_type::SLAVE;
                d.owner_db_id = db_id;
                d.data = i32::from(pick);
                d.faction_id = faction;
                d.type2 = 1;
                d.is_persistent = false;
                d.scale = 1.0;
                d.transform.instance_id = world.id;
            }
            let dobj = WorldObject::from(&doodad);
            apply_attach_point_location(server, &world, slave, &dobj, pick);
            let start = crate::systems::npcs::doodad::phase::get_func_group_id(
                &server.game_data.systems.npcs.doodad,
                binding.doodad_id,
            );
            crate::systems::npcs::doodad::manager::set_func_group_id(server, &doodad, start);
            slave.data().attached_doodads.push(doodad.obj_id);
            current.push(doodad.clone());
            crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
            i_add += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Slave behaviour
// ---------------------------------------------------------------------------------------------

/// `Slave.DoHpChangeTrigger(triggerValue, ...)`.
pub fn do_hp_change_trigger(server: &GameServer, slave: &SlaveRef, trigger_value: i32) {
    let (name, summoner) = {
        let d = slave.data();
        (d.unit.name.clone(), d.summoner_name.clone())
    };
    let summoner = if summoner.is_empty() {
        "unknown".to_string()
    } else {
        summoner
    };
    tracing::debug!("{name} from {summoner}'s HP is now at {trigger_value}%");
    update_slave_repair_points(server, slave);
}

/// KILL_SLAVE (kind 26, `value1` 2 = the siege catapult, `value2` 1 = the opposing side,
/// INFERRED): destroying an enemy-side siege catapult during an active siege credits its
/// killer. The authored achievement text asks for the opposing siege camp, so the catapult's
/// owner must be on the roster and the killer not on the owner's side (a killer off the roster
/// is credited); a catapult outside a siege, one whose owner is off the roster or one killed by
/// its own side credits nothing.
fn credit_siege_catapult_kill(server: &GameServer, slave: &SlaveRef, killer: Option<&WorldObject>) {
    const SIEGE_CATAPULT: u32 = 2;
    let (template_id, owner_id, zone_id) = {
        let d = slave.data();
        (
            d.unit.template_id,
            d.unit.owner_id,
            d.unit.transform.zone_id,
        )
    };
    if template_id != SIEGE_CATAPULT || owner_id == 0 {
        return;
    }
    let Some(killer) = killer.and_then(WorldObject::as_player) else {
        return;
    };
    let Some(zone_group_id) = server
        .game_data
        .zones
        .zone_by_key(zone_id)
        .map(|zone| zone.group_id)
    else {
        return;
    };
    let Some(state) = server.systems.housing.dominions.get(zone_group_id) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let Some(siege) = state.siege.as_ref().filter(|siege| siege.active_at(now)) else {
        return;
    };
    let victim_side = siege.participants.get(&owner_id).map(|p| p.side);
    let killer_side = siege.participants.get(&killer.character_id).map(|p| p.side);
    if victim_side.is_none() || victim_side == killer_side {
        return;
    }
    crate::systems::report_achievement(
        server,
        killer,
        AchievementInput::Increment {
            kind_id: char_record_kind::KILL_SLAVE,
            amount: 1,
            matcher: RecordMatch::Value1And2 {
                value1: SIEGE_CATAPULT,
                value2: 1,
            },
        },
    );
}

/// `Slave.DoDie(killer, killReason)`.
pub fn do_die(
    server: &GameServer,
    slave: &SlaveRef,
    killer: Option<&WorldObject>,
    kill_reason: KillReason,
) {
    let _items = server.systems.economy.operation_scope();
    let _publication = server.systems.vehicles.slave_summons.publication_scope();
    credit_siege_catapult_kill(server, slave, killer);
    let obj = WorldObject::from(slave);
    if let Some(world) = server.world.world_of(&obj) {
        audit_retirement(server, &world, slave);
    }
    crate::systems::skills::skill::interrupt_skills(server, &obj);
    crate::systems::skills::buffs::fire_unit_event(
        server,
        slave.obj_id,
        BuffEventTriggerKind::Death,
        0,
    );
    crate::systems::skills::buffs::remove_effects_on_death(server, &obj);
    match killer {
        Some(k) => {
            let killer_info = k
                .is_unit()
                .then(|| (k.obj_id(), k.with_base_unit(|b| b.name().to_string())));
            broadcast(
                server,
                k,
                &SCUnitDeathPacket {
                    obj_id: slave.obj_id,
                    kill_reason,
                    resurrect_wait_time: 0,
                    lost_exp: 0,
                    durability_loss_rate: 0,
                    killer: killer_info,
                },
                true,
            );
        }
        None => tracing::error!("Slave.DoDie without a killer (NullReferenceException)"),
    }
    // Keep child transforms and seat lists available until every gunner has detached in world
    // coordinates. Deleting a cannon first loses its riders and their detach notifications.
    unbind_child_riders(server, slave, attach_reason::NONE);
    destroy_attached_items(server, slave);
    distribute_slave_drop_doodads(server, slave);
    mark_summoning_item_as_destroyed(server, slave);
    let (summoner, tl, packet, attached, template_id, despawn_time) = {
        let d = slave.data();
        (
            d.summoner_obj_id,
            d.unit.tl_id,
            (
                d.unit.name.clone(),
                d.unit.template_id,
                d.unit.hp,
                d.unit.max_hp,
            ),
            d.attached_characters.clone(),
            d.unit.template_id,
            d.spawner_despawn_time.unwrap_or(20),
        )
    };
    let world = server.world.world_of(&obj);
    let pos = world
        .as_ref()
        .map(|w| w.world_pos_rot(&obj))
        .unwrap_or_default();
    if let Some(s) = (summoner != 0)
        .then(|| server.world.character_by_obj_id(summoner))
        .flatten()
    {
        s.send(SCMySlavePacket {
            obj_id: slave.obj_id,
            tl_id: tl,
            name: packet.0,
            template_id: packet.1,
            hp: packet.2,
            max_hp: packet.3,
            x: pos.x,
            y: pos.y,
            z: pos.z,
        });
        broadcast(
            server,
            &WorldObject::from(&s),
            &SCSlaveRemovedPacket {
                obj_id: s.obj_id,
                tl_id: tl,
            },
            true,
        );
    }
    crate::systems::skills::combat::clear_all_aggro(server, &obj);
    for (_, c) in attached {
        if let Some(p) = server.world.character_by_obj_id(c) {
            unbind_slave(server, &p, u32::from(tl), attach_reason::NONE);
        }
    }
    if server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)
        .is_some_and(SlaveTemplate::is_a_boat)
    {
        super::ship_physics::remove_ship(server, slave);
    }
    if let Some(s) = world.and_then(|w| spawn_manager_of(server, w.id)) {
        s.add_despawn(
            server,
            obj,
            Some(spawn_manager::add_secs(
                spawn_manager::now(),
                f64::from(despawn_time),
            )),
        );
    }
}

/// `DestroyAttachedItems()`: child doodads (dropping a carried backpack) and child slaves.
fn destroy_attached_items(server: &GameServer, slave: &SlaveRef) {
    let Some(world) = server.world.world_of(&WorldObject::from(slave)) else {
        return;
    };
    let (doodads, slaves) = {
        let d = slave.data();
        (d.attached_doodads.clone(), d.attached_slaves.clone())
    };
    for id in doodads {
        let Some(doodad) = world.doodad(id) else {
            continue;
        };
        let item_id = doodad.data().item_id;
        if item_id > 0 {
            super::effects::drop_carried_backpack(server, &world, &doodad, item_id);
        }
        server.ids.object.release_id(doodad.obj_id);
        doodad.data().is_persistent = false;
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
    }
    for id in slaves {
        server.ids.object.release_id(id);
        if let Some(s) = world.slave(id) {
            world.delete(&s);
        }
    }
}

/// `DistributeSlaveDropDoodads()` (debris of destroyed vehicles).
fn distribute_slave_drop_doodads(server: &GameServer, slave: &SlaveRef) {
    let obj = WorldObject::from(slave);
    let Some(world) = server.world.world_of(&obj) else {
        return;
    };
    let template_id = slave.data().unit.template_id;
    let Some(template) = server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)
    else {
        return;
    };
    let center = world.world_pos_rot(&obj);
    let mut rng = rand::thread_rng();
    for drop in &template.slave_drop_doodads {
        for _ in 0..drop.count {
            let Some(doodad) = crate::systems::npcs::doodad::manager::create(
                server,
                &world,
                0,
                drop.doodad_id,
                None,
                true,
            ) else {
                tracing::error!(
                    "Slave drop doodad {} missing (NullReferenceException)",
                    drop.doodad_id
                );
                continue;
            };
            let mut v = [rng.gen::<f32>() * 2.0 - 1.0, rng.gen::<f32>() * 2.0 - 1.0];
            let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
            if len > 0.0 {
                v = [v[0] / len, v[1] / len];
            }
            let r = rng.gen::<f32>() * drop.radius;
            let (x, y, z) = (center.x + v[0] * r, center.y + v[1] * r, center.z);
            let h = if !drop.on_water {
                crate::systems::ai::collision::ground_height_or(server, world.id, x, y, z, || {
                    crate::systems::npcs::heightmap::world_height(server, &world, x, y)
                        .unwrap_or(0.0)
                })
            } else {
                super::ship_physics::water_surface(server, &world, [x, y, z])
            };
            let rot = (rng.gen::<f64>() * std::f64::consts::PI * 2.0) as f32;
            {
                let mut d = doodad.data();
                d.transform.instance_id = world.id;
                d.transform.set_position(x, y, h, 0.0, 0.0, rot);
            }
            crate::systems::npcs::doodad::manager::init_doodad(server, &world, &doodad);
            crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
        }
    }
}

/// `MarkSummoningItemAsDestroyed()`.
fn mark_summoning_item_as_destroyed(server: &GameServer, slave: &SlaveRef) {
    let (item_id, summoner) = {
        let d = slave.data();
        (d.summoning_item_id, d.summoner_obj_id)
    };
    if item_id == 0 {
        return;
    }
    let summoner = server.world.character_by_obj_id(summoner);
    let task = super::mates::update_owned_item(server, summoner.as_ref(), item_id, |i| {
        if i.class != ItemClass::SummonSlave {
            return None;
        }
        i.details.slave_is_destroyed = 1;
        // The C# wrote DateTime.MinValue here, which made the 10 minute repair check of
        // `Create` pass immediately (a destroyed vehicle could be re-summoned at once).
        i.details.slave_repair_start_time = chrono::Utc::now().timestamp();
        i.dirty = true;
        Some(super::packets::item_update(i))
    })
    .flatten();
    if let (Some(task), Some(s)) = (task, summoner) {
        s.send(SCItemTaskSuccessPacket {
            action: item_task_type::MATE_DEATH,
            tasks: vec![task],
            force_remove: Vec::new(),
        });
    }
}

/// `CSRepairSlaveItemsPacket`: a repairman restores the destroyed slave summon items in the
/// player's bag.
///
/// The 1.2 client's cost preview `GetTotalRepairsForSlaveItems` (`FUN_39447c20`) scans
/// `items.impl_id = 8` (`ItemImplEnum.SummonSlave`, the mate variant scans impl 11), requires
/// the item's destroyed flag, resolves the slave through `item_summon_slaves` and adds the
/// slave template's `cost` (the client's `slaves` query selects `id, cost, ...` in that order;
/// `FUN_39696380(slaveId)` +4). It then keeps only the slots whose owning container is the
/// player's bag: `FUN_390457b0(location)` looks the slot's container up and the preview
/// compares it with `*(inventory + 0xc34c)` (the bag container id). The bank slots
/// `FUN_397d3130` also yields (key high byte 0x300) therefore never count, so the service is
/// bag-only too. The 1.2 C# never implemented the handler. The reply follows
/// the `RepairSlaveEffect` kit path: `SCItemTaskSuccess(ItemTaskType.RepairSlaves)` with one
/// `ItemUpdate` per repaired item, then `ErrorMessageType.SlaveRepaired` (the client locale has
/// `slave_repaired` / `slave_cannot_repair_already_spawned` for this service).
pub fn repair_slave_items(server: &GameServer, player: &PlayerRef, npc_obj_id: u32) {
    use crate::data::items::TemplateKind;
    use crate::systems::economy::error_message;
    use crate::systems::economy::inventory::send_error_message_to;

    if npc_obj_id == 0 || player.data().unit.current_interaction_object != npc_obj_id {
        return;
    }
    let Some(world) = player_world(server, player) else {
        return;
    };
    let Some(npc) = world.npc(npc_obj_id) else {
        return;
    };
    let repairman = server
        .systems
        .economy
        .hooks
        .npcs
        .get()
        .and_then(|h| h.shop_info(server, &npc))
        .is_some_and(|i| i.repairman);
    if !repairman {
        tracing::warn!(
            "Attempting to repair slave items while not at a repairman, NPC: {}",
            npc.obj_id
        );
        return;
    }
    let me = world.world_pos_rot(&WorldObject::from(player));
    let them = world.world_pos_rot(&WorldObject::from(&npc));
    if me.distance_to(&them, false) > 5.0 {
        error(player, error_message::TOO_FAR_AWAY);
        return;
    }
    let inv = Inv::of(player);
    let containers: Vec<_> = [slot_type::INVENTORY]
        .into_iter()
        .filter_map(|t| inv.get(t))
        .collect();
    // (item id, slave db id, price). Skip rows whose template/slave data is missing instead of
    // failing the whole request.
    let mut destroyed: Vec<(u64, u32, i32)> = Vec::new();
    for c in &containers {
        let c = c.lock().expect("container poisoned");
        for i in &c.items {
            if i.class != ItemClass::SummonSlave || i.details.slave_is_destroyed == 0 {
                continue;
            }
            let Some(template) = server.game_data.items.template(i.template_id) else {
                continue;
            };
            let TemplateKind::SummonSlave { slave_id } = &template.kind else {
                continue;
            };
            let Some(slave_template) = server.game_data.systems.vehicles.slave_template(*slave_id)
            else {
                continue;
            };
            destroyed.push((i.id, i.details.slave_db_id, slave_template.cost.max(0)));
        }
    }
    if destroyed.is_empty() {
        tracing::debug!(
            "RepairSlaveItems: no destroyed slave item, NPC: {}",
            npc.obj_id
        );
        return;
    }
    // A vehicle that is currently out cannot be repaired (`SLAVE_CANNOT_REPAIR_ALREADY_SPAWNED`).
    if destroyed
        .iter()
        .any(|(_, db_id, _)| *db_id != 0 && get_slave_by_db_id(&world, *db_id).is_some())
    {
        error(player, error_message::SLAVE_CANNOT_REPAIR_ALREADY_SPAWNED);
        return;
    }
    let total: i64 = destroyed.iter().map(|(_, _, cost)| i64::from(*cost)).sum();
    let Ok(cost) = i32::try_from(total) else {
        return;
    };
    let mut txn = ItemTxn::begin(server);
    // Debit first (check and subtract under the character's one lock), refund what could not
    // be repaired below: an item traded away between the scan and the update must not be paid.
    if cost > 0
        && !inventory::change_money(
            &mut txn.out(),
            player,
            slot_type::INVENTORY,
            slot_type::NONE,
            cost,
            task_type::DEPOSIT_MONEY,
        )
    {
        return;
    }
    let (tasks, refund) = repair_destroyed_slave_items(&containers, &destroyed);
    if refund > 0 {
        inventory::change_money(
            &mut txn.out(),
            player,
            slot_type::NONE,
            slot_type::INVENTORY,
            refund,
            task_type::DEPOSIT_MONEY,
        );
    }
    if tasks.is_empty() {
        // Every scanned item vanished before the update: only the refund goes out, no empty
        // RepairSlaves task and no "repaired" message.
        return;
    }
    let mut out = txn.out();
    out.send(
        player,
        SCItemTaskSuccessPacket {
            action: item_task_type::REPAIR_SLAVES,
            tasks,
            force_remove: Vec::new(),
        },
    );
    send_error_message_to(&mut out, player, error_message::SLAVE_REPAIRED);
}

/// Applies the repair to the destroyed items that are still in `containers` (the player's own
/// bag), returning the update tasks and the price of the items that vanished or were already
/// repaired. No global item store is consulted: an item traded or mailed away is only refunded.
pub(super) fn repair_destroyed_slave_items(
    containers: &[ContainerRef],
    destroyed: &[(u64, u32, i32)],
) -> (Vec<ItemTask>, i32) {
    let mut tasks = Vec::new();
    let mut refund = 0i32;
    for (item_id, _, price) in destroyed {
        let mut task = None;
        for c in containers {
            let mut c = c.lock().expect("container poisoned");
            let Some(i) = c.get_item_by_id_mut(*item_id) else {
                continue;
            };
            if i.class == ItemClass::SummonSlave && i.details.slave_is_destroyed != 0 {
                // The NPC service completes at once: unlike a repair kit, no 10 minute timer.
                i.details.slave_is_destroyed = 0;
                i.details.slave_repair_start_time = 0;
                i.dirty = true;
                task = Some(super::packets::item_update(i));
            }
            break;
        }
        match task {
            Some(task) => tasks.push(task),
            None => refund = refund.saturating_add(*price),
        }
    }
    (tasks, refund)
}

/// `Slave.RegenTick(delta)`.
pub fn regen_tick(server: &GameServer, slave: &SlaveRef) {
    flush_pending_turret_state(server, slave);
    let obj = WorldObject::from(slave);
    let (needs, dead, tl, attached) = {
        let d = slave.data();
        (
            d.unit.needs_regen(),
            d.unit.is_dead(),
            u32::from(d.unit.tl_id),
            d.attached_characters.clone(),
        )
    };
    if !needs {
        return;
    }
    if dead {
        for (_, c) in attached {
            if let Some(p) = server.world.character_by_obj_id(c) {
                unbind_slave(server, &p, tl, attach_reason::NONE);
            }
        }
        return;
    }
    stats::refresh_object_pools(server, &obj);
    let (old, hp, mp) = {
        let mut d = slave.data();
        let u = &mut d.unit;
        let old = u.hp;
        if u.is_in_battle {
            u.hp += u.persistent_hp_regen;
            u.mp += u.persistent_mp_regen;
        } else {
            u.hp += u.hp_regen;
            u.mp += u.mp_regen;
        }
        u.hp = u.hp.min(u.max_hp);
        u.mp = u.mp.min(u.max_mp);
        (old, u.hp, u.mp)
    };
    broadcast(
        server,
        &obj,
        &SCUnitPointsPacket {
            obj_id: slave.obj_id,
            hp,
            mp,
        },
        false,
    );
    crate::systems::skills::combat::post_update_current_hp(
        server,
        &obj,
        Some(&obj),
        old,
        hp,
        KillReason::Unknown,
    );
}
