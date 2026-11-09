//! Real-object headquarters. A GM-scheduled siege binds a retained HQ doodad explicitly; a
//! declared siege binds its camp (WP-SD2c) at acceptance and re-binds the retained doodad on
//! boot by its persisted position. The defender's HQ is spawned at the warmup only when
//! `Housing.DefenseHqOffset` is configured (the retail rule is UNKNOWN; default = the GM bind
//! behaviour, no spawn). No synthetic coordinates, scroll fees or auction period.
//! Call bind/prune/clear while holding housing.operation_lock; effect completion takes it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::data::world_data::Portal;
use crate::models::transform::PositionAndRotation;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::packets_special::SCTeleportUnitPacket;
use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster};
use crate::world::{DoodadRef, PlayerRef, WorldObject};

use super::participants;
use super::runtime;
use super::state::{DominionState, SiegeSide, SiegeStatus, TowerIdentity};

pub const OFFENSE_HQ_TEMPLATE: u32 = 4307;
pub const DEFENSE_HQ_TEMPLATE: u32 = 3410;
pub const OFFENSE_TELEPORT_SKILL: u32 = 17046;
pub const DEFENSE_TELEPORT_SKILL: u32 = 20950;
/// Original Trion2014 siege scroll placement rule; distance is horizontal.
pub const OFFENSE_HQ_MIN_DISTANCE: f64 = 250.0;
/// Original 1.2 TeleportReason values (Models/Game/Teleport/TeleportReason.cs).
pub const TELEPORT_REASON_DEFENSE_HQ: u8 = 8;
pub const TELEPORT_REASON_OFFENSE_HQ: u8 = 9;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HqError {
    #[error("no current unfinished scheduled siege for this territory")]
    MissingSiege,
    #[error("headquarters must be a retained original HQ doodad in this territory and instance")]
    InvalidHeadquarters,
    #[error("offense headquarters must be at least 250 meters from the opposing lodestone")]
    TooClose,
}

#[derive(Debug, Clone)]
struct Binding {
    doodad: DoodadRef,
    position: PositionAndRotation,
    siege_id: u64,
    ownership_generation: u64,
    tower: TowerIdentity,
    /// Attacker of the declaration this binding was created for (WP-SD2c); 0 for a GM bind.
    declared_attacker: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HqSnapshot {
    pub obj_id: u32,
    pub position: PositionAndRotation,
}

#[derive(Debug, Default)]
pub struct Headquarters {
    bindings: Mutex<HashMap<(u32, u8), Binding>>,
}

fn key(zone: u32, side: SiegeSide) -> (u32, u8) {
    (zone, side as u8)
}

fn current_tower(server: &GameServer, state: &DominionState) -> Option<WorldObject> {
    let tower = server.systems.housing.manager.by_id(state.tower.house_id)?;
    let object = WorldObject::from(&tower);
    (runtime::is_live_object(server, &object)
        && runtime::tower_identity(server, &tower) == Some((state.zone_group_id, state.tower)))
    .then_some(object)
}

fn hq_position(
    server: &GameServer,
    state: &DominionState,
    side: SiegeSide,
    doodad: &DoodadRef,
) -> Option<PositionAndRotation> {
    let (template, phase, transform, deleted) = {
        let d = doodad.data();
        (d.template_id, d.func_group_id, d.transform, d.phase.deleted)
    };
    let authored = match side {
        SiegeSide::Attacker => template == OFFENSE_HQ_TEMPLATE && matches!(phase, 10582 | 10779),
        SiegeSide::Defender => template == DEFENSE_HQ_TEMPLATE && matches!(phase, 8173 | 8238),
    };
    let object = WorldObject::from(doodad);
    if !authored
        || deleted
        || transform.parent_obj_id != 0
        || transform.instance_id != state.tower.instance_id
        || !runtime::is_live_object(server, &object)
    {
        return None;
    }
    let world = server.world.world(transform.instance_id)?;
    let position = world.world_pos_rot(&object);
    if ![
        position.x,
        position.y,
        position.z,
        position.rot_x,
        position.rot_y,
        position.rot_z,
    ]
    .into_iter()
    .all(f32::is_finite)
        || server
            .game_data
            .zones
            .zone_by_key(world.zone_id_at(position.x, position.y))?
            .group_id
            != state.zone_group_id
    {
        return None;
    }
    Some(position)
}

impl Headquarters {
    /// Only an already spawned original HQ is accepted. The caller's command authenticates
    /// GM access and holds housing.operation_lock; this method never creates or moves it.
    pub fn bind(
        &self,
        server: &GameServer,
        zone: u32,
        side: SiegeSide,
        doodad: &DoodadRef,
    ) -> Result<(), HqError> {
        let state = server
            .systems
            .housing
            .dominions
            .get(zone)
            .ok_or(HqError::MissingSiege)?;
        let siege = state.siege.as_ref().ok_or(HqError::MissingSiege)?;
        if state.owner_expedition_id == 0
            || siege.status == SiegeStatus::Finished
            || chrono::Utc::now().timestamp() >= siege.ends_at
        {
            return Err(HqError::MissingSiege);
        }
        let tower = current_tower(server, &state).ok_or(HqError::InvalidHeadquarters)?;
        let position =
            hq_position(server, &state, side, doodad).ok_or(HqError::InvalidHeadquarters)?;
        if side == SiegeSide::Attacker {
            let world = server
                .world
                .world(state.tower.instance_id)
                .ok_or(HqError::InvalidHeadquarters)?;
            let origin = world.world_pos_rot(&tower);
            let distance2 = (f64::from(position.x) - f64::from(origin.x)).powi(2)
                + (f64::from(position.y) - f64::from(origin.y)).powi(2);
            if !distance2.is_finite() || distance2 < OFFENSE_HQ_MIN_DISTANCE.powi(2) {
                return Err(HqError::TooClose);
            }
        }
        self.bindings.lock().expect("headquarters poisoned").insert(
            key(zone, side),
            Binding {
                doodad: doodad.clone(),
                position,
                siege_id: siege.siege_id,
                ownership_generation: state.ownership_generation,
                tower: state.tower,
                declared_attacker: 0,
            },
        );
        Ok(())
    }

