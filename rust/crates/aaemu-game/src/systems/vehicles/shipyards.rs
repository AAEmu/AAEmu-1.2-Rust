//! `ShipyardManager` (runtime: create, build steps, completion ceremony, decay / tax
//! protection tick, removal) and the `Shipyard` build-step logic (`AddBuildAction`,
//! `CurrentStep`). The templates are `data::VehiclesData::shipyard_templates`.

use std::sync::Mutex;

use rand::Rng;

use crate::models::container::slot_type;
use crate::models::shipyard::{Shipyard, ShipyardData};
use crate::server::GameServer;
use crate::systems::skills::types::KillReason;
use crate::world::{Entity, PlayerRef, ShipyardRef, WorldObject};

use super::slaves::buff;
use super::stats::{self, VehicleKind};
use super::{broadcast, error};
use crate::systems::npcs::doodad::templates::FuncTemplate;

/// `ItemTaskType` values used by the shipyard.
mod item_task {
    pub const SKILL_REAGENTS: u8 = 40;
    pub const SKILL_EFFECT_GAIN_ITEM: u8 = 42;
    pub const SHIPYARD: u8 = 60;
}

/// `ShipyardManager` runtime state (`_shipyard` by shipyard id, `_removedShipyards`).
#[derive(Debug, Default)]
pub struct ShipyardManager {
    shipyards: Mutex<Vec<(u32, ShipyardRef)>>,
    removed: Mutex<Vec<u32>>,
    /// Ids of removed shipyards whose `shipyards` row is not deleted yet (the next save retries).
    pending_delete: Mutex<Vec<u32>>,
}

impl ShipyardManager {
    /// The active shipyards (insertion order).
    pub fn all(&self) -> Vec<ShipyardRef> {
        self.shipyards
            .lock()
            .expect("shipyards poisoned")
            .iter()
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Takes the ids whose rows still have to be deleted. An id that belongs to a live shipyard
    /// again (ids are released at removal and may be reused before the next save) is dropped:
    /// the row is the new shipyard's.
    pub(super) fn take_pending_delete(&self) -> Vec<u32> {
        let live = self.shipyards.lock().expect("shipyards poisoned");
        let mut pending = self.pending_delete.lock().expect("shipyards poisoned");
        let mut ids = std::mem::take(&mut *pending);
        ids.retain(|id| !live.iter().any(|(l, _)| l == id));
        ids
    }

    /// Queues the row of `ship_id` for deletion at the next save.
    pub(super) fn queue_delete(&self, ship_id: u32) {
        self.pending_delete
            .lock()
            .expect("shipyards poisoned")
            .push(ship_id);
    }

    /// The ids whose rows are waiting for deletion (tests).
    #[cfg(test)]
    pub(super) fn pending_delete(&self) -> Vec<u32> {
        self.pending_delete
            .lock()
            .expect("shipyards poisoned")
            .clone()
    }

    /// `_removedShipyards`.
    pub fn removed(&self) -> Vec<u32> {
        self.removed.lock().expect("shipyards poisoned").clone()
    }
}

/// How far from the builder a shipyard can be placed: the housing builder's reach
/// (`caster_in_range`: this radius + 30 m), the same rule as every other client-chosen
/// placement. The client previews the shipyard at the player's feet.
const SHIPYARD_PLACE_RADIUS: f32 = 20.0;

/// `ShipyardManager.Create(owner, shipyardData)` (`CSCreateShipyardPacket`).
#[allow(clippy::too_many_arguments)]
pub fn create(
    server: &GameServer,
    owner: &PlayerRef,
    template_id: u32,
    x: f32,
    y: f32,
    z: f32,
    z_rot: f32,
    design_item: u64,
    step: i32,
) -> Option<ShipyardRef> {
    let template = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&template_id)?
        .clone();
    // The client-chosen position must be finite and near the builder; refused before the design
    // item is looked at, so nothing is consumed.
    if ![x, y, z, z_rot].iter().all(|v| v.is_finite())
        || !crate::systems::housing::placement::caster_in_range(
            server,
            owner,
            x,
            y,
            z,
            SHIPYARD_PLACE_RADIUS,
        )
    {
        tracing::warn!(
            "Shipyard {template_id}: placement ({x}, {y}, {z}) refused for character {}              (not finite or too far)",
            owner.character_id
        );
        error(owner, error::SLAVE_SPAWN_ERROR_INVALID_AREA);
        return None;
    }
    // The client names the design item it placed (`designItem`, a bag item object id). The
    // shipyard is built only from that item, and only when it maps to this shipyard; a refused
    // create leaves the item in the bag.
    let design_template_id = design_item_template(server, owner, &template, design_item)?;
    let (instance, zone, owner_name, owner_faction, owner_id) = {
        let c = owner.data();
        (
            c.unit.transform.instance_id,
            c.unit.transform.zone_id,
            c.unit.name.clone(),
            c.unit.faction_id,
            c.id,
        )
    };
    // `owner.Transform.CloneAsSpawnPosition()`: the owner's world roll / pitch are kept.
    let owner_rot = server
        .world
        .world(instance)
        .map(|w| w.world_pos_rot(&WorldObject::from(owner)))
        .unwrap_or_default();
    let Some(step_tpl) = template.step(step).copied() else {
        tracing::error!("Shipyard {template_id} has no step {step} (KeyNotFoundException)");
        return None;
    };
    let obj_id = server.ids.object.get_next_id().ok()?;
    let Some(ship_id) = server.ids.shipyard.get_next_id().ok() else {
        server.ids.object.release_id(obj_id);
        tracing::error!("Shipyard {template_id}: no free shipyard id");
        return None;
    };
    let data = ShipyardData {
        id: u64::from(ship_id),
        template_id: template.id,
        x,
        y,
        z,
        z_rot,
        money_amount: 0,
        actions: step,
        // capture-pending: the C# wrote `OriginItemId` (0 for shipyards 2-4, 10, 11); the
        // 1.2 client's meaning of `ShipyardData.Type` is unproven, so the value is unchanged.
        kind: template.origin_item_id,
        kind2: owner_id,
        owner_name: owner_name.clone(),
        kind3: owner_faction,
        spawned: chrono::Utc::now().timestamp(),
        obj_id,
        hp: step_tpl.max_hp * 100,
        step,
    };
    let sy = build_shipyard(
        server,
        &template,
        data,
        &step_tpl,
        (instance, zone),
        (owner_rot.rot_x, owner_rot.rot_y),
    );
    let shipyard = Entity::new(obj_id, sy);

