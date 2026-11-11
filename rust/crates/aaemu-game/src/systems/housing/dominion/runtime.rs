//! World-side castle lifecycle. Ownership is expedition state, never ordinary house ownership.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::server::GameServer;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::plots::PlotState;
use crate::systems::skills::state::SkillRef;
use crate::systems::skills::types::{CastAction, SkillCaster};
use crate::world::{HouseRef, WorldObject};

use crate::systems::housing::config::HousingConfig;

use super::calendar::{
    CalendarSettings, CalendarTimeZone, SiegeCalendar, PERIOD_SIEGE, PERIOD_WARMUP,
};
use super::state::{DominionState, EngravingHandle, SiegeStatus, TowerIdentity};

pub(crate) fn is_live_object(server: &GameServer, object: &WorldObject) -> bool {
    server
        .world
        .world(object.transform().instance_id)
        .and_then(|world| world.object(object.obj_id()))
        // ObjectCore is embedded in every retained Arc allocation. Numeric object IDs can
        // be reused; WorldObject::same compares only ID/kind and cannot validate that handle.
        .is_some_and(|live| {
            live.kind() == object.kind() && std::ptr::eq(live.core(), object.core())
        })
}

fn is_retained_house(server: &GameServer, house: &HouseRef) -> bool {
    let id = house.data().id;
    is_live_object(server, &WorldObject::from(house))
        && server
            .systems
            .housing
            .manager
            .by_id(id)
            .is_some_and(|current| Arc::ptr_eq(&current, house))
}

pub const ENGRAVING_SKILL: u32 = 20_866;
pub const ENGRAVING_PLOT: u32 = 681;
pub const ENGRAVABLE_BUFF: u32 = 5131;
pub const ENGRAVING_BUFF: u32 = 5074;

#[derive(Debug, Clone)]
struct ActiveEngraving {
    handle: EngravingHandle,
    skill: SkillRef,
    generation: u64,
    caster: WorldObject,
    tower: HouseRef,
    origin: crate::models::transform::PositionAndRotation,
}

#[derive(Debug, Default)]
pub struct EngravingRuntime {
    active: Mutex<HashMap<u32, ActiveEngraving>>,
}

pub fn tower_identity(server: &GameServer, house: &HouseRef) -> Option<(u32, TowerIdentity)> {
    let (template_id, house_id, transform) = {
        let h = house.data();
        (h.unit.template_id, h.id, h.unit.transform)
    };
    let setting = server
        .game_data
        .systems
        .housing
        .template(template_id)?
        .guard_tower_setting_id;
    if setting == 0 || house_id == 0 {
        return None;
    }
    let zone = server
        .game_data
        .zones
        .zone_by_key(transform.zone_id)?
        .group_id;
    Some((
        zone,
        TowerIdentity {
            house_id,
            instance_id: transform.instance_id,
            guard_tower_setting_id: setting,
        },
    ))
}

fn same_world_and_range(server: &GameServer, caster: &WorldObject, tower: &HouseRef) -> bool {
    let target = WorldObject::from(tower);
    let Some(world) = server.world.world_of(caster) else {
        return false;
    };
    if caster.transform().instance_id != target.transform().instance_id
        || !is_live_object(server, caster)
        || !is_live_object(server, &target)
    {
        return false;
    }
    let c = world.world_pos_rot(caster);
    let t = world.world_pos_rot(&target);
    let distance2 = (c.x - t.x).powi(2) + (c.y - t.y).powi(2) + (c.z - t.z).powi(2);
    distance2.is_finite() && distance2 <= 30.0 * 30.0
}

