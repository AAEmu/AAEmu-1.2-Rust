use std::sync::Arc;

use crate::models::container::slot_type;
use crate::models::item::{Item, ItemClass};
use crate::models::transform::Transform;
use crate::server::GameServer;
use crate::systems::skills::types::SkillCaster;
use crate::world::{PlayerRef, SlaveRef, WorldInstance, WorldObject};

use super::despawn::{can_delete, try_delete};
use super::persistence::load_hull;
use super::spawn::{commit_summon_item, create_spawn_committed, finish_create};
use super::summon_placement::{
    revalidated_searched_placement, summon_candidate_1_2, summon_placement_clear_1_2,
    summon_terrain, validate_summon_placement, SUMMON_MAX_TERRAIN_ABOVE_OWNER,
};
use super::{
    error, item_has_other_live_root, owned_roots, player_world, save, LoadedHull, SummonKey,
};

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

pub(super) fn owned_summon_item(
    server: &GameServer,
    owner: &PlayerRef,
    item_id: u64,
    item_template_id: u32,
) -> Option<Item> {
    let item = super::mates::owned_item(server, owner, item_id)?;
    (item.owner_id == u64::from(owner.character_id)
        && item.slot_type == slot_type::INVENTORY
        && item.class == ItemClass::SummonSlave
        && item.template_id == item_template_id)
        .then_some(item)
}

/// `SlaveManager.Create(owner, skillData)` (the `SpawnSlave` special effect): runs the async
/// creation on a task. `placement` is the `SummonPos` spot the client chose; it is validated
/// against the slave's `spawn_valid_area_range` / the skill range before use.
pub fn create_from_skill(
    server: &GameServer,
    owner: &PlayerRef,
    caster: &SkillCaster,
    placement: Option<crate::systems::skills::ext::SummonPlacement>,
    max_range: f32,
    client_spawn: Option<bool>,
) {
    let SkillCaster::Item {
        item_id,
        item_template_id,
        ..
    } = *caster
    else {
        return;
    };
    let Some(server) = server.self_arc() else {
        return;
    };
    let owner = owner.clone();
    // Capture synchronously, before the detached future can be delayed: a later refusal must
    // only roll back the cooldowns this cast started.
    let cooldown = summon_cooldown_token(&server, &owner, item_template_id);
    // Construct the lease before the detached future exists, so even a never-polled task
    // remains pending evidence. The captured lease drops on no runtime, abort and unwind.
    let lease = server
        .systems
        .vehicles
        .slave_summons
        .diagnostics
        .summon(owner.character_id, item_id);
    let fut = async move {
        create_for_owner_job(
            &server,
            &owner,
            item_id,
            item_template_id,
            client_spawn.unwrap_or(false),
            None,
            placement,
            max_range,
            client_spawn.is_some(),
            lease,
            cooldown,
        )
        .await;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn(fut);
        }
        Err(_) => tracing::error!("SlaveManager.Create: no runtime"),
    }
}

/// `Create(owner, skillData, hideSpawnEffect, positionOverride)`.
#[allow(clippy::too_many_arguments)]
pub fn create_for_owner<'a>(
    server: &'a GameServer,
    owner: &'a PlayerRef,
    item_id: u64,
    item_template_id: u32,
    hide_spawn_effect: bool,
    position_override: Option<Transform>,
    placement: Option<crate::systems::skills::ext::SummonPlacement>,
    max_range: f32,
) -> impl std::future::Future<Output = Option<SlaveRef>> + Send + 'a {
    // Capture synchronously (the returned future may be polled much later): a refusal must
    // only roll back the cooldowns this cast started.
    let cooldown = summon_cooldown_token(server, owner, item_template_id);
    let lease = server
        .systems
        .vehicles
        .slave_summons
        .diagnostics
        .summon(owner.character_id, item_id);
    create_for_owner_job(
        server,
        owner,
        item_id,
        item_template_id,
        hide_spawn_effect,
        position_override,
        placement,
        max_range,
        false,
        lease,
        cooldown,
    )
}