    // we will make checks for the availability of money and items to create a shipyard;
    // `remove_required_items` sends the one refusal error itself.
    if !remove_required_items(server, owner, &template, design_item, design_template_id) {
        server.ids.object.release_id(obj_id);
        server.ids.shipyard.release_id(ship_id);
        return None;
    }
    spawn_shipyard(server, ship_id, &shipyard);
    Some(shipyard)
}

/// The design item template of the bag item `design_item` the client named, when it builds
/// `template`; `None` (and the refusal error) otherwise.
///
/// The item's template is mapped through `item_shipyards` (`item_id -> shipyard_id`, the table
/// the 1.2 client loads with `FUN_396cd6e0`). Shipyards without a row (only 1 in the 1.2 data)
/// keep the old `origin_item_id` path. The 10.x server (`aaemu-world-10x`
/// `ShipyardManager.RemoveRequiredItems`) proves the packet field is the item *object* id: it
/// looks the item up by it, checks `OwnerId` / `SlotType.Inventory` and passes it as the
/// preferred item to `ConsumeItem`. No 1.2 decompile of the check itself was traced
/// (capture-pending).
fn design_item_template(
    server: &GameServer,
    owner: &PlayerRef,
    template: &super::data::ShipyardTemplate,
    design_item: u64,
) -> Option<u32> {
    let item = super::mates::owned_item(server, owner, design_item);
    let Some(item) = item.filter(|i| {
        i.owner_id == u64::from(owner.character_id) && i.slot_type == slot_type::INVENTORY
    }) else {
        error(owner, error::NOT_ENOUGH_ITEM);
        tracing::warn!(
            "Shipyard {}: design item {design_item} is not a bag item of character {}",
            template.id,
            owner.character_id
        );
        return None;
    };
    let mapped = server
        .game_data
        .systems
        .vehicles
        .item_shipyards
        .get(&item.template_id)
        .copied();
    let builds_this = match mapped {
        Some(shipyard_id) => shipyard_id == template.id,
        None => template.origin_item_id != 0 && template.origin_item_id == item.template_id,
    };
    if !builds_this {
        error(owner, error::NOT_ENOUGH_ITEM);
        tracing::warn!(
            "Shipyard {}: design item {design_item} (template {}) does not build it (item_shipyards {:?}, origin_item_id {})",
            template.id,
            item.template_id,
            mapped,
            template.origin_item_id
        );
        return None;
    }
    Some(item.template_id)
}