/// Reject unauthorized engraving before the stock plot applies visual/channel buffs.
pub fn check_skill(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    target: &Target,
) -> bool {
    if let Some(house) = target.as_obj().and_then(WorldObject::as_house) {
        if super::fort::is_fort(server, house)
            && crate::systems::housing::manager::template_of(server, house).is_some_and(
                |template| {
                    template
                        .build_steps
                        .values()
                        .any(|step| step.skill_id == skill.template.id)
                },
            )
        {
            return caster.as_player().is_some_and(|player| {
                let current_step = house.data().current_step;
                let matches_step = crate::systems::housing::manager::template_of(server, house)
                    .and_then(|template| template.build_steps.get(&current_step))
                    .is_some_and(|step| step.skill_id == skill.template.id);
                let alive = {
                    let c = player.data();
                    c.unit.hp > 0 && c.is_online
                };
                alive
                    && matches_step
                    && is_live_object(server, caster)
                    && is_live_object(server, &WorldObject::from(house))
                    && super::fort::allowed_to_interact(server, house, player)
            });
        }
    }
    if skill.template.id != ENGRAVING_SKILL {
        return true;
    }
    let Some(canonical) = server.game_data.systems.skills.skill(ENGRAVING_SKILL) else {
        return false;
    };
    if !Arc::ptr_eq(canonical, &skill.template) || skill.template.plot_id != Some(ENGRAVING_PLOT) {
        return false;
    }
    let (Some(player), Some(tower)) = (
        caster.as_player(),
        target.as_obj().and_then(WorldObject::as_house),
    ) else {
        return false;
    };
    let (expedition, alive) = {
        let c = player.data();
        (c.unit.expedition_id, c.unit.hp > 0 && c.is_online)
    };
    let Some((zone, identity)) = tower_identity(server, tower) else {
        return false;
    };
    let buffs = {
        let h = tower.data();
        h.current_step == -1
            && h.unit.buffs.check_buff(ENGRAVABLE_BUFF)
            && !h.unit.buffs.check_buff(ENGRAVING_BUFF)
    };
    alive
        && expedition != 0
        && server
            .expeditions
            .expedition_of_character(player.character_id)
            == expedition
        && buffs
        && same_world_and_range(server, caster, tower)
        && server.systems.housing.dominions.allow_engraving(
            zone,
            identity,
            player.character_id,
            expedition,
            crate::world::now_ms(),
        )
}

/// Called after the accepted plot is attached to its caster and SkillRun.
pub fn begin_plot(server: &GameServer, plot: &Arc<PlotState>) -> bool {
    if plot.skill.template.id != ENGRAVING_SKILL {
        return true;
    }
    if !check_skill(server, &plot.skill, &plot.ctx.caster, &plot.ctx.target) {
        return false;
    }
    let Some(player) = plot.ctx.caster.as_player() else {
        return false;
    };
    let Some(tower) = plot.ctx.target.as_obj().and_then(WorldObject::as_house) else {
        return false;
    };
    let Some((zone, identity)) = tower_identity(server, tower) else {
        return false;
    };
    let expedition = player.data().unit.expedition_id;
    let Ok(handle) = server.systems.housing.dominions.begin_engraving(
        zone,
        identity,
        player.character_id,
        expedition,
        crate::world::now_ms(),
    ) else {
        return false;
    };
    let origin = server
        .world
        .world_of(&plot.ctx.caster)
        .map(|w| w.world_pos_rot(&plot.ctx.caster))
        .unwrap_or_default();
    let generation = plot.skill.state().generation;
    server
        .systems
        .housing
        .engraving_runtime
        .active
        .lock()
        .expect("engraving poisoned")
        .insert(
            player.character_id,
            ActiveEngraving {
                handle,
                skill: plot.skill.clone(),
                generation,
                caster: plot.ctx.caster.clone(),
                tower: tower.clone(),
                origin,
            },
        );
    if let Some(state) = server.systems.housing.dominions.get(zone) {
        crate::systems::social::dominion::send_tower_alert(
            server,
            &state,
            crate::systems::social::dominion::tower_alert::ENGRAVING_STARTED,
            Some(player),
        );
    }
    true
}

