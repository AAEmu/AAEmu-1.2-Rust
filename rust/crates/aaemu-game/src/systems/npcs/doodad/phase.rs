//! The `Doodad` phase machine (`Models/Game/DoodadObj/Doodad.cs`): the `FuncGroupId` setter,
//! `Use`, `DoFunc`, `DoPhaseFuncs`, `DoChangePhase`, `HasOnlyGroupKindStart`,
//! `IsGroupKindStart`, `GetFuncGroupId`, `OnSkillHit`, `InitDoodad`, `GetOwnerCharacter`,
//! `IsPublicPropertyByAge`, the theft evidence of `Use` and `ZoneManager.DoodadHasMatchingClimate`.
//!
//! Locking: every function takes the doodad's lock only for short reads / writes and never holds
//! it while calling funcs, phase funcs, the world or another doodad.
//!
//! Phase operations reserve one doodad for their duration. Nested calls on the same thread
//! are allowed; timers retry when an interaction is already running.

use rand::Rng;

use super::data::DoodadData;
use super::packets::{
    error_message, error_msg, SCDoodadPhaseChangedPacket, SCDoodadPhaseMsgPacket,
    SCDoodadSoundPacket,
};
use super::runtime::DoodadCtx;
use super::templates::{doodad_func_group_kind, DoodadFunc, FuncTemplate};
use super::{db, funcs, phase_funcs, tasks};
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::server::GameServer;
use crate::world::{now_ms, DoodadRef, PlayerRef, WorldInstance, WorldObject};

/// `Climate.Any`.
const CLIMATE_ANY: i32 = 1;

/// A phase reservation, without holding object data while calling other systems.
pub(crate) struct Operation(DoodadRef);

impl Operation {
    pub(crate) fn enter(doodad: &DoodadRef) -> Option<Self> {
        let thread = std::thread::current().id();
        let mut d = doodad.data();
        if d.phase.deleted || d.phase.operation_thread.is_some_and(|t| t != thread) {
            return None;
        }
        d.phase.operation_thread = Some(thread);
        if d.phase.operation_depth == 0 {
            d.phase.list_group_id.clear();
        }
        d.phase.operation_depth += 1;
        Some(Self(doodad.clone()))
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        let mut d = self.0.data();
        d.phase.operation_depth -= 1;
        if d.phase.operation_depth == 0 {
            d.phase.operation_thread = None;
        }
    }
}

/// The `CurrentFuncs` / `CurrentPhaseFuncs` / `CurrentToDTriggers` / `TimeLeft` caches of the
/// current `FuncGroupId` (the second half of the `FuncGroupId` setter).
pub fn refresh_phase_caches(data: &DoodadData, d: &mut Doodad) {
    let group = d.func_group_id;
    d.current_func_types = data
        .funcs_for_group(group)
        .iter()
        .map(|f| f.func_type.clone())
        .collect();
    // Register new ToD triggers (if any)
    d.phase.tod_triggers.clear();
    let mut final_after = None;
    for pf in data.phase_funcs(group) {
        let template = data.phase_func_template(pf.func_id, &pf.func_type);
        if final_after.is_none() {
            if let Some(FuncTemplate::Final(f)) = template {
                if f.after > 0 {
                    final_after = Some(f.after);
                }
            }
        }
        if pf.func_type != "DoodadFuncTod" {
            continue;
        }
        let Some(FuncTemplate::Tod(tod)) = template else {
            tracing::error!("DoodadFuncTod is not a DoodadFuncTod");
            continue;
        };
        // Dictionary.TryAdd
        if !d.phase.tod_triggers.iter().any(|t| t.0 == tod.tod_as_hours) {
            d.phase
                .tod_triggers
                .push((tod.tod_as_hours, tod.next_phase));
        }
    }
    d.phase.final_after_ms = final_after;
    d.update_final_phase_end();
}