#[allow(clippy::too_many_arguments)]
async fn create_for_owner_job(
    server: &GameServer,
    owner: &PlayerRef,
    item_id: u64,
    item_template_id: u32,
    hide_spawn_effect: bool,
    position_override: Option<Transform>,
    placement: Option<crate::systems::skills::ext::SummonPlacement>,
    max_range: f32,
    client_placement: bool,
    lease: super::audit::Lease,
    cooldown: Option<SummonCooldownToken>,
) -> Option<SlaveRef> {
    lease.phase("validating");
    let world = player_world(server, owner)?;
    if item_id == 0 || item_template_id == 0 {
        return None;
    }
    // skillData.SkillSourceItem = ItemManager.GetItemByItemId(ItemId)
    let item = owned_summon_item(server, owner, item_id, item_template_id)?;
    let slave_id = match server
        .game_data
        .items
        .template(item.template_id)
        .map(|t| &t.kind)
    {
        Some(crate::data::items::TemplateKind::SummonSlave { slave_id }) => *slave_id,
        _ => return None,
    };
    let template = server
        .game_data
        .systems
        .vehicles
        .slave_template(slave_id)?
        .clone();
    // The 1.2 client search needs the zone collision world. Without it (tests, physics off)
    // the old C#-style placement stays in charge.
    let collision = crate::systems::ai::collision::files(server).is_some();
    let (position_override, searched) = if placement.is_some() {
        let Some(transform) = validate_summon_placement(
            server,
            &world,
            owner,
            &template,
            placement,
            max_range,
            client_placement,
        ) else {
            // The placement was refused (error 215): the summon never happens, so do not leave
            // the skill cooldown/GCD that the skill system started.
            refund_summon_cooldown(owner, cooldown);
            return None;
        };
        if collision && !summon_placement_clear_1_2(server, &world, owner, &template, &transform) {
            error(owner, error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE);
            refund_summon_cooldown(owner, cooldown);
            return None;
        }
        (Some(transform), false)
    } else if position_override.is_some() || !collision {
        (position_override, false)
    } else {
        match summon_candidate_1_2(server, &world, owner, &template) {
            Some(transform) => (Some(transform), true),
            None => {
                error(owner, error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE);
                refund_summon_cooldown(owner, cooldown);
                return None;
            }
        }
    };
    create_with_placement(
        server,
        &world,
        Some(owner),
        slave_id,
        Some(item),
        hide_spawn_effect,
        position_override,
        placement,
        max_range,
        searched,
        client_placement,
        cooldown,
        &lease,
    )
    .await
}