/// The `Shipyard` of `create`: transform, owner faction / name, the current step's model and the
/// pools. Shared with the restore at server start (`shipyards_db`).
pub(super) fn build_shipyard(
    server: &GameServer,
    template: &super::data::ShipyardTemplate,
    data: ShipyardData,
    step_tpl: &super::data::ShipyardStep,
    (instance, zone): (u32, u32),
    (rot_x, rot_y): (f32, f32),
) -> Shipyard {
    let (x, y, z, z_rot) = (data.x, data.y, data.z, data.z_rot);
    let owner_faction = data.kind3;
    let owner_name = data.owner_name.clone();
    let mut sy = Shipyard::new(data);
    sy.unit.transform.instance_id = instance;
    sy.unit.transform.zone_id = zone;
    sy.unit.transform.set_position(x, y, z, rot_x, rot_y, z_rot);
    sy.unit.faction_id = owner_faction;
    sy.unit.level = 30;
    sy.unit.name = owner_name;
    sy.unit.model_id = step_tpl.model_id;
    // Template setter: AllAction = sum of the steps' NumActions
    sy.all_action = template.all_actions();
    stats::refresh_pools(server, VehicleKind::Shipyard, &mut sy.unit);
    sy.unit.hp = sy.unit.max_hp;
    sy
}

/// Registers the shipyard in the manager and spawns it into its world instance.
pub(super) fn spawn_shipyard(server: &GameServer, ship_id: u32, shipyard: &ShipyardRef) {
    server
        .systems
        .vehicles
        .shipyards
        .shipyards
        .lock()
        .expect("shipyards poisoned")
        .push((ship_id, shipyard.clone()));
    let instance = shipyard.data().unit.transform.instance_id;
    if let Some(world) = server.world.world(instance) {
        let changes = world.spawn(shipyard, &server.view_ctx());
        crate::world::session::handle_zone_changes(server, changes);
    }
}