fn valid_channel(server: &GameServer, channel: &ActiveEngraving) -> bool {
    let state = channel.skill.state();
    let valid_plot = state.active_plot.as_ref().is_some_and(|plot| {
        !plot.cancellation_requested()
            && plot.ctx.caster.same(&channel.caster)
            && plot
                .ctx
                .target
                .as_obj()
                .is_some_and(|t| t.same(&WorldObject::from(&channel.tower)))
    });
    let active = !state.cancelled && state.generation == channel.generation && valid_plot;
    drop(state);
    let caster_alive = channel.caster.as_player().is_some_and(|p| {
        let c = p.data();
        c.unit.hp > 0 && c.is_online
    });
    let current_plot = channel
        .caster
        .with_unit(|u| u.unit().skill_state.active_plot.clone())
        .flatten();
    let exact_plot = current_plot
        .is_some_and(|p| Arc::ptr_eq(&p.0.skill, &channel.skill) && !p.0.cancellation_requested());
    let unmoved = server.world.world_of(&channel.caster).is_some_and(|w| {
        let p = w.world_pos_rot(&channel.caster);
        // Position changes interrupt a stationary five-minute engraving, including teleport.
        (p.x - channel.origin.x).abs() <= 0.05
            && (p.y - channel.origin.y).abs() <= 0.05
            && (p.z - channel.origin.z).abs() <= 0.05
    });
    let registered = channel.caster.as_player().is_some_and(|p| {
        let expedition = server.expeditions.expedition_of_character(p.character_id);
        server.systems.housing.dominions.registered_attacker(
            channel.handle.zone_group_id,
            channel.handle.tower,
            p.character_id,
            expedition,
            chrono::Utc::now().timestamp(),
        )
    });
    active
        && registered
        && exact_plot
        && caster_alive
        && unmoved
        && same_world_and_range(server, &channel.caster, &channel.tower)
}

/// Only the retained castle plot may remove its own channel buffs. A stale cancelled
/// plot must not clear the next engraver's reservation/visuals, including the same character.
pub fn cleanup_plot_buff(
    server: &GameServer,
    plot: &PlotState,
    unit: &WorldObject,
    buff_id: u32,
) -> bool {
    if plot.skill.template.id != ENGRAVING_SKILL {
        return false;
    }
    let Some(player) = plot.ctx.caster.as_player() else {
        return true;
    };
    let replaced = server
        .systems
        .housing
        .engraving_runtime
        .active
        .lock()
        .expect("engraving poisoned")
        .get(&player.character_id)
        .is_some_and(|channel| !Arc::ptr_eq(&channel.skill, &plot.skill));
    if replaced {
        return true;
    }
    unit.with_base_unit(|base| {
        let indices: Vec<_> = base
            .buffs()
            .effects
            .iter()
            .filter(|buff| {
                buff.template.id == buff_id
                    && buff.skill_id == ENGRAVING_SKILL
                    && buff.caster_obj_id == player.obj_id
            })
            .map(|buff| buff.index)
            .collect();
        for index in indices {
            base.buffs_mut().remove_effect_by_index(index);
        }
    });
    crate::systems::dispatch_buff_events(server, unit);
    true
}

pub fn abort_character(server: &GameServer, character_id: u32) {
    let channel = server
        .systems
        .housing
        .engraving_runtime
        .active
        .lock()
        .expect("engraving poisoned")
        .remove(&character_id);
    if let Some(channel) = channel {
        channel.skill.state().cancelled = true;
        let plot = { channel.skill.state().active_plot.clone() };
        if let Some(plot) = plot {
            plot.request_cancellation();
            cleanup_plot_buff(
                server,
                &plot,
                &WorldObject::from(&channel.tower),
                ENGRAVING_BUFF,
            );
        }
        if let Some(state) = server
            .systems
            .housing
            .dominions
            .get(channel.handle.zone_group_id)
        {
            crate::systems::social::dominion::send_tower_alert(
                server,
                &state,
                crate::systems::social::dominion::tower_alert::ENGRAVING_STOPPED,
                channel.caster.as_player(),
            );
        }
    }
    // Release authority only after old buff cleanup, so a new cast cannot lose its buff.
    server
        .systems
        .housing
        .dominions
        .abort_character(character_id);
}