/// The `FuncGroupId` setter: on a change, `PhaseTime = now`, `Save()` when persistent, and the
/// current funcs / phase funcs / ToD trigger caches.
pub fn set_func_group_id(server: &GameServer, doodad: &DoodadRef, value: u32) {
    let (save, previous) = {
        let mut d = doodad.data();
        if value == d.func_group_id {
            return;
        }
        let previous = d.func_group_id;
        d.func_group_id = value;
        d.phase.interaction_counts.clear();
        d.phase_time_ms = now_ms(); // Save PhaseTime at start of new phase (group)
        let persistent = d.is_persistent || d.natural_key.is_some();
        refresh_phase_caches(&server.game_data.systems.npcs.doodad, &mut d);
        (persistent, previous)
    };
    // The phase a doodad leaves no longer owns its area triggers. A duration-0
    // `DoodadFuncClout` (3410 safe zone 720, 4307 clout 992/1008) would otherwise stay until
    // the doodad is deleted, and every warmup would add another copy.
    crate::systems::npcs::area_trigger::remove_doodad_func_group_triggers(server, doodad, previous);
    if save {
        db::save(server, doodad);
    }
    // WP-S3 period phase funcs only look at doodads whose current phase has a SiegePeriod
    // function; keep that index current without polling every world object.
    server
        .systems
        .npcs
        .doodads
        .siege_phase
        .refresh(server, doodad);
    notify_phase_msg(server, doodad, value);
}

/// `SCDoodadPhaseMsg` for a func group whose `phase_msg` is authored (WP-UI1).
///
/// INFERRED trigger (the C# never sent the packet): a real func-group change of a doodad that is
/// already in the world. The spawn-time initial group (`DoodadManager.Create`'s `GetFuncGroupId`
/// and the saved-state restore, `FuncGroupId = phaseId`) is set before the doodad is spawned
/// (`region().is_none()`), so it is not announced. Recipients: every viewer for a normal group
/// (the same broadcast as `SCDoodadPhaseChanged`); every character whose zone key is the
/// doodad's for `is_msg_to_zone`, because the client needs no local doodad in that mode
/// (`FUN_3902e0f0`). INFERRED: "zone" is the doodad's zone key, not the zone group.
fn notify_phase_msg(server: &GameServer, doodad: &DoodadRef, func_group_id: u32) {
    let Some(group) = server
        .game_data
        .systems
        .npcs
        .doodad
        .func_group(func_group_id)
    else {
        return;
    };
    // Empty / whitespace-only messages (the data has `' '` rows) send nothing.
    if group.phase_msg.trim().is_empty() || doodad.region().is_none() {
        return;
    }
    let packet = SCDoodadPhaseMsgPacket {
        doodad_obj_id: doodad.obj_id,
        func_group_id,
    };
    let obj = WorldObject::from(doodad);
    let Some(world) = server.world.world_of(&obj) else {
        return;
    };
    if group.is_msg_to_zone {
        let pos = world.world_pos_rot(&obj);
        let zone_key = world.zone_id_at(pos.x, pos.y);
        server.world.broadcast_to_zone(zone_key, &packet);
    } else {
        crate::world::broadcast(&world, obj, &packet, false);
    }
}

/// The `Data` setter: on a change, `Save()` when persistent (`SetData` is a plain write).
pub fn set_data(server: &GameServer, doodad: &DoodadRef, value: i32) {
    let save = {
        let mut d = doodad.data();
        if value == d.data {
            return;
        }
        d.data = value;
        d.is_persistent || d.natural_key.is_some()
    };
    if save {
        db::save(server, doodad);
    }
}

/// The authored Start group, or first Normal group for world models such as shipwreck 6906
/// whose initial waiting phase is mislabeled Normal in the target 1.2 compact data.
pub fn get_func_group_id(data: &DoodadData, template_id: u32) -> u32 {
    data.doodad_func_groups(template_id)
        .iter()
        .find(|g| g.group_kind_id == doodad_func_group_kind::START)
        .or_else(|| {
            data.doodad_func_groups(template_id)
                .iter()
                .find(|g| g.group_kind_id == doodad_func_group_kind::NORMAL)
        })
        .map_or(0, |g| g.id)
}

/// `IsGroupKindStart(funcGroupId)`.
pub fn is_group_kind_start(ctx: &DoodadCtx<'_>, func_group_id: u32) -> bool {
    let template_id = ctx.doodad.data().template_id;
    ctx.data()
        .doodad_func_groups(template_id)
        .iter()
        .any(|g| g.group_kind_id == doodad_func_group_kind::START && g.id == func_group_id)
}

/// `HasOnlyGroupKindStart()`.
pub fn has_only_group_kind_start(ctx: &DoodadCtx<'_>) -> bool {
    let template_id = ctx.doodad.data().template_id;
    ctx.data().doodad_func_groups(template_id).iter().all(|g| {
        g.group_kind_id != doodad_func_group_kind::NORMAL
            && g.group_kind_id != doodad_func_group_kind::END
    })
}