/// `RemoveRequiredItems(shipyard)`. `design_item` is the bag item object the client named and
/// `design_template_id` its template (already mapped to this shipyard by [`design_item_template`]).
fn remove_required_items(
    server: &GameServer,
    character: &PlayerRef,
    template: &super::data::ShipyardTemplate,
    design_item: u64,
    design_template_id: u32,
) -> bool {
    let items = server.systems.skills.ext.items();
    let design_id = design_template_id;
    let Some(&money_owed) = server
        .game_data
        .systems
        .vehicles
        .taxations
        .get(&(template.taxation_id as u32))
    else {
        error(character, error::NOT_ENOUGH_ITEM);
        tracing::error!(
            "Shipyard {}: taxation {} missing (KeyNotFoundException)",
            template.id,
            template.taxation_id
        );
        return false;
    };
    if character.data().money < i64::from(money_owed) {
        error(character, error::NOT_ENOUGH_MONEY);
        return false;
    }
    let Some((use_skill_id, _)) = items.item_use_skill(server, design_id) else {
        error(character, error::NOT_ENOUGH_ITEM);
        tracing::error!("Could not find Reagents/Products for item template {design_id}");
        return false;
    };
    let skills = &server.game_data.systems.skills;
    let reagents = skills.reagents_for(use_skill_id);
    let products = skills.products_for(use_skill_id);
    if !reagents.is_empty() {
        let mut enough = true;
        for r in &reagents {
            if items.item_count(server, character, r.item_id) >= i32::from(r.amount) {
                continue;
            }
            enough = false;
            tracing::error!("Not enough reagents Id={}, Amount={}", r.item_id, r.amount);
        }
        if !enough {
            error(character, error::NOT_ENOUGH_ITEM);
            return false;
        }
    }
    // Consume the named design item first, in one bag lock: a racing removal / slot change
    // refuses the build before any reagent or money is spent, and the item is kept. The old
    // code consumed `template.origin_item_id`, which let a client build any shipyard it held
    // the matching origin item for and made 10 / 11 (origin 0) impossible.
    match items.consume_bag_item_by_id(
        server,
        character,
        item_task::SHIPYARD,
        design_item,
        design_id,
        1,
    ) {
        Some(true) => {}
        Some(false) => {
            error(character, error::NOT_ENOUGH_ITEM);
            tracing::warn!(
                "Shipyard {}: design item {design_item} (template {design_id}) could not be consumed",
                template.id
            );
            return false;
        }
        // Hooks that do not track items (unit tests): keep the old template consume.
        None => {
            if items.consume_bag_item(server, character, item_task::SHIPYARD, design_id, 1) < 1 {
                error(character, error::NOT_ENOUGH_ITEM);
                tracing::warn!("Shipyard {}: not enough item Id={design_id}", template.id);
                return false;
            }
        }
    }
    for r in &reagents {
        items.consume_bag_item(
            server,
            character,
            item_task::SKILL_REAGENTS,
            r.item_id,
            i32::from(r.amount),
        );
    }
    for p in &products {
        items.acquire_default_item(
            server,
            character,
            item_task::SKILL_EFFECT_GAIN_ITEM,
            p.item_id,
            i32::from(p.amount),
        );
    }
    // SubtractMoney(SlotType.Inventory, moneyOwed, ItemTaskType.Shipyard)
    character.data().money -= i64::from(money_owed);
    character.send(
        crate::systems::social::packets::expedition::item_task_money_packet(
            item_task::SHIPYARD,
            -(money_owed as i32),
        ),
    );
    true
}

/// `RemoveShipyard(shipyard)`.
pub fn remove_shipyard(server: &GameServer, shipyard: &ShipyardRef) {
    let ship_id = shipyard.data().data.id as u32;
    let m = &server.systems.vehicles.shipyards;
    m.removed.lock().expect("shipyards poisoned").push(ship_id);
    m.shipyards
        .lock()
        .expect("shipyards poisoned")
        .retain(|(id, _)| *id != ship_id);
    m.queue_delete(ship_id);
    super::shipyards_db::delete_now(server, ship_id);
    server.ids.shipyard.release_id(ship_id);
    server.ids.object.release_id(shipyard.obj_id);
    if let Some(world) = server.world.world_of(&WorldObject::from(shipyard)) {
        world.delete(shipyard);
    }
}

/// `Shipyard.OnDeath` (`Events.OnDeath` subscription): the `shipyard_rewards` debris, then
/// `RemoveShipyard(this)`.
pub fn on_death(server: &GameServer, shipyard: &ShipyardRef) {
    {
        let d = shipyard.data();
        tracing::debug!(
            "Shipyard died ObjId:{} - TemplateId:{} - {}",
            shipyard.obj_id,
            d.data.template_id,
            d.data.owner_name
        );
    }
    // INFERRED trigger: `shipyard_rewards` describes wreckage around a destroyed shipyard
    // (`doodad_id`, `on_water`, `radius`, `count`). Nothing in the 1.2 client strings or the
    // code read says whether it fires on death or on completion; death is the schema's reading
    // (debris). Goal: a capture of a real shipyard destruction. Do not spawn it on normal
    // completion unless that capture shows it.
    spawn_rewards(server, shipyard);
    remove_shipyard(server, shipyard);
}