    /// Binds the camp doodad of an accepted declaration as the offense HQ (WP-SD2c). There is
    /// no siege record yet: the binding is validated against the declaration until the
    /// calendar materialises the siege, which [`Self::bind_materialised`] stamps.
    pub fn bind_declaration(
        &self,
        server: &GameServer,
        zone: u32,
        doodad: &DoodadRef,
    ) -> Result<(), HqError> {
        let state = server
            .systems
            .housing
            .dominions
            .get(zone)
            .ok_or(HqError::MissingSiege)?;
        let declaration = state.declaration.ok_or(HqError::MissingSiege)?;
        let (template, transform) = {
            let d = doodad.data();
            (d.template_id, d.transform)
        };
        if template != OFFENSE_HQ_TEMPLATE
            || transform.parent_obj_id != 0
            || transform.instance_id != state.tower.instance_id
            || ![
                transform.x,
                transform.y,
                transform.z,
                transform.rot_x,
                transform.rot_y,
                transform.rot_z,
            ]
            .into_iter()
            .all(f32::is_finite)
            || server
                .game_data
                .zones
                .zone_by_key(transform.zone_id)
                .map(|zone| zone.group_id)
                != Some(state.zone_group_id)
        {
            return Err(HqError::InvalidHeadquarters);
        }
        self.bindings.lock().expect("headquarters poisoned").insert(
            key(zone, SiegeSide::Attacker),
            Binding {
                doodad: doodad.clone(),
                position: transform.local(),
                siege_id: 0,
                ownership_generation: state.ownership_generation,
                tower: state.tower,
                declared_attacker: declaration.attacker_expedition_id,
            },
        );
        Ok(())
    }

    /// Stamps the pending declaration binding with the materialised siege, so the snapshot
    /// and teleport validation accept it.
    pub fn bind_materialised(&self, server: &GameServer, state: &DominionState) {
        let Some(current) = server.systems.housing.dominions.get(state.zone_group_id) else {
            return;
        };
        if current.tower != state.tower {
            return;
        }
        let Some(siege) = current
            .siege
            .as_ref()
            .filter(|siege| siege.status != SiegeStatus::Finished)
        else {
            return;
        };
        let mut bindings = self.bindings.lock().expect("headquarters poisoned");
        let Some(binding) = bindings.get_mut(&key(current.zone_group_id, SiegeSide::Attacker))
        else {
            return;
        };
        if binding.declared_attacker == siege.attacker_expedition_id
            && binding.tower == current.tower
        {
            binding.siege_id = siege.siege_id;
            binding.ownership_generation = current.ownership_generation;
        }
    }