/// `GetOwnerCharacter()` of any object (`Character` itself, a doodad's owner, the owner of a
/// slave / mate / house; `null` otherwise).
pub fn owner_character(
    server: &GameServer,
    world: &WorldInstance,
    obj: &WorldObject,
) -> Option<PlayerRef> {
    match obj {
        WorldObject::Player(p) => Some(p.clone()),
        WorldObject::Doodad(d) => doodad_owner_character(server, world, d),
        WorldObject::Slave(s) => {
            let owner = s.data().owner_obj_id;
            server.world.character_by_obj_id(owner)
        }
        WorldObject::Mate(m) => {
            let owner = m.data().owner_obj_id;
            server.world.character_by_obj_id(owner)
        }
        WorldObject::House(h) => {
            let owner = h.data().unit.owner_id;
            server.world.character_by_id(owner)
        }
        _ => None,
    }
}

/// `Doodad.GetOwnerCharacter()`.
pub fn doodad_owner_character(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
) -> Option<PlayerRef> {
    let (owner_type, owner_id, owner_obj_id, owner_db_id) = {
        let d = doodad.data();
        (d.owner_type, d.owner_id, d.owner_obj_id, d.owner_db_id)
    };
    match owner_type {
        doodad_owner_type::CHARACTER => server.world.character_by_id(owner_id),
        doodad_owner_type::SLAVE => world
            .slave(owner_obj_id)
            .and_then(|s| owner_character(server, world, &WorldObject::from(&s))),
        doodad_owner_type::HOUSING => {
            crate::systems::npcs::hooks::house_by_db_id(server, owner_db_id)
                .and_then(|h| owner_character(server, world, &WorldObject::from(&h)))
        }
        _ => None,
    }
}

/// `IsPublicPropertyByAge(now)`.
pub fn is_public_property_by_age(ctx: &DoodadCtx<'_>, now_ms: i64) -> bool {
    let (template_id, plant_time) = {
        let d = ctx.doodad.data();
        (d.template_id, d.plant_time)
    };
    let Some(template) = ctx.data().template(template_id) else {
        return false;
    };
    let guard = template.group.guard_on_field_time;
    guard > 0 && (plant_time + i64::from(guard)) * 1000 < now_ms
}

/// `ZoneManager.DoodadHasMatchingClimate(doodad)`.
pub fn doodad_has_matching_climate(ctx: &DoodadCtx<'_>) -> bool {
    let template_id = ctx.doodad.data().template_id;
    let Some(template) = ctx.data().template(template_id) else {
        return false;
    };
    // If no climate defined, then don't give a bonus
    if template.climate_id == CLIMATE_ANY {
        return false;
    }
    let zone_id = {
        let zone_id = ctx.doodad.data().transform.zone_id;
        if zone_id > 0 {
            zone_id
        } else {
            // If ZoneId wasn't set yet, calculate it
            let pos = ctx.world.world_pos_rot(&ctx.obj());
            let zone_id = ctx.world.zone_id_at(pos.x, pos.y);
            ctx.doodad.data().transform.zone_id = zone_id;
            zone_id
        }
    };
    // C#: a missing zone throws (NullReferenceException in GetClimatesByZone); no bonus here.
    let Some(zone) = ctx.server.game_data.zones.zone_by_key(zone_id) else {
        return false;
    };
    ctx.server
        .systems
        .npcs
        .doodads
        .climates_of(zone.zone_climate_id)
        .contains(&template.climate_id)
}

/// The buff id from `blocked_child_doodads` that blocks a player's use of `doodad`, if any: the
/// doodad's parent unit carries an active buff of that template id and its row lists this doodad
/// template. `Transform.parent_obj_id` is the attach parent (`attach_child`); the doodad's own
/// `parent_obj_id` is the fallback for paths that only set the wire field.
pub fn blocking_parent_buff(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
) -> Option<u32> {
    let (template_id, parent_obj_id) = {
        let d = doodad.data();
        let parent = if d.transform.parent_obj_id != 0 {
            d.transform.parent_obj_id
        } else {
            d.parent_obj_id
        };
        (d.template_id, parent)
    };
    if parent_obj_id == 0 {
        return None;
    }
    let parent = world.object(parent_obj_id)?;
    let blocked = &server.game_data.systems.skills.blocked_child_doodads;
    parent
        .with_unit(|u| {
            u.unit()
                .buffs
                .effects
                .iter()
                .map(|b| b.template.id)
                .find(|&buff_id| blocked.blocks(buff_id, template_id))
        })
        .flatten()
}