/// Spawns the `shipyard_rewards` doodads around a shipyard. Called from [`on_death`] only (the
/// trigger is INFERRED, see there).
pub fn spawn_rewards(server: &GameServer, shipyard: &ShipyardRef) {
    let (template_id, instance, x, y, z, rot_x, rot_y) = {
        let s = shipyard.data();
        (
            s.data.template_id,
            s.unit.transform.instance_id,
            s.unit.transform.x,
            s.unit.transform.y,
            s.unit.transform.z,
            s.unit.transform.rot_x,
            s.unit.transform.rot_y,
        )
    };
    let Some(rewards) = server
        .game_data
        .systems
        .vehicles
        .shipyard_rewards
        .get(&template_id)
    else {
        return;
    };
    let Some(world) = server.world.world(instance) else {
        return;
    };
    let mut rng = rand::thread_rng();
    for reward in rewards {
        let Some(tpl) = server
            .game_data
            .systems
            .npcs
            .doodad
            .template(reward.doodad_id)
        else {
            tracing::warn!(
                "Shipyard {template_id}: reward doodad {} is not in the game data",
                reward.doodad_id
            );
            continue;
        };
        // Doodad 1330 has its own chain (group 2146 Timer 1159, 300 s -> 2147 LootItem 740
        // -> 2147 Final 451, 120 s); do not replace its FuncTask with our delete task.
        // Only a template with no Timer/Final phase func gets the fallback lifetime.
        let fallback_lifetime = (!has_authored_lifetime(server, reward.doodad_id)).then(|| {
            reward_lifetime_ms(&mut rng, tpl.min_time, tpl.max_time)
                .unwrap_or(REWARD_FALLBACK_LIFETIME_MS)
        });
        let points = reward_points(&mut rng, (x, y), reward.radius, reward.count);
        tracing::debug!(
            "Shipyard {template_id}: spawning {} reward doodads {} within {} m",
            points.len(),
            reward.doodad_id,
            reward.radius
        );
        for (px, py) in points {
            let pz = reward_ground(server, &world, reward.on_water, px, py, z);
            let Some(doodad) = crate::systems::npcs::doodad::manager::create(
                server,
                &world,
                0,
                reward.doodad_id,
                None,
                true,
            ) else {
                continue;
            };
            {
                let mut d = doodad.data();
                d.transform.set_position(
                    px,
                    py,
                    pz,
                    rot_x,
                    rot_y,
                    rng.gen::<f32>() * std::f32::consts::TAU,
                );
                d.scale = 1.0;
                d.faction_id = crate::data::world_data::factions::FRIENDLY;
            }
            crate::systems::npcs::doodad::manager::init_doodad(server, &world, &doodad);
            crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
            if let Some(lifetime) = fallback_lifetime {
                crate::systems::npcs::doodad::tasks::start_func_task(
                    server,
                    &doodad,
                    None,
                    crate::systems::npcs::doodad::tasks::FuncTaskKind::Delete,
                    std::time::Duration::from_millis(lifetime),
                );
            }
        }
    }
}

/// `count` random points uniformly inside the disc of `radius` around `center` (the reward
/// area; a negative count / radius yields no points).
pub(crate) fn reward_points(
    rng: &mut impl rand::Rng,
    center: (f32, f32),
    radius: f32,
    count: i32,
) -> Vec<(f32, f32)> {
    let radius = radius.max(0.0);
    (0..count.max(0))
        .map(|_| {
            let angle = rng.gen::<f32>() * std::f32::consts::TAU;
            let distance = radius * rng.gen::<f32>().sqrt();
            (
                center.0 + distance * angle.cos(),
                center.1 + distance * angle.sin(),
            )
        })
        .collect()
}

/// Fallback lifetime for reward doodads whose template has no authored Timer/Final phase
/// function, and no `doodad_almighties.min_time`-`max_time` values.
const REWARD_FALLBACK_LIFETIME_MS: u64 = 600_000;

/// Whether the doodad template's own phase chain ends the object (a `DoodadFuncTimer` with a
/// next phase, or any `DoodadFuncFinal`). Such a template must keep its `FuncTask`, so the
/// reward spawn adds no delete task for it.
fn has_authored_lifetime(server: &GameServer, template_id: u32) -> bool {
    let data = &server.game_data.systems.npcs.doodad;
    data.doodad_func_groups(template_id).iter().any(|g| {
        data.phase_funcs(g.id).iter().any(|pf| {
            match data.phase_func_template(pf.func_id, &pf.func_type) {
                Some(FuncTemplate::Timer(f)) => f.next_phase > 0,
                Some(FuncTemplate::Final(_)) => true,
                _ => false,
            }
        })
    })
}