    /// Re-binds retained camp doodads to reloaded declarations (WP-SD2c): the doodad obj id is
    /// runtime-only, so on boot the live 4307 nearest the persisted camp position is picked.
    pub fn restore_declarations(&self, server: &GameServer) {
        for state in server.systems.housing.dominions.all() {
            let Some(declaration) = state.declaration else {
                continue;
            };
            let Some(world) = server.world.world(state.tower.instance_id) else {
                continue;
            };
            let found = world.all_doodads().into_iter().find(|doodad| {
                let d = doodad.data();
                let zone_ok = server
                    .game_data
                    .zones
                    .zone_by_key(d.transform.zone_id)
                    .is_some_and(|zone| zone.group_id == state.zone_group_id);
                let dx = f64::from(d.transform.x) - f64::from(declaration.hq_x);
                let dy = f64::from(d.transform.y) - f64::from(declaration.hq_y);
                let dz = f64::from(d.transform.z) - f64::from(declaration.hq_z);
                d.template_id == OFFENSE_HQ_TEMPLATE
                    && zone_ok
                    && dx * dx + dy * dy + dz * dz <= 25.0
            });
            let Some(doodad) = found else {
                continue;
            };
            if self
                .bind_declaration(server, state.zone_group_id, &doodad)
                .is_ok()
            {
                let position = doodad.data().transform;
                let _ = server.systems.housing.dominions.bind_declaration_hq(
                    state.zone_group_id,
                    doodad.obj_id,
                    (position.x, position.y, position.z),
                );
            }
        }
    }

    /// Spawns the defender HQ (3410) at the configured lodestone offset when a declared siege
    /// materialises. The retail rule is UNKNOWN; `Housing.DefenseHqOffset` defaults to zero,
    /// which keeps the existing GM-bound behaviour (no automatic spawn).
    pub fn spawn_defense_hq(
        &self,
        server: &GameServer,
        state: &DominionState,
    ) -> Option<DoodadRef> {
        let offset = server.config.housing.defense_hq_offset;
        if offset == [0.0; 3] || state.siege.is_none() {
            return None;
        }
        let tower = server.systems.housing.manager.by_id(state.tower.house_id)?;
        let tower_object = WorldObject::from(&tower);
        let world = server.world.world(state.tower.instance_id)?;
        let base = world.world_pos_rot(&tower_object);
        let mut transform = tower.data().unit.transform;
        transform.set_position(
            base.x + offset[0],
            base.y + offset[1],
            base.z + offset[2],
            0.0,
            0.0,
            base.rot_z,
        );
        let Some(doodad) = crate::systems::npcs::doodad::manager::create(
            server,
            &world,
            0,
            DEFENSE_HQ_TEMPLATE,
            None,
            false,
        ) else {
            tracing::warn!(
                "Dominion {}: defense HQ template {DEFENSE_HQ_TEMPLATE} is missing",
                state.zone_group_id
            );
            return None;
        };
        doodad.data().transform = transform;
        crate::systems::npcs::doodad::manager::init_doodad(server, &world, &doodad);
        crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
        if let Err(error) = self.bind(server, state.zone_group_id, SiegeSide::Defender, &doodad) {
            tracing::warn!(
                "Dominion {}: spawned defense HQ cannot be bound: {error}",
                state.zone_group_id
            );
        }
        Some(doodad)
    }

    pub fn clear_zone(&self, zone: u32) {
        self.bindings
            .lock()
            .expect("headquarters poisoned")
            .retain(|(id, _), _| *id != zone);
    }

    pub fn clear_all(&self) {
        self.bindings.lock().expect("headquarters poisoned").clear();
    }

    /// Real positions for native Dominion team snapshots. A moved/reused object or stale
    /// authority generation cannot silently become the next siege's HQ.
    pub fn snapshot(
        &self,
        server: &GameServer,
        state: &DominionState,
        side: SiegeSide,
    ) -> Option<HqSnapshot> {
        let binding = self
            .bindings
            .lock()
            .expect("headquarters poisoned")
            .get(&key(state.zone_group_id, side))
            .cloned()?;
        self.validate(server, state, side, &binding)
    }

