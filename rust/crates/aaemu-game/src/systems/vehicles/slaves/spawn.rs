use std::sync::Arc;

use crate::data::world_templates::WorldSpawnPosition;
use crate::models::container::slot_type;
use crate::models::doodad::doodad_owner_type;
use crate::models::item::{Item, ItemClass};
use crate::models::item_task::ItemTask;
use crate::models::slave::Slave;
use crate::models::transform::Transform;
use crate::models::unit::{BaseUnitType, Bonus};
use crate::server::GameServer;
use crate::systems::skills::types::KillReason;
use crate::world::{Entity, PlayerRef, SlaveRef, WorldInstance, WorldObject};

use super::super::{now_ms, stats};
use super::data::SlaveTemplate;
use super::equipment::{initial_slave_equipment, is_binding_attach_point};
use super::packets::{
    item_task_type, SCItemTaskSuccessPacket, SCMySlavePacket, SCSlaveCreatedPacket,
};
use super::summon::repair_seconds_left;
use super::summon_placement::{
    fallback_summon_point, hull_clear_at, hull_probe_points, overhead_clear_at, summon_floor,
    summon_hull_bounds, summon_offset_z, summon_terrain, HullEnvelope,
};
use super::{
    add_distance_to_front, broadcast, buff, error, save, spawn_equipment_visuals_from_inventory,
    update_slave_repair_points, SavedChildSlave, SavedSlave, SlaveSpawner,
};

/// `Buoyancy.BaseWaterDensity` / `ShipWaterDensityMul` (physics constants used by the spawn
/// height adjustment).
const BASE_WATER_DENSITY: f32 = 1.025;
const SHIP_WATER_DENSITY_MUL: f32 = 3.0;

/// How far (metres) a level boat's authored Z may be from the water surface it is put on.
/// Further away, no pool is under the spawn and the authored height is kept.
const LEVEL_BOAT_SURFACE_SEARCH: f32 = 5.0;

/// `ApplySlaveBonuses(summonedSlave)`.
pub(super) fn apply_slave_bonuses(
    server: &GameServer,
    obj: &WorldObject,
    template: &SlaveTemplate,
) {
    for passive in &template.passive_buffs {
        match server.game_data.skills.passive_buff(*passive) {
            Some(p) => {
                crate::systems::skills::buffs::add_buff_by_id(server, obj, p.buff_id, obj, 0)
            }
            None => tracing::error!(
                "Slave {}: passive buff {passive} missing (NullReferenceException)",
                template.id
            ),
        }
    }
    let has_summoner = obj
        .as_slave()
        .is_some_and(|slave| slave.data().summoner_obj_id != 0);
    for buff_id in &template.initial_buffs {
        // Authored level boats are public. The rowboat template also serves
        // player summons, which have an owner for Owner's Mark to protect.
        if *buff_id == buff::OWNERS_MARK && !has_summoner {
            continue;
        }
        crate::systems::skills::buffs::add_buff_by_id(server, obj, *buff_id, obj, 0);
    }
    obj.with_unit(|u| {
        for b in &template.bonuses {
            u.unit_mut().bonuses.add_bonus(
                0,
                Bonus {
                    template: b.clone(),
                    // Every 1.2 `unit_modifiers` Slave row has `linear_level_bonus = 0` (pinned
                    // by `real_slave_unit_modifiers_have_no_linear_level_bonus`), so the linear
                    // level term cannot change this value with the shipped data.
                    value: b.value,
                },
            );
        }
    });
}

/// `ApplyAttachPointLocation(slave, baseUnit, attachPoint)`.
pub(super) fn apply_attach_point_location(
    server: &GameServer,
    world: &WorldInstance,
    slave: &SlaveRef,
    child: &WorldObject,
    point: u8,
) {
    let model_id = slave.data().unit.model_id;
    match server
        .systems
        .vehicles
        .attach_points_for_slave(server, model_id)
    {
        Some(points) => match points.iter().find(|(p, _)| *p == point) {
            Some((_, v)) => {
                let v: WorldSpawnPosition = *v;
                // Transform = slave.Transform.CloneAttached(child); Parent = slave.Transform
                child.with_transform(|t| {
                    t.set_position(v.x, v.y, v.z, v.roll, v.pitch, v.yaw);
                });
                world.attach_child(&WorldObject::from(slave), child);
                tracing::debug!(
                    "Model id: {model_id} attachment {point} => pos ({}, {}, {})",
                    v.x,
                    v.y,
                    v.z
                );
            }
            None => tracing::warn!("Model id: {model_id} incomplete attach point information"),
        },
        None => tracing::warn!("Model id: {model_id} has no attach point information"),
    }
}