/// Fallback lifetime in `[min_time, max_time]` ms (`doodad_almighties`), `None` when the template
/// has neither. These columns are not the authored lifetime (1330's own chain is the 300 s
/// Timer -> 120 s Final above); they only bound reward doodads without a phase lifetime.
fn reward_lifetime_ms(rng: &mut impl rand::Rng, min_time: i32, max_time: i32) -> Option<u64> {
    if max_time <= 0 {
        return None;
    }
    let min = min_time.max(0).min(max_time);
    let ms = if max_time > min {
        rng.gen_range(min..=max_time)
    } else {
        min
    };
    Some(ms.max(1) as u64)
}

/// The reward point's height: the water surface for an `on_water` row that lands in water,
/// otherwise the ground (the same collision / heightmap fallback the backpack drop uses).
fn reward_ground(
    server: &GameServer,
    world: &std::sync::Arc<crate::world::WorldInstance>,
    on_water: bool,
    x: f32,
    y: f32,
    fallback_z: f32,
) -> f32 {
    let ground =
        crate::systems::ai::collision::ground_height_or(server, world.id, x, y, fallback_z, || {
            crate::systems::npcs::heightmap::world_height(server, world, x, y).unwrap_or(fallback_z)
        });
    if on_water {
        let surface = super::ship_physics::water_surface(server, world, [x, y, ground]);
        // Test the water at the seabed, not at the shipyard's (above-surface) z: a strict
        // `is_water` at the shipyard z is false over deep water, which put the debris on the
        // seabed.
        if super::ship_physics::is_water(server, world, [x, y, ground]) || ground < surface {
            return surface;
        }
    }
    ground
}

/// `BaseUnit.GetDistanceTo`: the shipyard part (`BuildRadius * Scale` instead of the model
/// radius). Shipyard models are prefab models without an actor radius, so the build radius
/// stands in for the footprint; without it the 4 m build skills only reach the center point of
/// a 25 m drydock. `None` for other objects.
pub fn shipyard_radius(server: &GameServer, obj: &WorldObject) -> Option<f32> {
    let shipyard = obj.as_shipyard()?;
    let (template_id, scale) = {
        let s = shipyard.data();
        (s.data.template_id, s.unit.scale)
    };
    let r = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&template_id)
        .map_or(0.0, |t| t.build_radius as f32);
    Some(r * scale)
}

/// `CurrentStep` setter (`ModelId`, `BaseAction`).
pub(super) fn set_current_step(server: &GameServer, s: &mut Shipyard, step: i32) {
    s.current_step = step;
    s.is_dirty = true;
    let Some(t) = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&s.data.template_id)
    else {
        return;
    };
    s.unit.model_id = if step == -1 {
        t.main_model_id
    } else {
        t.step(step).map_or(s.unit.model_id, |x| x.model_id)
    };
    if step <= 0 {
        return;
    }
    s.base_action = (0..step)
        .map(|i| t.step(i).map_or(0, |x| x.num_actions))
        .sum();
}

/// `Shipyard.AddBuildAction()`.
pub fn add_build_action(server: &GameServer, shipyard: &ShipyardRef) {
    let mut s = shipyard.data();
    if s.current_step == -1 {
        return;
    }
    let Some(t) = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&s.data.template_id)
    else {
        return;
    };
    let Some(step) = t.step(s.current_step).copied() else {
        tracing::error!(
            "Shipyard {}: no step {} (KeyNotFoundException)",
            s.data.template_id,
            s.current_step
        );
        return;
    };
    let count = t.steps.len() as i32;
    let next_action = s.num_action + 1;
    if step.num_actions > next_action {
        s.num_action = next_action;
        s.is_dirty = true;
    } else {
        s.num_action = 0;
        s.is_dirty = true;
        let next_step = s.current_step + 1;
        if count > next_step {
            set_current_step(server, &mut s, next_step);
        } else {
            set_current_step(server, &mut s, -1);
        }
    }
}