/// Refuses a blocked `blocked_child_doodads` child doodad use by a player: the existing
/// `NO_INTERACTION_AVAILABLE` error (no new packet) and `Character.SkillCancelled`, so the
/// skills refund the cast's item / labor cost. Returns true when the caller must stop.
pub fn refuse_blocked_child_doodad(
    server: &GameServer,
    world: &WorldInstance,
    doodad: &DoodadRef,
    caster: Option<&WorldObject>,
) -> bool {
    let Some(player) = caster.and_then(WorldObject::as_player) else {
        return false;
    };
    let Some(buff_id) = blocking_parent_buff(server, world, doodad) else {
        return false;
    };
    let (template_id, obj_id) = {
        let d = doodad.data();
        (d.template_id, doodad.obj_id)
    };
    tracing::debug!(
        "Use refused: doodad {obj_id} (template {template_id}) is blocked by buff {buff_id} on its parent"
    );
    player.send(error_msg(error_message::NO_INTERACTION_AVAILABLE));
    funcs::skills_hook(server).cancel_skill(server, player);
    true
}

/// A use that meets another thread's phase reservation is dropped (the C# `Doodad.Use` waited on
/// the doodad lock and then ran). A dropped use of an instance door left the player pressing it
/// with no answer at all, so the player is told to try again.
fn answer_busy_instance_door(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>) {
    let Some(player) = caster.and_then(WorldObject::as_player) else {
        return;
    };
    let group = ctx.doodad.data().func_group_id;
    let is_door = ctx.data().funcs_for_group(group).iter().any(|f| {
        matches!(
            f.func_type.as_str(),
            "DoodadFuncEnterInstance" | "DoodadFuncEnterSysInstance" | "DoodadFuncExitIndun"
        )
    });
    if is_door {
        tracing::debug!(
            "Use refused: instance door {} is busy, {} must try again",
            ctx.doodad.obj_id,
            player.data().unit.name
        );
        player.send(error_msg(
            crate::systems::instances::worlds::error_message::TRY_LATER_INSTANCE,
        ));
    }
}

/// Refuses a func whose `doodad_funcs.perm_id` the player does not satisfy. The 1.2 client
/// filters these rows out before it starts a skill (`FUN_393b3300`, see `permission.rs`), so a
/// client that sends the skill directly must get the client's permission-denial error. The
/// running cast is cancelled so its item / labor cost is refunded (the same shape as
/// [`refuse_blocked_child_doodad`]). Returns true when the caller must stop.
pub(crate) fn refuse_func_permission(
    ctx: &DoodadCtx<'_>,
    player: &PlayerRef,
    func: &DoodadFunc,
) -> bool {
    if super::permission::allows_func(ctx.server, &ctx.world, &ctx.doodad, player, func) {
        return false;
    }
    tracing::debug!(
        "Use refused: doodad {} func {} perm {} denies {}",
        ctx.doodad.obj_id,
        func.func_key,
        func.perm_id,
        player.data().unit.name
    );
    player.send(error_msg(error_message::INTERACTION_PERMISSION_DENY));
    funcs::skills_hook(ctx.server).cancel_skill(ctx.server, player);
    true
}