/// A slave spawned by the first half of `Create` (up to `summonedSlave.Spawn()`).
pub(crate) struct Spawned {
    pub slave: SlaveRef,
    pub template: SlaveTemplate,
    pub is_loaded: bool,
}

/// `Create(owner, useSpawner, templateId, item, hideSpawnEffect, positionOverride)` without DB
/// data (spawners, tests): both halves back to back.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_core(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    owner: Option<&PlayerRef>,
    spawner: Option<&Arc<SlaveSpawner>>,
    template_id: u32,
    item: Option<Item>,
    hide_spawn_effect: bool,
    position_override: Option<Transform>,
    saved: Option<SavedSlave>,
) -> Option<SlaveRef> {
    let s = create_spawn(
        server,
        world,
        owner,
        spawner,
        template_id,
        item.clone(),
        hide_spawn_effect,
        position_override,
        saved,
    )?;
    finish_create(
        server,
        world,
        owner,
        &s.slave,
        &s.template,
        item.as_ref(),
        s.is_loaded,
        Vec::new(),
    )
}

pub(super) struct CommittedSummonItem {
    db_id: u32,
    task: ItemTask,
}

pub(super) fn commit_summon_item(
    server: &GameServer,
    owner: &PlayerRef,
    source: &Item,
    db_id: u32,
) -> Option<CommittedSummonItem> {
    let result = {
        let _ops = server.systems.economy.ops_lock();
        super::mates::update_owned_item(server, Some(owner), source.id, |item| {
            if item.id != source.id
                || item.template_id != source.template_id
                || item.owner_id != u64::from(owner.character_id)
                || item.class != ItemClass::SummonSlave
                || item.slot_type != slot_type::INVENTORY
            {
                return Err(None);
            }
            if let Some(left) = repair_seconds_left(item, chrono::Utc::now().timestamp()) {
                return Err(Some(left));
            }
            item.details.slave_type = 0x02;
            item.details.slave_db_id = db_id;
            item.details.slave_repair_start_time = 0;
            item.dirty = true;
            Ok(super::packets::item_update(item))
        })
    };
    match result {
        Some(Ok(task)) => Some(CommittedSummonItem { db_id, task }),
        Some(Err(Some(left))) => {
            owner.send(crate::packets::world::SCErrorMsgPacket::new(
                error::SLAVE_SPAWN_ERROR_NEED_REPAIR_TIME,
                left as u32,
                true,
            ));
            None
        }
        Some(Err(None)) | None => None,
    }
}