    fn validate(
        &self,
        server: &GameServer,
        state: &DominionState,
        side: SiegeSide,
        binding: &Binding,
    ) -> Option<HqSnapshot> {
        let current = server.systems.housing.dominions.get(state.zone_group_id)?;
        if binding.declared_attacker != 0 {
            if let Some(declaration) = current.declaration {
                if declaration.attacker_expedition_id != binding.declared_attacker
                    || declaration.hq_obj_id != binding.doodad.obj_id
                    || binding.tower != current.tower
                    || current.owner_expedition_id == 0
                    || binding.doodad.data().transform.local() != binding.position
                {
                    return None;
                }
                return Some(HqSnapshot {
                    obj_id: binding.doodad.obj_id,
                    position: binding.position,
                });
            }
            // The declaration was materialised (or dropped): only a stamped siege binding is
            // usable from here on.
            if binding.siege_id == 0 {
                return None;
            }
        }
        let siege = current.siege.as_ref()?;
        if siege.status == SiegeStatus::Finished
            || chrono::Utc::now().timestamp() >= siege.ends_at
            || binding.siege_id != siege.siege_id
            || state.siege.as_ref()?.siege_id != siege.siege_id
            || binding.ownership_generation != current.ownership_generation
            || state.ownership_generation != current.ownership_generation
            || binding.tower != current.tower
            || state.tower != current.tower
            || current.owner_expedition_id == 0
        {
            return None;
        }
        current_tower(server, &current)?;
        let position = hq_position(server, &current, side, &binding.doodad)?;
        if position != binding.position {
            return None;
        }
        Some(HqSnapshot {
            obj_id: binding.doodad.obj_id,
            position,
        })
    }