/// `Doodad.Use(caster, startedSkillId, funcGroupId)`.
pub fn use_doodad(
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
    started_skill_id: u32,
    func_group_id: i32,
) {
    let Some(_operation) = Operation::enter(&ctx.doodad) else {
        answer_busy_instance_door(ctx, caster);
        return;
    };
    let server = ctx.server;
    let mut skill_id = started_skill_id;
    let Some(caster) = caster else {
        return;
    };

    // `blocked_child_doodads` (buff 6064 -> 6560, buff 6245 -> 3446) is a parent-side
    // interaction block, before any func / phase func runs.
    if refuse_blocked_child_doodad(server, &ctx.world, &ctx.doodad, Some(caster)) {
        return;
    }

    // Fix #1443 (defense in depth): refuse interaction with a doodad scheduled for despawn.
    if server
        .systems
        .npcs
        .spawns
        .despawn_field(ctx.doodad.obj_id)
        .is_some()
    {
        if let Some(p) = caster.as_player() {
            tracing::debug!(
                "Use refused: doodad {} is scheduled for despawn, ignoring use from {}",
                ctx.doodad.obj_id,
                p.data().unit.name
            );
            p.send(error_msg(error_message::NO_INTERACTION_AVAILABLE));
        }
        return;
    }

    let player = caster.as_player();

    // Siege larceny (ui 3334): crops, livestock and trees on private land are usable while the
    // territory's lodestone is held. This is the shared `Doodad.Use` funnel, so the skill-bound
    // `DoodadFuncUse` path and the skill-less `LootItem` / `LootPack` / `Cutdowning` path both
    // pass it. Only the farm groups are gated here; houses, furniture, doors and coffers keep
    // their own permission checks. The window grants produce only, never the uproot.
    if let Some(player) = player {
        if !crate::systems::housing::dominion::larceny::house_doodad_use_allowed(
            server,
            &ctx.doodad,
            player,
            started_skill_id,
        ) {
            player.send(error_msg(error_message::INTERACTION_PERMISSION_DENY));
            return;
        }
    }

    if func_group_id > 0 {
        if !crate::systems::instances::mentoring::allows_phase_request(ctx, func_group_id) {
            return;
        }
        set_func_group_id(server, &ctx.doodad, func_group_id as u32);
    }

    let mut chained = false;
    loop {
        let (template_id, current) = {
            let mut d = ctx.doodad.data();
            d.phase.to_next_phase = false;
            d.phase.loot_missed = false;
            d.phase.list_group_id.clear();
            (d.template_id, d.func_group_id)
        };
        tracing::trace!(
            "Use: TemplateId {template_id}, Using phase {current} with SkillId {skill_id}"
        );

        // first we find the functions, then we execute
        let func_with_skill = ctx.data().func_for_skill(current, skill_id).cloned();
        let all_funcs = ctx.data().funcs_for_group(current);
        if all_funcs.is_empty() {
            return; // Phase has no funcs
        }

        if skill_id == 0 {
            for f in all_funcs.iter().filter(|f| {
                matches!(
                    f.func_type.as_str(),
                    "DoodadFuncLootItem" | "DoodadFuncLootPack" | "DoodadFuncCutdowning"
                )
            }) {
                // The client builds this list with the func's `perm` filter (`FUN_393bd290`);
                // a direct `CSLootOpenBag` must not run a row the client would have hidden.
                if let Some(player) = player {
                    if chained {
                        // A chained iteration runs after an earlier loot row already ran and
                        // advanced the phase. The interaction (and its loot / cast) already
                        // happened, so a denied bonus row is skipped silently instead of
                        // sending 232 and refunding a cast whose effect is done.
                        if !super::permission::allows_func(
                            ctx.server,
                            &ctx.world,
                            &ctx.doodad,
                            player,
                            f,
                        ) {
                            continue;
                        }
                    } else if refuse_func_permission(ctx, player, f) {
                        return;
                    }
                }
                if do_func(ctx, Some(caster), started_skill_id, Some(f)) {
                    ctx.doodad.data().phase.list_group_id.clear();
                    return;
                }
            }
            // A skill-bound func (e.g. the "extend the life of dying plants" Use) leads to a
            // bonus-loot group that has no interaction of its own. When every roll there
            // missed, the doodad would sit in that group until its timer fires; the bonus
            // is optional, so continue to the group's authored next phase instead.
            if chained
                && ctx.doodad.data().phase.loot_missed
                && ctx.doodad.data().func_group_id == current
            {
                let next = all_funcs
                    .iter()
                    .find(|f| f.func_type == "DoodadFuncLootItem" && f.next_phase > 0)
                    .map(|f| f.next_phase as u32);
                if let Some(next) = next {
                    set_func_group_id(server, &ctx.doodad, next);
                }
            }
        } else {
            // `FUN_393b3300` filters a skill interaction by the matched func's `perm` before
            // the client starts it; a client that sends `CSStartSkill` itself bypasses that,
            // so the same rule is enforced here, before the func runs or changes the phase.
            if let (Some(player), Some(func)) = (player, func_with_skill.as_ref()) {
                if refuse_func_permission(ctx, player, func) {
                    return;
                }
            }
            if do_func(
                ctx,
                Some(caster),
                started_skill_id,
                func_with_skill.as_ref(),
            ) {
                // FuncGroupId is the current phase, func.NextPhase or OverridePhase
                let fg = ctx.doodad.data().func_group_id;
                if fg != current {
                    do_change_phase(ctx, Some(caster), fg as i32);
                }
                return;
            }
        }

        theft_evidence(ctx, caster, started_skill_id);

        // then execute the phase functions (the FuncGroupId may have changed)
        let fg = ctx.doodad.data().func_group_id;
        if fg == current && !ctx.doodad.data().phase.to_next_phase {
            return;
        }
        let stop = do_change_phase(ctx, Some(caster), fg as i32);
        if stop || !ctx.doodad.data().phase.to_next_phase {
            tracing::trace!("Use: Did not pass the conditions check! TemplateId {template_id}");
            return;
        }
        skill_id = 0;
        chained = true;
    }
}