/// The skill id the current build step requires (`shipStep.SkillId`); `None` when there is no
/// step template (`shipStep == null`).
pub fn current_step_template(server: &GameServer, shipyard: &ShipyardRef) -> Option<u32> {
    let s = shipyard.data();
    let t = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&s.data.template_id)?;
    let current_action = s.base_action + s.num_action;
    if current_action >= 0 && s.current_step < t.steps.len() as i32 {
        if s.current_step == -1 {
            // C#: `ShipyardSteps[-1]` throws KeyNotFoundException.
            tracing::error!(
                "Shipyard {}: ShipyardSteps[-1] (KeyNotFoundException)",
                t.id
            );
        }
        t.step(s.current_step).map(|x| x.skill_id)
    } else {
        None
    }
}

/// The `ShipyardData.Actions` / `Step` update of `CraftEffect` after a build action.
pub fn sync_build_state(server: &GameServer, shipyard: &ShipyardRef) {
    let mut s = shipyard.data();
    let count = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&s.data.template_id)
        .map_or(0, |t| t.steps.len() as i32);
    if s.current_step == -1 {
        s.data.actions = s.all_action;
        s.data.step = count;
        tracing::trace!("[Shipyard] Actions {}, Step {count}", s.all_action);
    } else {
        s.data.actions = s.base_action + s.num_action;
        s.data.step = s.current_step;
    }
}

/// `ShipyardCompletedTask(shipyard)`: the finished ship item, step 1000 (launch ceremony),
/// `ShipyardCompleted` after `CeremonyAnimTime`.
pub fn shipyard_completed_task(server: &GameServer, shipyard: &ShipyardRef) {
    let (owner_name, template_id) = {
        let s = shipyard.data();
        (s.data.owner_name.clone(), s.data.template_id)
    };
    let Some(character) = server.world.character_by_name(&owner_name) else {
        tracing::error!(
            "ShipyardCompletedTask: owner {owner_name} offline (NullReferenceException)"
        );
        return;
    };
    let Some(template) = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&template_id)
        .cloned()
    else {
        return;
    };
    server.systems.skills.ext.items().acquire_default_item(
        server,
        &character,
        item_task::SHIPYARD,
        template.item_id,
        1,
    );
    let data = {
        let mut s = shipyard.data();
        s.data.step = 1000; // last step, the ceremony of launching the ship
        s.data.clone()
    };
    broadcast(
        server,
        &WorldObject::from(&character),
        &crate::packets::world_objects::SCShipyardStatePacket { data },
        true,
    );
    let delay = template.ceremony_anim_time.max(0) as u64;
    let (Some(server), Ok(rt)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
        return;
    };
    let shipyard = shipyard.clone();
    rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        shipyard_completed(&server, &shipyard).await;
    });
}

/// `ShipyardCompleted(shipyard)` (`ShipyardCompleteTask`). Note (C#): the `SkillItem` it builds
/// only has `ItemId` set, so `SlaveManager.Create(owner, skillData, ...)` returns before creating
/// the ship (its `ItemTemplateId == 0` check); the owner's active vehicle is still removed.
pub async fn shipyard_completed(server: &GameServer, shipyard: &ShipyardRef) {
    let (owner_name, template_id) = {
        let s = shipyard.data();
        (s.data.owner_name.clone(), s.data.template_id)
    };
    if let Some(character) = server.world.character_by_name(&owner_name) {
        let item_id = server
            .game_data
            .systems
            .vehicles
            .shipyard_templates
            .get(&template_id)
            .and_then(|t| first_bag_item(&character, t.item_id));
        if let Some(item_id) = item_id {
            // `(SkillItem)SkillCaster.GetByType(Item)` with only `ItemId` set.
            let item_template_id = 0;
            let pos = shipyard.data().unit.transform;
            super::slaves::create_for_owner(
                server,
                &character,
                item_id,
                item_template_id,
                true,
                Some(pos),
                None,
                0.0,
            )
            .await;
        }
    } else {
        tracing::error!("ShipyardCompleted: owner {owner_name} offline (NullReferenceException)");
    }
    remove_shipyard(server, shipyard);
}