/// Special85 must come from this accepted, still-active original plot and retained handles.
pub fn complete_effect(server: &GameServer, args: &EffectArgs<'_>) {
    let (Some(player), Some(source), Some(tower)) = (
        args.caster.as_ref().and_then(WorldObject::as_player),
        args.source.skill.as_ref(),
        args.target.as_obj().and_then(WorldObject::as_house),
    ) else {
        return;
    };
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    let channel = server
        .systems
        .housing
        .engraving_runtime
        .active
        .lock()
        .expect("engraving poisoned")
        .get(&player.character_id)
        .cloned();
    let Some(channel) = channel else { return };
    let eligible = tower.data().unit.buffs.check_buff(ENGRAVABLE_BUFF);
    let cast_matches = matches!(args.cast, CastAction::Plot { plot_id: ENGRAVING_PLOT, event_id: 5552, skill_id: ENGRAVING_SKILL, tl_id } if tl_id == source.tl_id());
    if !eligible
        || !cast_matches
        || args.source.is_trigger
        || args.source.buff.is_some()
        || !Arc::ptr_eq(source, &channel.skill)
        || !Arc::ptr_eq(tower, &channel.tower)
        || !matches!(args.target_caster, crate::systems::skills::types::SkillCastTarget::Unit(id) if id == tower.obj_id)
        || !matches!(args.caster_caster, SkillCaster::Unit(id) if id == player.obj_id)
        || !valid_channel(server, &channel)
    {
        return;
    }
    let expedition = server
        .expeditions
        .expedition_of_character(player.character_id);
    let previous = server
        .systems
        .housing
        .dominions
        .get(channel.handle.zone_group_id);
    let Ok(state) = server.systems.housing.dominions.complete_engraving(
        &channel.handle,
        expedition,
        crate::world::now_ms(),
    ) else {
        return;
    };
    server
        .systems
        .housing
        .engraving_runtime
        .active
        .lock()
        .expect("engraving poisoned")
        .remove(&player.character_id);
    apply_tower_state(server, &state);
    if let Some(previous) = previous {
        crate::systems::social::dominion::clear_national_monument_on_owner_change(
            server, &previous, &state,
        );
    }
    crate::systems::social::dominion::broadcast_owner_change(server, &state);
    crate::systems::social::dominion::send_tower_alert(
        server,
        &state,
        crate::systems::social::dominion::tower_alert::ENGRAVING_SUCCEEDED,
        Some(player),
    );
    // The attackers captured the territory: credit kinds 57 (attackers) and 60 (defenders)
    // once. This function holds only the reentrant operation scope, not the economy ops lock,
    // so the facade writes the records immediately.
    super::credit::credit_siege_outcome(server, &state);
}

/// Tower damage is an explicit participant action while the scheduled siege is active.
pub fn can_attack_tower(server: &GameServer, caster: &Target, target: &Target) -> Option<bool> {
    let house = target.as_obj()?.as_house()?;
    let (zone, identity) = if let Some(tower) = tower_identity(server, house) {
        tower
    } else if super::fort::is_fort(server, house) {
        let Some(state) = super::fort::state_for_house(server, house) else {
            return Some(false);
        };
        (state.zone_group_id, state.tower)
    } else {
        return None;
    };
    if !is_retained_house(server, house) {
        return Some(false);
    }
    let allowed = caster
        .as_obj()
        .and_then(|c| crate::systems::skills::relation::owner_character(server, c))
        .is_some_and(|p| {
            let expedition = p.data().unit.expedition_id;
            same_world_and_range_for_damage(server, caster, house)
                && server
                    .systems
                    .housing
                    .dominions
                    .get(zone)
                    .is_some_and(|state| state.tower == identity)
                && server.systems.housing.dominions.registered_attacker(
                    zone,
                    identity,
                    p.character_id,
                    expedition,
                    chrono::Utc::now().timestamp(),
                )
        });
    Some(allowed)
}

fn same_world_and_range_for_damage(server: &GameServer, caster: &Target, tower: &HouseRef) -> bool {
    caster.as_obj().is_some_and(|c| {
        is_live_object(server, c)
            && c.transform().instance_id == tower.data().unit.transform.instance_id
    })
}