/// The theft-evidence part of `Use` (`GenerateTheftEvidence` via the skills hook).
fn theft_evidence(ctx: &DoodadCtx<'_>, caster: &WorldObject, started_skill_id: u32) {
    let server = ctx.server;
    let caster_owner = owner_character(server, &ctx.world, caster);
    let (owner_type, owner_id) = {
        let d = ctx.doodad.data();
        (d.owner_type, d.owner_id)
    };
    // A garden plant is owned by its house, not by a character. Its use is theft only when the
    // siege larceny window (not a normal house permission such as PUBLIC) granted the access;
    // the normal evidence then follows `Housing.SiegeLarcenyCrime`.
    let larceny_house =
        if crate::systems::housing::dominion::larceny::covers_doodad(server, &ctx.doodad) {
            crate::systems::housing::dominion::larceny::house_of_doodad(server, &ctx.doodad)
        } else {
            None
        };
    let larceny_granted = match (&larceny_house, caster_owner.as_ref()) {
        (Some(house), Some(player)) => {
            !crate::systems::housing::allowed_to_interact(server, house, player)
        }
        _ => false,
    };
    let owner_character_id = match owner_type {
        doodad_owner_type::CHARACTER => Some(owner_id),
        doodad_owner_type::HOUSING if larceny_granted => larceny_house
            .as_ref()
            .map(|house| house.data().unit.owner_id)
            .filter(|id| *id != 0),
        _ => None,
    };
    let caster_owner_id = caster_owner.as_ref().map(|p| p.data().id);
    let is_different_character_owner =
        owner_character_id.is_some_and(|id| Some(id) != caster_owner_id);
    if !is_different_character_owner {
        return;
    }
    // The window's text says the crops are "used", not stolen; keep the footprint evidence
    // only when the configuration asks for the normal theft rule.
    if larceny_granted
        && crate::systems::housing::dominion::larceny::suppresses_theft_evidence(
            server,
            &ctx.doodad,
        )
    {
        return;
    }
    let hooks = server.systems.npcs.hooks.skills.get();
    let is_explicit_crime_skill = started_skill_id != 0
        && hooks
            .as_ref()
            .is_some_and(|h| h.skill_crime_point(server, started_skill_id) > 0);
    let is_skill_less_pickup = started_skill_id == 0;
    if (is_skill_less_pickup || is_explicit_crime_skill)
        && !is_public_property_by_age(ctx, now_ms())
    {
        // C#: GenerateTheftEvidence(casterOwningCharacter) with a possibly null criminal.
        if let (Some(h), Some(criminal)) = (hooks, caster_owner.as_ref()) {
            h.generate_theft_evidence(server, criminal, &ctx.doodad);
        }
    }
}

/// `DoFunc(caster, skillId, func)`: `true` stops the `Use` loop (wait for interaction).
pub fn do_func(
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
    skill_id: u32,
    func: Option<&DoodadFunc>,
) -> bool {
    if ctx.doodad.data().phase.deleted {
        return true;
    }
    let Some(func) = func else {
        tracing::trace!("DoFunc: Finished execution with func = null");
        return true;
    };

    if !super::gathering::allows_func(ctx.server, func, skill_id, caster) {
        return true;
    }

    // then perform the function (DoodadFunc.Use -> template?.Use)
    if let Some(t) = ctx.data().func_template(func.func_id, &func.func_type) {
        if !crate::systems::instances::mentoring::authorize(ctx, caster, skill_id, t) {
            return true;
        }
        funcs::use_func(t, ctx, caster, skill_id, func.next_phase);
    }
    if func.sound_id > 0 {
        ctx.broadcast(&SCDoodadSoundPacket {
            obj_id: ctx.doodad.obj_id,
            sound_id: func.sound_id,
        });
    }

    let (to_next_phase, override_phase) = {
        let d = ctx.doodad.data();
        (d.phase.to_next_phase, d.phase.override_phase)
    };
    if !to_next_phase {
        tracing::trace!("DoFunc Finished execution without ToNextPhase");
        // DoodadFuncLootItem sets ToNextPhase=false when loot fails; let Use() try the next one.
        return func.func_type != "DoodadFuncLootItem";
    }
    crate::systems::instances::mentoring::succeeded(ctx, caster);
    if func.count > 1 {
        let complete = {
            let mut d = ctx.doodad.data();
            let count = d.phase.interaction_counts.entry(func.func_key).or_default();
            *count += 1;
            *count >= func.count as u32
        };
        if !complete {
            ctx.doodad.data().phase.to_next_phase = false;
            db::save(ctx.server, &ctx.doodad);
            return true;
        }
    }
    if func.next_phase == -1 {
        // We stay in the current phase; the check is needed for Windstone id=1473
        if !has_only_group_kind_start(ctx) {
            if tasks::cancel_func_task(&ctx.doodad) {
                tracing::debug!("DoFunc: The current timer has been canceled.");
            }
            despawn_or_delete(ctx);
        }
        return true;
    }
    if override_phase > 0 {
        // occurs in DoodadFuncConditionalUse
        set_func_group_id(ctx.server, &ctx.doodad, override_phase as u32);
        ctx.doodad.data().phase.override_phase = 0;
    } else {
        set_func_group_id(ctx.server, &ctx.doodad, func.next_phase as u32);
    }
    false
}