/// `Inventory.Bag.GetAllItemsByTemplate(templateId, -1)` first item id.
fn first_bag_item(character: &PlayerRef, template_id: u32) -> Option<u64> {
    let bag = character
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .cloned()?;
    let c = bag.lock().expect("container poisoned");
    let (ids, _) = c.items_by_template(template_id, -1);
    ids.first().copied()
}

/// `ShipyardTickTask` start delay (`TimeSpan.FromMinutes(1)`).
pub const TICK_START_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
/// `ShipyardTickTask` repeat interval (`TimeSpan.FromMinutes(1)`).
pub const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// `ShipyardTick()` (every minute): tax protection / deterioration of every shipyard.
pub fn shipyard_tick(server: &GameServer) {
    for s in server.systems.vehicles.shipyards.all() {
        update_shipyard_info(server, &s);
    }
}

pub(super) fn update_shipyard_info(server: &GameServer, shipyard: &ShipyardRef) {
    let spawned = shipyard.data().data.spawned;
    let now = chrono::Utc::now().timestamp();
    let is_decaying = now >= spawned + 3 * 24 * 3600;
    set_protection_buff(server, shipyard, is_decaying, now);
    set_decay_buff(server, shipyard, is_decaying);
}

fn set_protection_buff(server: &GameServer, shipyard: &ShipyardRef, is_decay: bool, now: i64) {
    let obj = WorldObject::from(shipyard);
    let has = shipyard.data().unit.buffs.check_buff(buff::TAX_PROTECTION);
    if !is_decay {
        let (spawned, template_id) = {
            let s = shipyard.data();
            (s.data.spawned, s.data.template_id)
        };
        let duration_min = (spawned - now) as f64 / 60.0;
        let mins = duration_min.round_ties_even() * 60000.0;
        let tax_duration = server
            .game_data
            .systems
            .vehicles
            .shipyard_templates
            .get(&template_id)
            .map_or(0, |t| t.tax_duration);
        let time_left = f64::from(tax_duration) + mins;
        if has {
            return;
        }
        if server
            .game_data
            .systems
            .skills
            .buff(buff::TAX_PROTECTION)
            .is_some()
        {
            crate::systems::skills::buffs::add_buff_by_id(
                server,
                &obj,
                buff::TAX_PROTECTION,
                &obj,
                time_left as i32,
            );
        } else {
            tracing::error!("Unable to find Protection Buff template");
        }
    } else if has {
        crate::systems::skills::buffs::remove_buff(server, &obj, buff::TAX_PROTECTION);
    }
}

fn set_decay_buff(server: &GameServer, shipyard: &ShipyardRef, is_decay: bool) {
    let obj = WorldObject::from(shipyard);
    let has = shipyard.data().unit.buffs.check_buff(buff::DETERIORATION);
    if is_decay {
        if has {
            crate::systems::skills::combat::reduce_current_hp(
                server,
                &obj,
                Some(&obj),
                7,
                KillReason::Damage,
            );
            let owner_name = shipyard.data().data.owner_name.clone();
            let Some(character) = server.world.character_by_name(&owner_name) else {
                tracing::error!(
                    "Shipyard decay: owner {owner_name} offline (NullReferenceException)"
                );
                return;
            };
            let (state, data) = {
                let mut s = shipyard.data();
                let state = crate::world::unit_state::shipyard_unit_state(
                    shipyard.obj_id,
                    &mut s,
                    &owner_name,
                    crate::world::now_ms(),
                );
                (state, s.data.clone())
            };
            character.send(state);
            character.send(crate::packets::world_objects::SCShipyardStatePacket { data });
            return;
        }
        if server
            .game_data
            .systems
            .skills
            .buff(buff::DETERIORATION)
            .is_some()
        {
            crate::systems::skills::buffs::add_buff_by_id(
                server,
                &obj,
                buff::DETERIORATION,
                &obj,
                0,
            );
        } else {
            tracing::error!("Unable to find Deterioration Debuff template");
        }
    } else if has {
        crate::systems::skills::buffs::remove_buff(server, &obj, buff::DETERIORATION);
    }
}

#[cfg(test)]
#[path = "shipyards_tests.rs"]
mod port_tests;