/// Guard towers remain world objects after destruction so the original engraving plot can target them.
pub fn on_hp_changed(
    server: &GameServer,
    target: &WorldObject,
    attacker: Option<&WorldObject>,
    old_hp: i32,
    hp: i32,
) {
    let Some(house) = target.as_house() else {
        return;
    };
    let Some((zone, identity)) = tower_identity(server, house) else {
        return;
    };
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    if !is_retained_house(server, house) {
        return;
    }
    let Ok(state) = server
        .systems
        .housing
        .dominions
        .update_tower_hp(zone, identity, hp)
    else {
        return;
    };
    if hp >= old_hp {
        return;
    }
    let actor =
        attacker.and_then(|actor| crate::systems::skills::relation::owner_character(server, actor));
    let max_hp = house.data().unit.max_hp;
    use crate::systems::social::dominion::{send_tower_alert, tower_alert};
    if old_hp == max_hp {
        send_tower_alert(server, &state, tower_alert::FIRST_ATTACKED, actor.as_ref());
    }
    if hp > 0 {
        let previous = crate::systems::ai::collision::objects::house_model_state(old_hp, max_hp);
        let next = crate::systems::ai::collision::objects::house_model_state(hp, max_hp);
        if previous == 1 && next >= 2 {
            send_tower_alert(
                server,
                &state,
                tower_alert::BELOW_75_PERCENT,
                actor.as_ref(),
            );
        }
        if previous <= 2 && next >= 3 {
            send_tower_alert(
                server,
                &state,
                tower_alert::BELOW_50_PERCENT,
                actor.as_ref(),
            );
        }
    }
}

/// Guard towers remain world objects after destruction so the original engraving plot can target them.
pub fn handle_tower_death(server: &GameServer, victim: &WorldObject) -> bool {
    let Some(house) = victim.as_house() else {
        return false;
    };
    let Some((zone, identity)) = tower_identity(server, house) else {
        return false;
    };
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    if !is_retained_house(server, house) {
        return true;
    }
    if let Ok(state) = server.systems.housing.dominions.mark_tower_destroyed(
        zone,
        identity,
        chrono::Utc::now().timestamp(),
    ) {
        crate::systems::skills::skill::interrupt_skills(server, victim);
        crate::systems::skills::buffs::remove_effects_on_death(server, victim);
        apply_tower_state(server, &state);
        crate::systems::social::dominion::broadcast_snapshot(server, &state, false);
        crate::systems::social::dominion::send_tower_alert(
            server,
            &state,
            crate::systems::social::dominion::tower_alert::ENGRAVABLE,
            None,
        );
    } else {
        // A GM/raw damage call outside a siege must not permanently delete territory infrastructure.
        {
            let mut h = house.data();
            h.unit.hp = h.unit.max_hp;
        }
        send_points(server, house);
    }
    true
}

fn send_points(server: &GameServer, house: &HouseRef) {
    let (hp, mp) = {
        let h = house.data();
        (h.unit.hp, h.unit.mp)
    };
    if let Some(world) = server.world.world_of(&WorldObject::from(house)) {
        crate::world::broadcast(
            &world,
            house,
            &crate::packets::world::SCUnitPointsPacket {
                obj_id: house.obj_id,
                hp,
                mp,
            },
            true,
        );
    }
    crate::systems::ai::collision::objects::sync_house(server, house);
}