/// `Create(owner, null, templateId, item, hideSpawnEffect, positionOverride)` with the DB parts
/// (saved slave, its persistent doodads, its saved child slaves).
pub fn create<'a>(
    server: &'a GameServer,
    world: &'a Arc<WorldInstance>,
    owner: Option<&'a PlayerRef>,
    template_id: u32,
    item: Option<Item>,
    hide_spawn_effect: bool,
    position_override: Option<Transform>,
) -> impl std::future::Future<Output = Option<SlaveRef>> + Send + 'a {
    let lease = server.systems.vehicles.slave_summons.diagnostics.summon(
        owner.map_or(0, |o| o.character_id),
        item.as_ref().map_or(0, |i| i.id),
    );
    async move {
        create_with_placement(
            server,
            world,
            owner,
            template_id,
            item,
            hide_spawn_effect,
            position_override,
            None,
            0.0,
            false,
            false,
            None,
            &lease,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_with_placement(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    owner: Option<&PlayerRef>,
    template_id: u32,
    item: Option<Item>,
    hide_spawn_effect: bool,
    mut position_override: Option<Transform>,
    placement: Option<crate::systems::skills::ext::SummonPlacement>,
    max_range: f32,
    searched: bool,
    client_placement: bool,
    cooldown: Option<SummonCooldownToken>,
    lease: &super::audit::Lease,
) -> Option<SlaveRef> {
    server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)?;
    if position_override.is_some_and(|t| {
        [t.x, t.y, t.z, t.rot_x, t.rot_y, t.rot_z]
            .iter()
            .any(|v| !v.is_finite())
    }) {
        return None;
    }
    let summons = &server.systems.vehicles.slave_summons;
    let owner_id = owner.map_or(0, |o| o.character_id);
    // Acquire in one order. Owned async guards release on all errors and future cancellation.
    lease.phase("waiting_owner");
    let _owner_reservation = match owner {
        Some(_) => Some(
            summons
                .reservation(SummonKey::Owner(owner_id))
                .lock_owned()
                .await,
        ),
        None => None,
    };
    lease.phase("waiting_item");
    let _item_reservation = match (&item, owner) {
        (Some(item), Some(_)) if item.id != 0 => Some(
            summons
                .reservation(SummonKey::Item(item.id))
                .lock_owned()
                .await,
        ),
        _ => None,
    };
    lease.phase("active");
    let mut item = match (item, owner) {
        (Some(item), Some(owner)) => {
            let item = owned_summon_item(server, owner, item.id, item.template_id)?;
            if !matches!(server.game_data.items.template(item.template_id).map(|t| &t.kind),
                Some(crate::data::items::TemplateKind::SummonSlave { slave_id }) if *slave_id == template_id)
            {
                return None;
            }
            if let Some(left) = repair_seconds_left(&item, chrono::Utc::now().timestamp()) {
                owner.send(crate::packets::world::SCErrorMsgPacket::new(
                    error::SLAVE_SPAWN_ERROR_NEED_REPAIR_TIME,
                    left as u32,
                    true,
                ));
                return None;
            }
            Some(item)
        }
        (item, _) => item,
    };
    let prior = if let Some(owner) = owner {
        let _publication = summons.publication(server, owner)?;
        if player_world(server, owner)?.id != world.id
            || item
                .as_ref()
                .is_some_and(|i| item_has_other_live_root(server, i.id, owner_id))
        {
            return None;
        }
        let prior = owned_roots(server, owner_id);
        if prior
            .iter()
            .any(|(world, slave)| !can_delete(world, slave, Some(owner), false))
        {
            return None;
        }
        for (_, slave) in &prior {
            save(server, slave);
        }
        prior
    } else {
        Vec::new()
    };
    let mut loaded = if let Some(item) = item.as_ref().filter(|_| owner_id != 0) {
        let mut flush_items: Vec<_> = prior
            .iter()
            .map(|(_, s)| s.data().summoning_item_id)
            .collect();
        flush_items.push(item.id);
        match load_hull(server, world, owner_id, item, template_id, &flush_items).await {
            Ok(loaded) => loaded,
            Err(e) => {
                tracing::error!("SlaveManager.Create({owner_id}, {}): {e}", item.id);
                return None;
            }
        }
    } else {
        LoadedHull::default()
    };
    if loaded.saved.as_ref().is_some_and(|saved| saved.id == 0) {
        tracing::error!("Saved hull has no persistent identity");
        return None;
    }
    for child in &loaded.children {
        if child.id == 0
            || server
                .game_data
                .systems
                .vehicles
                .slave_template(child.template_id)
                .is_none()
        {
            tracing::error!(
                "Saved slave child {} has invalid template {}",
                child.id,
                child.template_id
            );
            return None;
        }
    }
    {
        let _items = server.systems.economy.operation_scope();
        let _publication = match owner {
            Some(owner) => Some(summons.publication(server, owner)?),
            None => None,
        };
        if let (Some(owner), Some(current)) = (owner, &item) {
            // Inventory and session may have changed during the DB await.
            let fresh = owned_summon_item(server, owner, current.id, current.template_id)?;
            if player_world(server, owner)?.id != world.id
                || item_has_other_live_root(server, fresh.id, owner_id)
            {
                return None;
            }
            if repair_seconds_left(&fresh, chrono::Utc::now().timestamp()).is_some() {
                return None;
            }
            item = Some(fresh);
        }
        #[cfg(test)]
        {
            let hook = summons
                .test_after_item_validation
                .lock()
                .expect("test publication hook poisoned")
                .clone();
            if let Some(hook) = hook {
                (hook.0)(server);
            }
        }
        if let Some(owner) = owner {
            let current = owned_roots(server, owner_id);
            if current.len() != prior.len()
                || prior.iter().any(|(world, slave)| {
                    !world
                        .slave(slave.obj_id)
                        .is_some_and(|live| Arc::ptr_eq(&live, slave))
                        || !current.iter().any(|(_, live)| Arc::ptr_eq(live, slave))
                })
            {
                return None;
            }
            if placement.is_some() || searched {
                let template = server
                    .game_data
                    .systems
                    .vehicles
                    .slave_template(template_id)?;
                if placement.is_some() {
                    let Some(transform) = validate_summon_placement(
                        server,
                        world,
                        owner,
                        template,
                        placement,
                        max_range,
                        client_placement,
                    ) else {
                        refund_summon_cooldown(owner, cooldown);
                        return None;
                    };
                    // The cast's full 1.2 placement check ran before the reservations and the
                    // hull load. The owner can have turned since, and the final ship yaw comes
                    // from the current pose, so the 0.5 m centre sphere alone can accept a
                    // turned hull that now lies across a pier. Recheck the transform that will
                    // be spawned exactly as the search path does; refuse before the item is
                    // used if it no longer passes.
                    if crate::systems::ai::collision::files(server).is_some()
                        && !summon_placement_clear_1_2(server, world, owner, template, &transform)
                    {
                        error(owner, error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE);
                        refund_summon_cooldown(owner, cooldown);
                        return None;
                    }
                    position_override = Some(transform);
                } else if let Some(chosen) = position_override {
                    // The search ran before the reservations and the hull load; the player can
                    // have moved meanwhile. Recheck the chosen spot against the current pose.
                    match revalidated_searched_placement(server, world, owner, template, chosen) {
                        Some(transform) => position_override = Some(transform),
                        None => {
                            error(owner, error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE);
                            refund_summon_cooldown(owner, cooldown);
                            return None;
                        }
                    }
                }
            }
            // The 1.2 land height rule for every item summon, after the owner could have moved
            // and before the item commit / old-vehicle deletion. GM / ownerless spawns pass no
            // item and keep the previous behaviour.
            if item.is_some() {
                if let (Some(chosen), Some(template)) = (
                    position_override,
                    server
                        .game_data
                        .systems
                        .vehicles
                        .slave_template(template_id),
                ) {
                    if !template.is_a_boat() {
                        let terrain = summon_terrain(server, world, chosen.x, chosen.y);
                        let owner_z = world.world_pos_rot(&WorldObject::from(owner)).z;
                        if terrain - owner_z > SUMMON_MAX_TERRAIN_ABOVE_OWNER {
                            tracing::warn!(
                                "Slave {} land spawn at ({:.1},{:.1}) refused: terrain {terrain:.1} is more than {SUMMON_MAX_TERRAIN_ABOVE_OWNER} m above the owner ({owner_z:.1})",
                                template.id,
                                chosen.x,
                                chosen.y
                            );
                            error(owner, error::INVALID_SUMMON_LOCATION);
                            refund_summon_cooldown(owner, cooldown);
                            return None;
                        }
                    }
                }
            }
            let prior = owned_roots(server, owner_id);
            if prior
                .iter()
                .any(|(w, s)| !can_delete(w, s, Some(owner), false))
            {
                return None;
            }
        }
        if let Some(saved) = &loaded.saved {
            let live_hull = prior.iter().map(|(_, hull)| hull).find(|hull| {
                let h = hull.data();
                h.id == saved.id
                    && item
                        .as_ref()
                        .is_some_and(|item| h.summoning_item_id == item.id)
            });
            if let Err(error) = loaded
                .doodads
                .validate_slave_cargo(server, saved.id, live_hull)
            {
                tracing::warn!("SlaveManager.Create cargo preflight: {error}");
                return None;
            }
        }
        // Commit the actual item update before altering the prior hull. A missing or moved
        // source (including an unlocked quest mutation) must fail at this boundary.
        let committed = if let (Some(owner), Some(item)) = (owner, &item) {
            let new_id = loaded.saved.is_none();
            let db_id = match &loaded.saved {
                Some(saved) => saved.id,
                None => server.ids.character.get_next_id().ok()?,
            };
            match commit_summon_item(server, owner, item, db_id) {
                Some(committed) => Some(committed),
                None => {
                    if new_id {
                        server.ids.character.release_id(db_id);
                    }
                    return None;
                }
            }
        } else {
            None
        };
        if let Some(owner) = owner {
            for (w, s) in owned_roots(server, owner_id) {
                if !try_delete(server, Some(owner), w.id, s.obj_id, false) {
                    return None;
                }
            }
        }
        let spawned = create_spawn_committed(
            server,
            world,
            owner,
            None,
            template_id,
            item.clone(),
            hide_spawn_effect,
            position_override,
            loaded.saved,
            committed,
        )?;
        if spawned.is_loaded {
            let manager = server.systems.npcs.spawns.get_or_create(world);
            if let Err(error) = manager.publish_persistent_doodads(
                server,
                loaded.doodads,
                Some(&WorldObject::from(&spawned.slave)),
                true,
            ) {
                tracing::error!("Publish persistent slave doodads: {error}");
                return None;
            }
        }
        if !world
            .slave(spawned.slave.obj_id)
            .is_some_and(|live| Arc::ptr_eq(&live, &spawned.slave))
        {
            return None;
        }
        finish_create(
            server,
            world,
            owner,
            &spawned.slave,
            &spawned.template,
            item.as_ref(),
            spawned.is_loaded,
            loaded.children,
        )
    }
}

/// The `SlaveSpawnErrorNeedRepairTime` seconds left on a destroyed vehicle: the C#
/// `(RepairStartTime.AddMinutes(10) - UtcNow)` check of the `SummonSlave` item, and only while
/// the repair time is still in the future. A non-destroyed item is never pending.
pub(super) fn repair_seconds_left(item: &Item, now: i64) -> Option<i64> {
    if item.class != ItemClass::SummonSlave {
        return None;
    }
    let d = &item.details;
    if d.slave_is_destroyed == 0 && d.slave_repair_start_time == 0 {
        return None;
    }
    let left = d.slave_repair_start_time + 600 - now;
    (left > 0).then_some(left)
}

/// The cooldown entries a summon cast started, captured when its job is created. The job can
/// refuse long after the cast (reservations, DB, hull load), so the refund must only clear the
/// GCD/skill cooldown of this cast and leave a newer skill's entries alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct SummonCooldownToken {
    gcd_end_ms: i64,
    skill_id: u32,
    skill_end_ms: Option<i64>,
    tag_id: u32,
    tag_end_ms: Option<i64>,
}