    /// Remove expired/replaced bindings after phase changes. Validation is performed outside
    /// the cache lock; no world or authority lookup runs while that leaf lock is held.
    pub fn prune(&self, server: &GameServer) -> usize {
        let bindings: Vec<_> = self
            .bindings
            .lock()
            .expect("headquarters poisoned")
            .iter()
            .map(|(key, binding)| (*key, binding.clone()))
            .collect();
        let mut removed = 0;
        for ((zone, side), binding) in bindings {
            let side = if side == SiegeSide::Attacker as u8 {
                SiegeSide::Attacker
            } else {
                SiegeSide::Defender
            };
            let valid = server
                .systems
                .housing
                .dominions
                .get(zone)
                .and_then(|state| self.validate(server, &state, side, &binding))
                .is_some();
            if !valid {
                let mut entries = self.bindings.lock().expect("headquarters poisoned");
                let key = key(zone, side);
                if entries.get(&key).is_some_and(|current| {
                    Arc::ptr_eq(&current.doodad, &binding.doodad)
                        && current.siege_id == binding.siege_id
                        && current.position == binding.position
                }) {
                    entries.remove(&key);
                    removed += 1;
                }
            }
        }
        removed
    }
}

/// Ordinary siege resurrection uses the current registered side's real HQ. A living
/// character cannot use the resurrect packet to bypass the authored HQ cast/cooldown.
/// None preserves the existing ordinary portal fallback, including missing runtime bindings.
pub fn respawn_portal(server: &GameServer, player: &PlayerRef) -> Option<Portal> {
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    let (online, dead, expedition, transform) = {
        let c = player.data();
        (
            c.is_online,
            c.unit.hp <= 0,
            c.unit.expedition_id,
            c.unit.transform,
        )
    };
    if !online
        || !dead
        || expedition == 0
        || !runtime::is_live_object(server, &WorldObject::from(player))
        || server
            .expeditions
            .expedition_of_character(player.character_id)
            != expedition
        || !server
            .expeditions
            .get(expedition)
            .is_some_and(|e| !e.is_disbanded)
    {
        return None;
    }
    let group = server
        .game_data
        .zones
        .zone_by_key(transform.zone_id)?
        .group_id;
    let state = server.systems.housing.dominions.get(group)?;
    let siege = state.siege.as_ref()?;
    let participant = siege.participants.get(&player.character_id)?;
    let side_expedition = match participant.side {
        SiegeSide::Attacker => siege.attacker_expedition_id,
        SiegeSide::Defender => siege.defender_expedition_id,
    };
    if state.tower.instance_id != transform.instance_id
        || !siege.active_at(chrono::Utc::now().timestamp())
        || participant.expedition_id != expedition
        || expedition != side_expedition
    {
        return None;
    }
    let hq = server
        .systems
        .housing
        .headquarters
        .snapshot(server, &state, participant.side)?;
    let p = hq.position;
    let world = server.world.world(transform.instance_id)?;
    let z = crate::systems::ai::collision::ground_height_or(
        server,
        transform.instance_id,
        p.x,
        p.y,
        p.z,
        || p.z,
    );
    if !z.is_finite() {
        return None;
    }
    Some(Portal {
        world_id: world.template.id,
        zone_id: world.zone_id_at(p.x, p.y),
        x: p.x,
        y: p.y,
        z,
        z_rot: p.rot_z,
        yaw: p.rot_z,
        ..Default::default()
    })
}

/// Only the normal completed original five-second self cast can use an HQ binding.
pub fn teleport_effect(server: &GameServer, args: &EffectArgs<'_>) {
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    teleport_effect_inner(server, args);
}

pub(super) fn teleport_effect_inner(server: &GameServer, args: &EffectArgs<'_>) -> bool {
    let (Some(caster), Some(source), Some(target)) = (
        args.caster.as_ref(),
        args.source.skill.as_ref(),
        args.target.as_obj(),
    ) else {
        return false;
    };
    let Some(player) = caster.as_player() else {
        return false;
    };
    let (side, reason, participant_buff, commander_buff) = match source.id {
        OFFENSE_TELEPORT_SKILL => (
            SiegeSide::Attacker,
            TELEPORT_REASON_OFFENSE_HQ,
            participants::OFFENSE_PARTICIPANT_BUFF,
            participants::OFFENSE_COMMANDER_BUFF,
        ),
        DEFENSE_TELEPORT_SKILL => (
            SiegeSide::Defender,
            TELEPORT_REASON_DEFENSE_HQ,
            participants::DEFENSE_PARTICIPANT_BUFF,
            participants::DEFENSE_COMMANDER_BUFF,
        ),
        _ => return false,
    };
    let Some(canonical) = server.game_data.systems.skills.skill(source.id) else {
        return false;
    };
    let accepted = {
        let state = source.state();
        state.generation != 0 && !state.cancelled && state.initial_target == player.obj_id
    };
    if !Arc::ptr_eq(canonical, &source.template)
        || canonical.casting_time != 5000
        || canonical.target_type != 0
        || args.source.buff.is_some()
        || args.source.is_trigger
        || !accepted
        || !target.same(caster)
        || !matches!(args.cast, CastAction::Skill { skill_id, tl_id } if skill_id == source.id && tl_id == source.tl_id())
        || !matches!(args.caster_caster, SkillCaster::Unit(id) if id == player.obj_id)
        || !matches!(args.target_caster, SkillCastTarget::Unit(id) if id == player.obj_id)
        || !runtime::is_live_object(server, caster)
    {
        return false;
    }
    let (online, alive, expedition, transform, granted) = {
        let c = player.data();
        (
            c.is_online,
            c.unit.hp > 0 && !c.skill_char.skill_cancelled,
            c.unit.expedition_id,
            c.unit.transform,
            c.unit.buffs.check_buff(participant_buff) || c.unit.buffs.check_buff(commander_buff),
        )
    };
    let Some(zone) = server
        .game_data
        .zones
        .zone_by_key(transform.zone_id)
        .map(|z| z.group_id)
    else {
        return false;
    };
    let Some(state) = server.systems.housing.dominions.get(zone) else {
        return false;
    };
    let Some(siege) = state.siege.as_ref() else {
        return false;
    };
    let side_expedition = if side == SiegeSide::Attacker {
        siege.attacker_expedition_id
    } else {
        siege.defender_expedition_id
    };
    if !online
        || !alive
        || !granted
        || expedition == 0
        || expedition != side_expedition
        || transform.instance_id != state.tower.instance_id
        || !siege.active_at(chrono::Utc::now().timestamp())
        || !siege
            .participants
            .get(&player.character_id)
            .is_some_and(|p| p.side == side && p.expedition_id == expedition)
        || server
            .expeditions
            .expedition_of_character(player.character_id)
            != expedition
        || !server
            .expeditions
            .get(expedition)
            .is_some_and(|e| !e.is_disbanded)
    {
        return false;
    }
    let Some(hq) = server
        .systems
        .housing
        .headquarters
        .snapshot(server, &state, side)
    else {
        player.send(crate::packets::world::SCErrorMsgPacket::new(
            error_message::TELEPORT_TO_SIEGE_HQ,
            0,
            true,
        ));
        return false;
    };
    let p = hq.position;
    let z = crate::systems::ai::collision::ground_height_or(
        server,
        transform.instance_id,
        p.x,
        p.y,
        p.z,
        || p.z,
    );
    if !z.is_finite() {
        return false;
    }
    // Same-instance portal semantics: the client acknowledges through the existing teleport
    // load/movement path, which updates server position/visibility and clears this flag.
    crate::world::session::send_teleport_unit(
        player,
        SCTeleportUnitPacket {
            reason,
            error_message: 0,
            x: p.x,
            y: p.y,
            z,
            z2: p.rot_z,
        },
    );
    true
}