pub fn apply_tower_state(server: &GameServer, state: &DominionState) {
    if server
        .systems
        .housing
        .dominions
        .get(state.zone_group_id)
        .as_ref()
        != Some(state)
    {
        return;
    }
    let Some(house) = server.systems.housing.manager.by_id(state.tower.house_id) else {
        return;
    };
    let Some((_, identity)) = tower_identity(server, &house) else {
        return;
    };
    if identity != state.tower {
        return;
    }
    server.systems.housing.headquarters.prune(server);
    // A claimed lodestone has completed its original single claim action. This also
    // repairs rows saved by older versions that persisted territory authority alone.
    if state.owner_expedition_id != 0 && house.data().current_step == 0 {
        let authored_claim = crate::systems::housing::manager::template_of(server, &house)
            .is_some_and(|template| {
                template.build_steps.len() == 1
                    && template
                        .build_steps
                        .get(&0)
                        .is_some_and(|step| step.skill_id == 13_661 && step.num_actions == 1)
            });
        if authored_claim {
            crate::systems::housing::manager::set_current_step(server, &house, -1, false);
        }
    }
    let now = chrono::Utc::now().timestamp();
    let active = state.siege.as_ref().is_some_and(|s| s.active_at(now));
    if !active {
        let abort: Vec<_> = server
            .systems
            .housing
            .engraving_runtime
            .active
            .lock()
            .expect("engraving poisoned")
            .iter()
            .filter(|(_, channel)| channel.handle.tower == state.tower)
            .map(|(character, _)| *character)
            .collect();
        for character in abort {
            abort_character(server, character);
        }
    }
    let mut rescaled: Option<i32> = None;
    {
        let mut h = house.data();
        h.unit.expedition_id = state.owner_expedition_id;
        let max_hp = h.unit.max_hp;
        let basis = h.pending_hp_basis.take();
        let mut tower_hp = state.tower_hp;
        // The dominion row's absolute tower_hp was saved against the house row's old HP basis:
        // rescale it once like `load_house_row` did for the house HP.
        if let (Some(hp), Some(basis)) = (tower_hp, basis) {
            let scaled =
                crate::systems::housing::manager::scale_loaded_structure_hp(hp, basis, max_hp);
            if scaled != hp {
                tower_hp = Some(scaled);
                rescaled = Some(scaled);
            }
        }
        h.unit.hp = tower_hp.unwrap_or(max_hp).clamp(0, max_hp);
    }
    if let Some(hp) = rescaled {
        // Persist the converted value with the dominion row (the house row is saved with the
        // new basis), so the conversion happens once.
        if let Err(err) =
            server
                .systems
                .housing
                .dominions
                .update_tower_hp(state.zone_group_id, state.tower, hp)
        {
            tracing::warn!(
                "Dominion {}: could not persist the rescaled tower hp {hp}: {err:?}",
                state.zone_group_id
            );
        }
    }
    crate::systems::housing::manager::set_untouchable(server, &house, !active);
    let obj = WorldObject::from(&house);
    if state.tower_destroyed {
        if !house.data().unit.buffs.check_buff(ENGRAVABLE_BUFF) {
            crate::systems::skills::buffs::add_buff_by_id(server, &obj, ENGRAVABLE_BUFF, &obj, 0);
        }
    } else {
        crate::systems::skills::buffs::remove_buff(server, &obj, ENGRAVABLE_BUFF);
        crate::systems::skills::buffs::remove_buff(server, &obj, ENGRAVING_BUFF);
    }
    super::fort::transfer_captured(server, state);
    super::tax::refresh_bills(server, state.zone_group_id);
    super::participants::sync_all(server);
    send_points(server, &house);
}

/// A calendar period change for one zone. The caller rebroadcasts the state snapshot.
#[derive(Debug, Clone)]
pub struct CalendarTransition {
    pub state: DominionState,
    pub previous_period: u8,
    pub period: u8,
}

/// Runtime state of the calendar: the optional GM test clock, the last period actually
/// broadcast per zone and the last invalid timezone that was reported. None is persisted:
/// after a restart clients receive the current snapshot at world entry, and a later change is
/// announced normally. The clock only drives the calendar; the persisted siege deadlines use
/// the real clock.
#[derive(Debug, Default)]
pub struct CalendarRuntime {
    clock: Mutex<Option<i64>>,
    periods: Mutex<BTreeMap<u32, u8>>,
    warned_timezone: Mutex<Option<String>>,
}

impl CalendarRuntime {
    fn lock_clock(&self) -> MutexGuard<'_, Option<i64>> {
        self.clock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_periods(&self) -> MutexGuard<'_, BTreeMap<u32, u8>> {
        self.periods.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_warned_timezone(&self) -> MutexGuard<'_, Option<String>> {
        self.warned_timezone
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// GM test clock (Unix seconds); `None` restores the real clock.
    pub fn set_clock(&self, now: Option<i64>) {
        *self.lock_clock() = now;
    }

    pub fn clock(&self) -> Option<i64> {
        *self.lock_clock()
    }

    pub fn now(&self) -> i64 {
        self.clock()
            .unwrap_or_else(|| chrono::Utc::now().timestamp())
    }