/// The first half of `Create`: template, position, summon item checks, `SCSlaveCreatedPacket`,
/// the slave itself (bonuses, HP / MP, initial items) and `Spawn()`. `saved` is the
/// `load_saved_slave` row (owner + item only).
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_spawn(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    owner: Option<&PlayerRef>,
    spawner: Option<&Arc<SlaveSpawner>>,
    template_id: u32,
    item: Option<Item>,
    hide_spawn_effect: bool,
    position_override: Option<Transform>,
    saved: Option<SavedSlave>,
) -> Option<Spawned> {
    create_spawn_committed(
        server,
        world,
        owner,
        spawner,
        template_id,
        item,
        hide_spawn_effect,
        position_override,
        saved,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create_spawn_committed(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    owner: Option<&PlayerRef>,
    spawner: Option<&Arc<SlaveSpawner>>,
    template_id: u32,
    item: Option<Item>,
    hide_spawn_effect: bool,
    position_override: Option<Transform>,
    saved: Option<SavedSlave>,
    committed: Option<CommittedSummonItem>,
) -> Option<Spawned> {
    let data = &server.game_data.systems.vehicles;
    let template = data
        .slave_template(spawner.map_or(template_id, |s| s.def.base.unit_id))?
        .clone();
    let tl_id = server.ids.tl.get_next_id().ok()?;
    let obj_id = server.ids.object.get_next_id().ok()?;

    let mut spawn_pos = position_override.unwrap_or_default();
    spawn_pos.instance_id = world.id;
    // The ship controller also checks WorldId before ticking the hull.
    spawn_pos.world_id = world.id;
    let mut db_id = 0u32;
    let mut slave_name = String::new();
    let (mut slave_hp, mut slave_mp) = (1, 1);
    let is_loaded = saved.is_some();
    if let Some(s) = &saved {
        db_id = s.id;
        slave_name = s.name.clone();
        slave_hp = s.hp;
        slave_mp = s.mp;
    }
    let (owner_id, owner_obj_id, owner_name, owner_faction) = match owner {
        Some(o) => {
            let c = o.data();
            (c.id, o.obj_id, c.unit.name.clone(), c.unit.faction_id)
        }
        None => (0, 0, String::new(), 0),
    };

    // Put it at the correct location
    if spawn_pos.x == 0.0 && spawn_pos.y == 0.0 && spawn_pos.z == 0.0 {
        if owner.is_none() && spawner.is_none() {
            tracing::warn!(
                "Tried creating a slave without a defined position, either use a Owner, Spawner or PositionOverride"
            );
            return None;
        }
        if let Some(sp) = spawner {
            let p = sp.def.base.position;
            spawn_pos.zone_id = p.zone_id;
            spawn_pos.set_position(p.x, p.y, p.z, p.roll, p.pitch, p.yaw);
        } else if let Some(o) = owner {
            let me = WorldObject::from(o);
            let w = world.world_pos_rot(&me);
            let t = o.data().unit.transform;
            spawn_pos.instance_id = t.instance_id;
            spawn_pos.zone_id = t.zone_id;
            spawn_pos.set_position(w.x, w.y, w.z, w.rot_x, w.rot_y, w.rot_z);
        }
        if owner.is_some() {
            add_distance_to_front(&mut spawn_pos, template.spawn_y_offset.clamp(5.0, 50.0));
        }
        // Always spawn horizontal(level) and 90° CCW of the player. The probes below run along
        // this final yaw, not the player's facing the hull is turned away from.
        let final_yaw = match owner {
            Some(o) => {
                world.world_pos_rot(&WorldObject::from(o)).rot_z + std::f32::consts::PI / 2.0
            }
            None => spawner.map_or(0.0, |s| s.def.base.position.yaw),
        };
        if template.is_a_boat() && spawner.is_none() {
            let water = super::ship_physics::water_surface(
                server,
                world,
                [spawn_pos.x, spawn_pos.y, spawn_pos.z],
            );
            spawn_pos.set_height(water);
            let Some(ship) = data.ship_model(template.model_id).copied() else {
                tracing::error!(
                    "Slave {}: no ship model {} (NullReferenceException)",
                    template.id,
                    template.model_id
                );
                return None;
            };
            let min_depth = ship.mass_box_size_z - ship.mass_center_z + 1.0;
            let offset_z = summon_offset_z(&ship);
            // Enough water for the whole hull: the actual collision-world ground (terrain,
            // rocks, piers), not just the heightmap. A shallow spot under the 30 m hull lets
            // its collision box overlap the ground and CryPhysics ejects the summoned hull.
            // The probes cover the hull footprint the solid physics hull uses (the clipped
            // authored collision bounds, else the mass box, at the fresh slave's scale 1.0) at
            // the ship's final (turned) yaw, including the asymmetric bow/stern extents and
            // the middle of the long sides. A downward ray from above the hull rejects spots
            // under a pier, bridge or deck (those are above the water and invisible to the
            // ground probe, which starts only 2 m above the surface). The rays miss a solid
            // they start inside (back faces are ignored), so the whole hull envelope from the
            // keel to just above the hull top is also tested for overlap with static
            // colliders: a thick deck or a quay that contains the hull centre is rejected even
            // when every probe ray passes through it.
            let hull_bounds = summon_hull_bounds(server, template.model_id);
            let envelope = HullEnvelope::from_hull(hull_bounds.as_ref(), &ship, offset_z);
            let footprint = envelope.footprint;
            let hull_top = envelope.hull_top();
            let spawn_z = spawn_pos.z;
            let spot_ok = |x: f32, y: f32| {
                let surface = super::ship_physics::water_surface(server, world, [x, y, spawn_z]);
                let floor = summon_floor(
                    crate::systems::ai::collision::ground_height_probe(
                        server, world.id, x, y, spawn_z, 2.0, 2048.0,
                    ),
                    || {
                        crate::systems::ai::collision::terrain_height_or(
                            server,
                            world.id,
                            x,
                            y,
                            || {
                                crate::systems::npcs::heightmap::world_height(server, world, x, y)
                                    .unwrap_or(0.0)
                            },
                        )
                    },
                );
                surface - floor > min_depth
                    && overhead_clear_at(server, world.id, x, y, surface, hull_top)
            };
            let mut in_front = 0.0f32;
            let mut deep = false;
            let mut chosen = "initial";
            while in_front < 50.0 + ship.mass_box_size_x {
                let mut check = spawn_pos;
                add_distance_to_front(&mut check, in_front);
                if hull_clear_at(
                    server, world.id, check.x, check.y, check.z, final_yaw, envelope,
                ) && spot_ok(check.x, check.y)
                    && hull_probe_points(check.x, check.y, final_yaw, footprint)
                        .into_iter()
                        .all(|(px, py)| spot_ok(px, py))
                {
                    spawn_pos.set_position(
                        check.x,
                        check.y,
                        check.z,
                        check.rot_x,
                        check.rot_y,
                        check.rot_z,
                    );
                    chosen = "search";
                    deep = true;
                    break;
                }
                in_front += 1.0;
            }
            if !deep {
                if let Some(o) = owner {
                    let w = world.world_pos_rot(&WorldObject::from(o));
                    // The owner may stand on a pier/deck well above the sea; the hull belongs
                    // on the water surface at the fallback point, not at the owner's height.
                    let (fx, fy, fz) = fallback_summon_point(w.x, w.y, spawn_pos.rot_z, |x, y| {
                        super::ship_physics::water_surface(server, world, [x, y, spawn_z])
                    });
                    let mut fallback = spawn_pos;
                    fallback.set_position(
                        fx,
                        fy,
                        fz,
                        spawn_pos.rot_x,
                        spawn_pos.rot_y,
                        spawn_pos.rot_z,
                    );
                    if hull_clear_at(
                        server, world.id, fallback.x, fallback.y, fallback.z, final_yaw, envelope,
                    ) && spot_ok(fallback.x, fallback.y)
                    {
                        chosen = "fallback";
                        spawn_pos = fallback;
                    }
                }
            }
            if tracing::enabled!(tracing::Level::DEBUG) {
                let water = super::ship_physics::water_surface(
                    server,
                    world,
                    [spawn_pos.x, spawn_pos.y, spawn_pos.z],
                );
                let probe = crate::systems::ai::collision::ground_height_probe(
                    server,
                    world.id,
                    spawn_pos.x,
                    spawn_pos.y,
                    water,
                    2.0,
                    2048.0,
                );
                let terrain = crate::systems::ai::collision::terrain_height_or(
                    server,
                    world.id,
                    spawn_pos.x,
                    spawn_pos.y,
                    || f32::NAN,
                );
                let hull_clear = hull_clear_at(
                    server,
                    world.id,
                    spawn_pos.x,
                    spawn_pos.y,
                    spawn_pos.z,
                    final_yaw,
                    envelope,
                );
                tracing::debug!(
                    "Slave {} summon at ({:.1},{:.1},{:.1}) water {water:.1} yaw {final_yaw:.2} adrift {} path {chosen} hull_clear {hull_clear}: ground probe {probe:?}, terrain {terrain:.1}, hull top {hull_top:.1} (model {} mcz {:.2} boxz {:.2} keel {:.2} offset {:.2} bounds top {:?})",
                    template.id,
                    spawn_pos.x,
                    spawn_pos.y,
                    spawn_pos.z,
                    !deep,
                    ship.id,
                    ship.mass_center_z,
                    ship.mass_box_size_z,
                    ship.keel_height,
                    offset_z,
                    hull_bounds.as_ref().map(|b| b.center.z + b.size.z)
                );
                if !deep {
                    for (px, py) in
                        hull_probe_points(spawn_pos.x, spawn_pos.y, final_yaw, footprint)
                    {
                        let surface =
                            super::ship_physics::water_surface(server, world, [px, py, spawn_z]);
                        let probe = crate::systems::ai::collision::ground_height_probe(
                            server, world.id, px, py, spawn_z, 2.0, 2048.0,
                        );
                        let terrain = crate::systems::ai::collision::terrain_height_or(
                            server,
                            world.id,
                            px,
                            py,
                            || {
                                crate::systems::npcs::heightmap::world_height(server, world, px, py)
                                    .unwrap_or(0.0)
                            },
                        );
                        let floor = summon_floor(probe, || terrain);
                        tracing::debug!(
                            "Slave {} summon point ({px:.1},{py:.1}) water {surface:.2} probe {probe:?} terrain {terrain:.2} floor {floor:.2} spot_ok {}",
                            template.id,
                            surface - floor > min_depth
                                && overhead_clear_at(server, world.id, px, py, surface, hull_top)
                        );
                    }
                }
            }
            spawn_pos.z += offset_z;
        } else if template.is_a_boat() {
            // A level boat keeps its authored X/Y and yaw. Its hull rides at the
            // water surface with the same keel offset as a summoned boat. The authored
            // Z only selects the pool: the level editor puts Kroloal Cradle's rowboats
            // on the pool floor.
            let water = super::ship_physics::water_surface(
                server,
                world,
                [spawn_pos.x, spawn_pos.y, spawn_pos.z],
            );
            if let Some(ship) = data.ship_model(template.model_id) {
                if (water - spawn_pos.z).abs() <= LEVEL_BOAT_SURFACE_SEARCH {
                    spawn_pos.set_height(water + summon_offset_z(ship));
                }
            }
        } else {
            // The 1.2 land rule's ground is the terrain heightfield only.
            let h = summon_terrain(server, world, spawn_pos.x, spawn_pos.y);
            if h > 0.0 {
                spawn_pos.set_height(h);
            }
        }
        // Always spawn horizontal(level) and 90° CCW of the player
        spawn_pos.rot_x = 0.0;
        spawn_pos.rot_y = 0.0;
        spawn_pos.rot_z = final_yaw;
    } else if !template.is_a_boat() {
        // A searched or client-chosen land spot carries its surface height already; re-ground
        // it here with the same 1.2 rule as the search: the terrain heightfield only, never a
        // static floor above it (a deck would have blocked the search's line of sight).
        let h = summon_terrain(server, world, spawn_pos.x, spawn_pos.y);
        if h > 0.0 {
            spawn_pos.set_height(h);
        }
    }

    if let Some(committed) = &committed {
        db_id = committed.db_id;
    }
    // Get new Id to save if it has a player as owner
    if owner_id > 0 && db_id == 0 {
        db_id = server.ids.character.get_next_id().ok()?;
    }

    let committed = match (committed, &item, owner) {
        (Some(committed), _, _) => Some(committed),
        (None, Some(item), Some(owner)) => Some(commit_summon_item(server, owner, item, db_id)?),
        _ => None,
    };
    if let (Some(committed), Some(owner)) = (committed, owner) {
        owner.send(SCItemTaskSuccessPacket {
            action: item_task_type::UPDATE_SUMMON_MATE_ITEM,
            tasks: vec![committed.task],
            force_remove: Vec::new(),
        });
    }

    // Create the Slave (packet)
    if let Some(o) = owner {
        broadcast(
            server,
            &WorldObject::from(o),
            &SCSlaveCreatedPacket {
                owner_obj_id,
                tl_id: tl_id as u16,
                slave_obj_id: obj_id,
                hide_spawn_effect,
                unk_id: 0,
                creator_name: owner_name.clone(),
            },
            true,
        );
    }
    let faction = if owner.is_some() {
        owner_faction
    } else {
        template.faction_id
    };
    let mut s = Slave::new(db_id, template.id);
    s.unit.tl_id = tl_id as u16;
    s.unit.name = if slave_name.trim().is_empty() {
        template.name.clone()
    } else {
        slave_name
    };
    s.unit.level = template.level as u8;
    s.unit.model_id = template.model_id;
    s.unit.hp = slave_hp;
    s.unit.mp = slave_mp;
    s.unit.faction_id = faction;
    s.unit.owner_id = owner_id;
    s.summoner_id = owner_id;
    s.summoner_obj_id = owner_obj_id;
    s.summoner_name = owner_name.clone();
    s.summoning_item_id = item.as_ref().map_or(0, |i| i.id);
    s.spawn_time_ms = now_ms();
    s.spawner_id = spawner.map_or(0, |sp| sp.id);
    s.spawner_despawn_time = spawner.map(|sp| sp.def.base.despawn_time);
    s.owner_type = if owner.is_some() {
        BaseUnitType::Character
    } else {
        BaseUnitType::Invalid
    };
    s.owner_obj_id = owner_obj_id;
    let slave = Entity::new(obj_id, s);
    let slave_obj = WorldObject::from(&slave);
    apply_slave_bonuses(server, &slave_obj, &template);
    stats::refresh_object_pools(server, &slave_obj);
    {
        let mut d = slave.data();
        if !is_loaded {
            d.unit.hp = d.unit.max_hp;
            d.unit.mp = d.unit.max_mp;
        }
    }
    // Equip its default items. 10.x: player-owned slave parts live in a persistent
    // `EquipmentSlave` container keyed by the slave DB id (`item_containers.mate_id`), seeded
    // once per hull; the model mirror drives stats and visibility packets.
    if owner_id > 0 && db_id > 0 {
        let existing = server.items.find_container_for_character(
            owner_id,
            crate::models::container::slot_type::EQUIPMENT_SLAVE,
            db_id,
        );
        // A saved 1.2 hull may predate EquipmentSlave containers. Container creation, not
        // hull/item age, marks its one-time initialization; an existing empty one stays empty.
        let is_new_equipment = existing.is_none();
        let container = existing.unwrap_or_else(|| {
            server.items.container_for_character(
                owner_id,
                crate::models::container::slot_type::EQUIPMENT_SLAVE,
                db_id,
                &server.ids.container,
            )
        });
        if is_new_equipment {
            let pack = initial_slave_equipment(server, &template);
            let now = chrono::Utc::now().timestamp();
            for initial in pack {
                if !data.has_slave_equip_slot(template.id, initial.equip_slot_id) {
                    continue;
                }
                let Some(mut it) = server.items.create(
                    initial.item_id,
                    1,
                    0,
                    server.config.id,
                    &server.game_data.items,
                    &server.ids.item,
                    now,
                ) else {
                    continue;
                };
                it.slot_type = crate::models::container::slot_type::EQUIPMENT_SLAVE;
                it.slot = i32::from(initial.equip_slot_id);
                it.owner_id = u64::from(owner_id);
                it.dirty = true;
                if container
                    .lock()
                    .expect("container poisoned")
                    .add_new_item(
                        it.clone(),
                        i32::from(initial.equip_slot_id),
                        &server.game_data.items,
                    )
                    .is_err()
                {
                    continue;
                }
                let entered = {
                    let mut d = slave.data();
                    match d.equipment.get_mut(usize::from(initial.equip_slot_id)) {
                        Some(slot) => {
                            *slot = Some(it.clone());
                            true
                        }
                        None => false,
                    }
                };
                // EquipmentContainer.OnEnterContainer: the SCUnitEquipmentsChanged broadcast
                // reaches nobody (not spawned yet), then ParentUnit.UpdateGearBonuses(...).
                if entered {
                    crate::systems::skills::stats::update_vehicle_gear_bonuses(
                        server,
                        &slave_obj,
                        Some(&it),
                        None,
                    );
                }
            }
        } else {
            let items = container.lock().expect("container poisoned").items.clone();
            let mut d = slave.data();
            for it in items {
                let slot = it.slot.max(0) as usize;
                if let Some(entry) = d.equipment.get_mut(slot) {
                    *entry = Some(it);
                }
            }
        }
    } else {
        let pack = initial_slave_equipment(server, &template);
        // NPC / spawner slaves have no DB row: keep the 1.2 C# in-memory starter pack.
        let now = chrono::Utc::now().timestamp();
        for initial in pack {
            if !data.has_slave_equip_slot(template.id, initial.equip_slot_id) {
                continue;
            }
            let Some(it) = server.items.create_detached(
                initial.item_id,
                1,
                0,
                server.config.id,
                &server.game_data.items,
                now,
            ) else {
                continue;
            };
            let mut it = it;
            it.slot_type = crate::models::container::slot_type::EQUIPMENT_SLAVE;
            it.slot = i32::from(initial.equip_slot_id);
            let entered = {
                let mut d = slave.data();
                match d.equipment.get_mut(usize::from(initial.equip_slot_id)) {
                    Some(slot) => {
                        *slot = Some(it.clone());
                        true
                    }
                    None => false,
                }
            };
            if entered {
                crate::systems::skills::stats::update_vehicle_gear_bonuses(
                    server,
                    &slave_obj,
                    Some(&it),
                    None,
                );
            }
        }
    }
    {
        let mut d = slave.data();
        d.unit.hp = d.unit.hp.min(d.unit.max_hp);
        d.unit.mp = d.unit.mp.min(d.unit.max_mp);
        // C#: "Reset HP on 'dead' vehicles (can't summon with 0 HP)". Only once the repair time
        // has passed: a destroyed vehicle must not come back at full HP for free.
        let repair_pending = item
            .as_ref()
            .is_some_and(|i| repair_seconds_left(i, chrono::Utc::now().timestamp()).is_some());
        if d.unit.hp <= 0 && !repair_pending {
            d.unit.hp = d.unit.max_hp;
        }
        d.unit.transform = spawn_pos;
        d.unit.transform.parent_obj_id = 0;
    }
    if template.is_a_boat() && template.portal_time > 0.0 {
        let pos = world.world_pos_rot(&slave_obj);
        let water_at_final =
            super::ship_physics::water_surface(server, world, [pos.x, pos.y, pos.z]);
        if water_at_final > 0.0 {
            let mul = SHIP_WATER_DENSITY_MUL;
            if mul > 0.0 {
                let target_submerged = 1.0 / (BASE_WATER_DENSITY * mul);
                let target_z = water_at_final - target_submerged;
                let mut d = slave.data();
                if d.unit.transform.z < target_z {
                    d.unit.transform.set_height(target_z);
                }
            }
        }
    }
    let changes = world.spawn(&slave, &server.view_ctx());
    crate::world::session::handle_zone_changes(server, changes);
    Some(Spawned {
        slave,
        template,
        is_loaded,
    })
}

/// The part of `Create` after `summonedSlave.Spawn()` (and, for a loaded slave, after its
/// persistent doodads were spawned).
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_create(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    owner: Option<&PlayerRef>,
    slave: &SlaveRef,
    template: &SlaveTemplate,
    item: Option<&Item>,
    is_loaded: bool,
    saved_children: Vec<SavedChildSlave>,
) -> Option<SlaveRef> {
    let data = &server.game_data.systems.vehicles;
    let slave_obj = WorldObject::from(slave);
    if is_loaded {
        // The persistent doodads were spawned by `create` (async) before this point.
        let ids: Vec<u32> = world
            .all_doodads()
            .into_iter()
            .filter(|d| d.data().parent_obj_id == slave.obj_id)
            .map(|d| d.obj_id)
            .collect();
        let mut d = slave.data();
        for id in ids {
            if !d.attached_doodads.contains(&id) {
                d.attached_doodads.push(id);
            }
        }
    }
    // summonedSlave.UpdateGearBonuses(null, null)
    crate::systems::skills::stats::update_unit_gear_bonuses(server, &slave_obj, None, None);
    crate::systems::dispatch_buff_events(server, &slave_obj);
    let (owner_id, owner_obj_id, slave_db_id, faction, spawn_time) = {
        let d = slave.data();
        (
            d.summoner_id,
            d.summoner_obj_id,
            d.id,
            d.unit.faction_id,
            d.spawn_time_ms,
        )
    };
    // Create all remaining doodads that where not previously loaded
    for binding in &template.doodad_bindings {
        if !is_binding_attach_point(server, template, binding.attach_point_id) {
            continue;
        }
        let loaded_points: Vec<u8> = {
            let ids = slave.data().attached_doodads.clone();
            ids.iter()
                .filter_map(|id| world.doodad(*id))
                .map(|d| d.data().attach_point)
                .collect()
        };
        if loaded_points.contains(&binding.attach_point_id) {
            continue;
        }
        let Some(doodad) = crate::systems::npcs::doodad::manager::create(
            server,
            world,
            0,
            binding.doodad_id,
            None,
            true,
        ) else {
            tracing::error!(
                "Slave {}: doodad template {} missing",
                template.id,
                binding.doodad_id
            );
            continue;
        };
        let item_ucc = item
            .filter(|i| i.flags & crate::models::item::ITEM_FLAG_HAS_UCC != 0 && i.ucc_id > 0)
            .map(|i| i.ucc_id);
        {
            let mut d = doodad.data();
            d.owner_obj_id = owner_obj_id;
            d.parent_obj_id = slave.obj_id;
            d.attach_point = binding.attach_point_id;
            d.owner_id = owner_id;
            d.plant_time = spawn_time / 1000;
            d.owner_type = doodad_owner_type::SLAVE;
            d.owner_db_id = slave_db_id;
            d.data = i32::from(binding.attach_point_id);
            d.faction_id = faction;
            d.type2 = 1;
            d.scale = binding.scale;
            d.transform.instance_id = world.id;
            d.transform.set_position(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            // NOTE (C#): In 1.2 we can't replace slave parts like sail, so the UCC goes to all
            if let Some(u) = item_ucc {
                d.ucc_id = u;
            }
        }
        let start = crate::systems::npcs::doodad::phase::get_func_group_id(
            &server.game_data.systems.npcs.doodad,
            binding.doodad_id,
        );
        crate::systems::npcs::doodad::manager::set_func_group_id(server, &doodad, start);
        let dobj = WorldObject::from(&doodad);
        world.attach_child(&slave_obj, &dobj);
        apply_attach_point_location(server, world, slave, &dobj, binding.attach_point_id);
        slave.data().attached_doodads.push(doodad.obj_id);
        crate::systems::npcs::doodad::manager::init_doodad(server, world, &doodad);
        crate::systems::npcs::doodad::manager::spawn(server, world, &doodad);
        // Only set IsPersistent if the binding is defined as such
        if owner_id > 0 && item.is_some_and(|i| i.id > 0) && binding.persist {
            doodad.data().is_persistent = true;
            crate::systems::npcs::doodad::db::save(server, &doodad);
        }
    }

    // Spawn Slave's slaves
    for binding in &template.slave_bindings {
        if binding.owner_type != "Slave"
            || !is_binding_attach_point(server, template, binding.attach_point_id)
        {
            continue;
        }
        let mut child_db_id = 0u32;
        let mut child_template_id = 0u32;
        let mut child_name = String::new();
        if slave_db_id > 0 {
            if let Some(c) = saved_children
                .iter()
                .find(|c| c.attach_point == binding.attach_point_id)
            {
                child_db_id = c.id;
                child_template_id = c.template_id;
                child_name = c.name.clone();
            }
        }
        if slave_db_id > 0 && child_db_id == 0 {
            child_db_id = server.ids.character.get_next_id().ok()?;
        }
        let tpl_id = if child_template_id > 0 {
            child_template_id
        } else {
            binding.slave_id
        };
        let Some(child_template) = data.slave_template(tpl_id).cloned() else {
            tracing::error!(
                "Slave {}: child template {tpl_id} missing (NullReferenceException)",
                template.id
            );
            return None;
        };
        let child_tl = server.ids.tl.get_next_id().ok()?;
        let child_obj_id = server.ids.object.get_next_id().ok()?;
        let mut c = Slave::new(child_db_id, child_template.id);
        c.unit.tl_id = child_tl as u16;
        c.unit.name = if child_name.trim().is_empty() {
            child_template.name.clone()
        } else {
            child_name
        };
        c.unit.level = child_template.level as u8;
        c.unit.model_id = child_template.model_id;
        c.unit.faction_id = faction;
        c.summoner_id = owner_id;
        c.summoner_obj_id = owner_obj_id;
        c.summoner_name = slave.data().summoner_name.clone();
        c.spawn_time_ms = now_ms();
        c.attach_point_id = binding.attach_point_id as i8;
        c.owner_obj_id = slave.obj_id;
        c.owner_type = BaseUnitType::Slave;
        c.unit.owner_id = slave_db_id;
        let child = Entity::new(child_obj_id, c);
        let child_obj = WorldObject::from(&child);
        apply_slave_bonuses(server, &child_obj, &child_template);
        stats::refresh_object_pools(server, &child_obj);
        let w = world.world_pos_rot(&slave_obj);
        {
            // TODO (C#): persistent HP for child slaves when customization is enabled
            let mut d = child.data();
            d.unit.hp = d.unit.max_hp;
            d.unit.mp = d.unit.max_mp;
            // Transform = summonedSlave.Transform.CloneDetached(child); Parent = slave
            d.unit.transform.instance_id = world.id;
            d.unit
                .transform
                .set_position(w.x, w.y, w.z, w.rot_x, w.rot_y, w.rot_z);
        }
        world.attach_child(&slave_obj, &child_obj);
        apply_attach_point_location(server, world, slave, &child_obj, binding.attach_point_id);
        slave.data().attached_slaves.push(child_obj_id);
        let changes = world.spawn(&child, &server.view_ctx());
        crate::world::session::handle_zone_changes(server, changes);
        let hp = child.data().unit.hp;
        crate::systems::skills::combat::post_update_current_hp(
            server,
            &child_obj,
            Some(&child_obj),
            0,
            hp,
            KillReason::Unknown,
        );
    }

    // Equipment-driven visuals (cannons, sails, masts, cargo, figureheads) from the parts the
    // hull carries.
    spawn_equipment_visuals_from_inventory(server, world, slave);

    // Level boats have no summoner. Ship physics resolves the instance from
    // the registered slave, the same way it does for a player-owned hull.
    if template.is_a_boat() {
        super::ship_physics::add_ship(server, slave);
    }

    if let Some(o) = owner {
        let w = world.world_pos_rot(&slave_obj);
        let p = {
            let d = slave.data();
            SCMySlavePacket {
                obj_id: slave.obj_id,
                tl_id: d.unit.tl_id,
                name: d.unit.name.clone(),
                template_id: d.unit.template_id,
                hp: d.unit.hp,
                max_hp: d.unit.max_hp,
                x: w.x,
                y: w.y,
                z: w.z,
            }
        };
        o.send(p);
    }
    // Save to DB
    save(server, slave);
    let hp = slave.data().unit.hp;
    crate::systems::skills::combat::post_update_current_hp(
        server,
        &slave_obj,
        Some(&slave_obj),
        0,
        hp,
        KillReason::Unknown,
    );
    update_slave_repair_points(server, slave);
    Some(slave.clone())
}