/// `Spawner is not null ? Spawner.Despawn(this) : Delete()`.
pub fn despawn_or_delete(ctx: &DoodadCtx<'_>) {
    match ctx
        .server
        .systems
        .npcs
        .spawns
        .doodad_spawner_of(ctx.doodad.obj_id)
    {
        Some(spawner) => spawner.despawn(ctx.server, &ctx.doodad),
        None => super::manager::delete(ctx.server, &ctx.world, &ctx.doodad),
    }
}

/// The result of an externally driven phase change (a WP-S3 period boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalPhaseChange {
    /// The doodad was in `expected_group` and the change ran.
    Applied,
    /// The doodad is deleted or no longer in `expected_group`: nothing to do.
    Skipped,
    /// Another thread holds the phase-operation reservation: retry later.
    Busy,
}

/// An externally driven phase change (a period boundary, WP-S3): only while the doodad is
/// still in `expected_group`, checked and applied under one phase-operation reservation.
pub(crate) fn do_change_phase_if_group(
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
    expected_group: u32,
    next_phase: i32,
) -> ExternalPhaseChange {
    let Some(_operation) = Operation::enter(&ctx.doodad) else {
        return ExternalPhaseChange::Busy;
    };
    {
        let d = ctx.doodad.data();
        if d.phase.deleted || d.func_group_id != expected_group {
            return ExternalPhaseChange::Skipped;
        }
    }
    // A nested `Operation::enter` on this thread is allowed.
    do_change_phase(ctx, caster, next_phase);
    ExternalPhaseChange::Applied
}

/// [`despawn_or_delete`] for an externally driven final phase, under the same expected-group
/// check.
pub(crate) fn despawn_or_delete_if_group(
    ctx: &DoodadCtx<'_>,
    expected_group: u32,
) -> ExternalPhaseChange {
    let Some(_operation) = Operation::enter(&ctx.doodad) else {
        return ExternalPhaseChange::Busy;
    };
    {
        let d = ctx.doodad.data();
        if d.phase.deleted || d.func_group_id != expected_group {
            return ExternalPhaseChange::Skipped;
        }
    }
    despawn_or_delete(ctx);
    ExternalPhaseChange::Applied
}

/// `DoPhaseFuncs(caster, ref nextPhase)`: `true` = the phase did not pass (abort).
fn do_phase_funcs(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>, next_phase: &mut i32) -> bool {
    if *next_phase <= 0 {
        return true;
    }
    // Changing the phase.
    set_func_group_id(ctx.server, &ctx.doodad, *next_phase as u32);
    let group = *next_phase as u32;
    let recursed = {
        let mut d = ctx.doodad.data();
        if d.phase.list_group_id.contains(&group) {
            d.phase.list_group_id.clear();
            true
        } else {
            d.phase.list_group_id.push(group); // to check CheckPhase()
            false
        }
    };
    if recursed {
        // e.g. ID=2231, Target: break the recursion (ID=898, Prison Gate has no funcs: continue)
        tracing::trace!("DoPhase: Finished execution with recurse: phase {group}");
        return true;
    }

    if tasks::cancel_func_task(&ctx.doodad) {
        tracing::trace!("DoPhaseFuncs:DoodadFuncTimer: The current timer has been canceled.");
    }
    tracing::trace!(
        "DoPhaseFuncs: ObjId {}, nextPhase {}",
        ctx.doodad.obj_id,
        *next_phase
    );

    let phase_funcs = ctx.data().phase_funcs(group);
    if phase_funcs.is_empty() {
        return false; // no phase functions for FuncGroupId
    }

    let mut stop = false;
    let manager = &ctx.server.systems.npcs.doodads;
    for pf in phase_funcs {
        let ratio = manager
            .forced_phase_ratio()
            .unwrap_or_else(|| rand::thread_rng().gen_range(0..10000));
        ctx.doodad.data().phase.phase_ratio = ratio;
        // DoodadPhaseFunc.Use: template != null && template.Use(caster, owner)
        stop = match ctx.data().phase_func_template(pf.func_id, &pf.func_type) {
            Some(t) => phase_funcs::use_phase_func(t, ctx, caster),
            None => false,
        };
        if stop {
            break; // interrupt execution of phase functions and switch to OverridePhase
        }
    }

    let (override_phase, func_group_id, deleted) = {
        let d = ctx.doodad.data();
        (d.phase.override_phase, d.func_group_id, d.phase.deleted)
    };
    if override_phase != 0 && stop && func_group_id as i32 != override_phase {
        *next_phase = override_phase;
        ctx.doodad.data().phase.override_phase = 0;
        return do_phase_funcs(ctx, caster, next_phase);
    }
    if !deleted {
        db::save(ctx.server, &ctx.doodad); // let's save the doodad in the database
    }
    stop
}