/// Snapshot of the owner's GCD and the summon skill's cooldowns (`None` without a use skill).
fn summon_cooldown_token(
    server: &GameServer,
    owner: &PlayerRef,
    item_template_id: u32,
) -> Option<SummonCooldownToken> {
    let skill_id = server
        .game_data
        .items
        .template(item_template_id)
        .map(|t| t.use_skill_id)
        .filter(|id| *id != 0)?;
    let tag_id = server
        .game_data
        .systems
        .skills
        .skill(skill_id)
        .map_or(0, |s| s.cooldown_tag_id.max(0) as u32);
    let c = owner.data();
    Some(SummonCooldownToken {
        gcd_end_ms: c.unit.global_cooldown_ms,
        skill_id,
        skill_end_ms: c.unit.cooldowns.cooldowns.get(&skill_id).copied(),
        tag_id,
        tag_end_ms: c.unit.cooldowns.tag_cooldowns.get(&tag_id).copied(),
    })
}

/// The 1.2 client refuses before sending, so a server-side refusal must not leave the skill's
/// cooldown or the GCD that the skill system started before the effect ran. Each entry is
/// cleared only while it is still the one captured in `cooldown`: a skill used since then owns
/// its newer GCD/cooldown and must keep it. The cleared entries are also reset on the client
/// with one `SCSkillCooldownResetPacket` (the same packet the skill system uses), so its
/// cooldown icons do not stay greyed out until relog.
fn refund_summon_cooldown(owner: &PlayerRef, cooldown: Option<SummonCooldownToken>) {
    let Some(token) = cooldown else {
        return;
    };
    let (mut skill_cleared, mut tag_cleared, mut gcd_cleared) = (false, false, false);
    {
        let mut c = owner.data();
        if c.unit.global_cooldown_ms == token.gcd_end_ms {
            c.unit.global_cooldown_ms = 0;
            gcd_cleared = token.gcd_end_ms != 0;
        }
        if token.skill_end_ms.is_some()
            && c.unit.cooldowns.cooldowns.get(&token.skill_id).copied() == token.skill_end_ms
        {
            c.unit.cooldowns.remove_cooldown(token.skill_id);
            skill_cleared = true;
        }
        if token.tag_id != 0
            && c.unit.cooldowns.tag_cooldowns.get(&token.tag_id).copied() == token.tag_end_ms
        {
            c.unit.cooldowns.tag_cooldowns.remove(&token.tag_id);
            tag_cleared = true;
        }
    }
    if skill_cleared || tag_cleared || gcd_cleared {
        owner.send(
            crate::systems::skills::packets::SCSkillCooldownResetPacket {
                obj_id: owner.obj_id,
                skill_id: if skill_cleared { token.skill_id } else { 0 },
                tag_id: if tag_cleared { token.tag_id } else { 0 },
                gcd: gcd_cleared,
            },
        );
    }
}