    /// The configured settings, warning once per invalid timezone value instead of silently
    /// falling back to the UTC default. A repaired value resets the warning.
    pub fn settings(
        &self,
        config: &HousingConfig,
        profile: crate::config::ServerProfile,
    ) -> CalendarSettings {
        let parsed = CalendarTimeZone::parse(&config.siege_calendar_timezone);
        let mut warned = self.lock_warned_timezone();
        if parsed.is_none() && !config.siege_calendar_timezone.trim().is_empty() {
            if warned.as_deref() != Some(config.siege_calendar_timezone.as_str()) {
                tracing::warn!(
                    "Housing.SiegeCalendarTimezone {:?} is not a valid UTC offset; the siege calendar uses its UTC default",
                    config.siege_calendar_timezone
                );
                *warned = Some(config.siege_calendar_timezone.clone());
            }
        } else {
            *warned = None;
        }
        CalendarSettings {
            enabled: config.siege_calendar.unwrap_or_else(|| profile.sieges()),
            timezone: parsed.unwrap_or_default(),
        }
    }

    pub fn last_period(&self, zone_group_id: u32) -> Option<u8> {
        self.lock_periods().get(&zone_group_id).copied()
    }

    /// Record the period carried by a phase snapshot that was actually broadcast. Every
    /// `broadcast_phase_change` / `broadcast_owner_change` calls this, so announcements made
    /// outside the tick (`/dominion schedule`, `/dominion cancel`, capture, release) are not
    /// repeated by the next calendar tick.
    pub fn record(&self, zone_group_id: u32, period: u8) {
        self.lock_periods().insert(zone_group_id, period);
    }

    /// Compare every owned territory's calendar period with the last broadcast one. The first
    /// observation of a zone is silent (a client at world entry gets the snapshot instead).
    /// An unfinished explicit siege (the GM override) owns that zone's timer, so the calendar
    /// records its phase without announcing a calendar change. The explicit siege's phase is
    /// always computed from the real clock, never from the GM test clock.
    pub fn tick(&self, server: &GameServer, now: i64) -> Vec<CalendarTransition> {
        if now < 0 {
            return Vec::new();
        }
        let settings = self.settings(&server.config.housing, server.config.world.profile);
        if !settings.enabled {
            self.lock_periods().clear();
            return Vec::new();
        }
        let calendar = SiegeCalendar::from_castles(&server.game_data.systems.housing.castles);
        if calendar.is_empty() {
            self.lock_periods().clear();
            return Vec::new();
        }
        let claim_delay = calendar.siege_delay_after_claim_days();
        let real_now = chrono::Utc::now().timestamp();
        let states = server.systems.housing.dominions.all();
        let mut periods = self.lock_periods();
        let mut transitions = Vec::new();
        let mut seen = BTreeSet::new();
        for state in states {
            let zone = state.zone_group_id;
            seen.insert(zone);
            if state.owner_expedition_id == 0 {
                periods.remove(&zone);
                continue;
            }
            let explicit = state.siege.as_ref().filter(|siege| {
                siege.status != SiegeStatus::Finished
                    && real_now < siege.ends_at
                    // A calendar declaration keeps describing its cycle; only a GM schedule
                    // owns its own timers (WP-SD2a).
                    && !siege.materialised_from_declaration
            });
            let period = if let Some(siege) = explicit {
                if siege.active_at(real_now) {
                    PERIOD_SIEGE
                } else {
                    PERIOD_WARMUP
                }
            } else {
                let siege_not_before = if state.claimed_at > 0 && claim_delay > 0 {
                    state
                        .claimed_at
                        .saturating_add(claim_delay.saturating_mul(86_400))
                } else {
                    0
                };
                let Some(view) = calendar.view_for_declaration(
                    zone,
                    now,
                    &settings.timezone,
                    state.declared_cycle(),
                    siege_not_before,
                ) else {
                    continue;
                };
                view.period
            };
            match periods.get(&zone).copied() {
                None => {
                    periods.insert(zone, period);
                }
                Some(previous) if previous == period => {}
                Some(previous) => {
                    periods.insert(zone, period);
                    transitions.push(CalendarTransition {
                        state,
                        previous_period: previous,
                        period,
                    });
                }
            }
        }
        periods.retain(|zone, _| seen.contains(zone));
        transitions
    }
}