/// `DoChangePhase(caster, nextPhase)`: phase funcs of `next_phase`, then
/// `SCDoodadPhaseChangedPacket` (unless deleted). `true` = aborted.
pub fn do_change_phase(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>, next_phase: i32) -> bool {
    let Some(_operation) = Operation::enter(&ctx.doodad) else {
        return true;
    };
    if next_phase <= 0 {
        return false;
    }
    tracing::trace!(
        "DoChangePhase: ObjId {}, nextPhase {next_phase}",
        ctx.doodad.obj_id
    );
    let mut np = next_phase;
    let stop = do_phase_funcs(ctx, caster, &mut np);
    // the phase change packet must be after the phase functions (correct FuncGroupId)
    let packet = {
        let d = ctx.doodad.data();
        (!d.phase.deleted).then(|| SCDoodadPhaseChangedPacket::new(ctx.doodad.obj_id, &d, now_ms()))
    };
    if let Some(p) = packet {
        ctx.broadcast(&p);
    }
    stop
}

/// `OnSkillHit(caster, skillId)`: `Use(caster, skillId)` once per `DoodadFuncSkillHit` of the
/// current phase.
pub fn on_skill_hit(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>, skill_id: u32) {
    let group = ctx.doodad.data().func_group_id;
    let n = ctx
        .data()
        .funcs_for_group(group)
        .iter()
        .filter(|f| f.func_type == "DoodadFuncSkillHit")
        .count();
    for _ in 0..n {
        use_doodad(ctx, caster, skill_id, 0);
    }
}

/// `InitDoodad()`: growth time (growth rate + climate bonus), then `DoChangePhase(owner unit,
/// FuncGroupId)`.
pub fn init_doodad(ctx: &DoodadCtx<'_>) {
    let owner_obj_id = ctx.doodad.data().owner_obj_id;
    let unit = ctx.world.unit(owner_obj_id);
    init_doodad_for(ctx, unit.as_ref());
}

/// [`init_doodad`] with its owner unit (`ParentWorld.GetUnit(OwnerObjId)`) already resolved: an
/// init that runs later than the C# `InitDoodad()` call it stands for (a delayed `SpawnDoodad`)
/// passes the unit of that call, which may have left the world since.
pub fn init_doodad_for(ctx: &DoodadCtx<'_>, unit: Option<&WorldObject>) {
    let (template_id, plant_time) = {
        let d = ctx.doodad.data();
        (d.template_id, d.plant_time)
    };
    let total = ctx
        .data()
        .template(template_id)
        .map_or(0, |t| t.total_doodad_growth_time);
    // Apply Climate settings
    let mut grow_time = f64::from(total) / ctx.server.systems.npcs.doodads.growth_rate();
    if total > 0 && doodad_has_matching_climate(ctx) {
        grow_time = (grow_time * f64::from(0.73f32)).round_ties_even() as i32 as f64;
    }
    ctx.doodad.data().growth_time_ms = plant_time * 1000 + grow_time as i64;

    // Actually do the phase change
    let fg = ctx.doodad.data().func_group_id;
    do_change_phase(ctx, unit, fg as i32);
    // A ratio-respawn replacement's Tod catch-up suppression only covers its entry phase.
    ctx.doodad.data().phase.suppress_tod_catch_up = false;
}

#[cfg(test)]
#[path = "phase_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "guard_time_tests.rs"]
mod guard_time_tests;

#[cfg(test)]
#[path = "blocked_child_tests.rs"]
mod blocked_child_tests;