/// One dominion tick: explicit siege transitions and deadline expiry use the real clock; the
/// weekly calendar phases use the GM test clock when one is set. The caller serializes it with
/// the housing operations (the spawned loop or a test).
pub fn tick_once(server: &GameServer) {
    let (finished, started) = {
        let _items = server.systems.economy.operation_scope();
        // Documented order inside the scope (`economy/operation_scope.rs`):
        // auction -> housing -> authority. `auction_post::tick` receives this guard.
        let _auction = server.systems.economy.auction.operation_lock();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .expect("housing poisoned");
        let real_now = chrono::Utc::now().timestamp();
        let calendar_now = server.systems.housing.calendar.now();
        let mut finished = Vec::new();
        let mut started = Vec::new();
        for state in server
            .systems
            .housing
            .dominions
            .tick_dual(real_now, calendar_now)
            .unwrap_or_else(|e| {
                tracing::error!("Dominion tick: {e}");
                Vec::new()
            })
        {
            apply_tower_state(server, &state);
            crate::systems::social::dominion::broadcast_phase_change(server, &state);
            if state
                .siege
                .as_ref()
                .is_some_and(|siege| siege.status == SiegeStatus::Finished)
            {
                finished.push(state);
            } else if state
                .siege
                .as_ref()
                .is_some_and(|siege| siege.status == SiegeStatus::Active)
            {
                started.push(state);
            }
        }
        // Defender reinforcement waves (WP-S4): start(siege) + the authored `bdm`, announced
        // once through `SCSiegeReinforce`. The authority check-and-sets the persisted flag, so a
        // repeated or restarted tick cannot repeat the packet.
        super::reinforce::tick(server, real_now);
        // Calendar phases. Broadcasts above (and those outside the tick) record the period they
        // sent, so only a period that was not announced yet produces a transition here.
        // WP-SD1c: the running cycle's camp lots are posted at the auction start.
        super::auction_post::tick(server, calendar_now, &_auction);
        // WP-SD2a: an accepted declaration becomes the cycle's siege at the warmup boundary.
        super::declare::tick(server, calendar_now);
        for transition in server.systems.housing.calendar.tick(server, calendar_now) {
            tracing::debug!(
                "Dominion {} siege period {} -> {}",
                transition.state.zone_group_id,
                transition.previous_period,
                transition.period
            );
            crate::systems::social::dominion::broadcast_phase_change(server, &transition.state);
        }
        // Non-PvP windows use the real clock (unlike the calendar test clock): the client's
        // cached end and the server's authored `start + minutes*60` are wall-clock deadlines.
        crate::systems::social::dominion_notifications::expire_non_pvp(server, real_now);
        (finished, started)
    };
    // The kinds 57-60 credit reports outside the economy operation scope, so an online
    // participant's record is written immediately instead of waiting for a packet flush.
    for state in &finished {
        super::credit::credit_siege_outcome(server, state);
    }
    // A roster frozen before the start gets its seals now; later registrations are handed
    // theirs by the join path, and `seals::reconcile` takes them back after the siege.
    for state in &started {
        super::seals::grant_roster(server, state);
    }
}

pub fn start_tick(server: &Arc<GameServer>) {
    let server = server.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut participant_tick = 0_u8;
        loop {
            tick.tick().await;
            participant_tick += 1;
            if participant_tick == 4 {
                participant_tick = 0;
                super::participants::sync_all(&server);
                // Roster seals and the non-participant lockdown share the roster snapshot:
                // both are cheap and must never run while the housing operation lock is held
                // (they touch item containers and the world).
                super::seals::reconcile(&server);
                super::siege_area::evict_non_participants(&server);
            }
            let channels: Vec<_> = server
                .systems
                .housing
                .engraving_runtime
                .active
                .lock()
                .expect("engraving poisoned")
                .iter()
                .map(|(id, c)| (*id, c.clone()))
                .collect();
            for (id, channel) in channels {
                if !valid_channel(&server, &channel) {
                    abort_character(&server, id);
                }
            }
            tick_once(&server);
        }
    });
}

#[cfg(test)]
#[path = "credit_tests.rs"]
mod credit_tests;
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
