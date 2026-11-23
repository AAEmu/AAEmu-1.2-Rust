//! Encounter behaviour and geometry at the shared engine seams.
//!
//! Each encounter implements [`Encounter`] in its own file and delegates to its free
//! functions. Every method has a no-op default. The engine dispatches with the original
//! key. AI and walker floors use [`for_zone`], character water uses [`for_world`], and
//! id-keyed behaviour uses [`all`]. The six dungeons precede the world content modules.
use crate::models::buff::Buff;
use crate::models::transform::PositionAndRotation;
use crate::server::GameServer;
use crate::systems::ai::framework::{AiCtx, BehaviorKind};
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::{DoodadFuncSpawnGimmick, DoodadFuncWaterVolume};
use crate::systems::npcs::gimmick::manager::SpawnGimmickParams;
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::data::effects::SpawnGimmickEffect;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::state::SkillRef;
use crate::world::{PlayerRef, WorldInstance, WorldObject};

use super::{armory, cradle, hadir, howling_abyss, palace_cellar, sharpwind};

/// Content behaviour at the engine's seams. Every method is a no-op by default.
pub trait Encounter: Sync {
    /// Before a behaviour's tick: `true` when the encounter moved the NPC itself and the
    /// behaviour does not tick.
    fn ai_tick(&self, _c: &AiCtx<'_>, _k: BehaviorKind, _delta_ms: i64) -> bool {
        false
    }

    /// Before an attack behaviour's tick: `true` when the encounter drove the NPC this tick.
    fn combat_step(&self, _c: &mut AiCtx<'_>, _k: BehaviorKind, _delta_ms: i64) -> bool {
        false
    }

    /// `ShouldReturn` for the NPC at `me` fighting a target at `target`: `Some` replaces the
    /// distance leash.
    fn leash(&self, _c: &AiCtx<'_>, _me: Vec3, _target: Vec3) -> Option<bool> {
        None
    }

    /// `MoveInRange` once the target is out of reach: `true` when the encounter moved the NPC
    /// toward `target` (a `speed` m step with `move_flags`) instead of the route chase.
    fn chase(&self, _c: &AiCtx<'_>, _target: Vec3, _speed: f64, _move_flags: u8) -> bool {
        false
    }

    /// The floor height of a walker's step from `pos` to `new`, given the reference height
    /// `target_z`.
    fn walker_floor(
        &self,
        _template_id: u32,
        _zone_id: u32,
        _pos: Vec3,
        _new: Vec3,
        _spawner_z: Option<f32>,
        target_z: f32,
    ) -> f32 {
        target_z
    }

    /// A doodad's `SpawnGimmick` func: `Some` is the func's result instead of the generic
    /// spawn.
    fn spawn_gimmick(
        &self,
        _server: &GameServer,
        _ctx: &DoodadCtx<'_>,
        _f: &DoodadFuncSpawnGimmick,
    ) -> Option<bool> {
        None
    }

    /// A doodad's `WaterVolume` func: `true` when the encounter applied it (the func then
    /// returns `false`, as the generic one does after logging it).
    fn water_volume(
        &self,
        _server: &GameServer,
        _ctx: &DoodadCtx<'_>,
        _f: &DoodadFuncWaterVolume,
    ) -> bool {
        false
    }

    /// The start of a skill use: `Some` refuses it with that result.
    fn cast_gate(
        &self,
        _server: &GameServer,
        _skill: &SkillRef,
        _caster: &WorldObject,
    ) -> Option<(u8, u32)> {
        None
    }

    /// The start of `ApplyEffects`: `true` skips the cast's effects.
    fn on_apply_effects(
        &self,
        _server: &GameServer,
        _t: &SkillTemplate,
        _caster: &WorldObject,
    ) -> bool {
        false
    }

    /// The area candidates of a cast, after the relation and alive filters.
    fn area_targets(
        &self,
        _server: &GameServer,
        _caster: &WorldObject,
        _t: &SkillTemplate,
        _units: &mut Vec<WorldObject>,
    ) {
    }

    /// The readiness check of a cast's effects: `Some` is the check of this skill.
    fn effects_ready(
        &self,
        _server: &GameServer,
        _t: &SkillTemplate,
        _caster: &WorldObject,
        _target: &Target,
    ) -> Option<Result<(), (u8, u32)>> {
        None
    }

    /// A `KnockBack` of a player, from `target_pos` to `end`: `true` when the encounter sent
    /// the player a knockback (player knockbacks otherwise run on the client).
    fn knock_back_player(
        &self,
        _server: &GameServer,
        _args: &EffectArgs<'_>,
        _player: &PlayerRef,
        _target: &WorldObject,
        _target_pos: PositionAndRotation,
        _end: (f32, f32, f32),
    ) -> bool {
        false
    }

    /// `true` when `buff` is not added to `owner`.
    fn blocks_buff(&self, _server: &GameServer, _owner: &WorldObject, _buff: &Buff) -> bool {
        false
    }

    /// A character in water (the underwater part of `Character.SetPosition`).
    fn on_character_water(
        &self,
        _server: &GameServer,
        _world: &WorldInstance,
        _p: &PlayerRef,
        _obj: &WorldObject,
    ) {
    }

    /// A `SpawnGimmickEffect` cast by `caster`: the encounter may aim `params` elsewhere.
    fn gimmick_spawn_target(
        &self,
        _server: &GameServer,
        _e: &SpawnGimmickEffect,
        _args: &EffectArgs<'_>,
        _caster: &WorldObject,
        _world: &WorldInstance,
        _params: &mut SpawnGimmickParams,
    ) {
    }
}

/// For an unregistered zone or world, every hook is the default.
struct NoEncounter;

impl Encounter for NoEncounter {}

static ALL: [&dyn Encounter; 8] = [
    &armory::Armory,
    &palace_cellar::PalaceCellar,
    &hadir::HadirFarm,
    &sharpwind::SharpwindMines,
    &cradle::KroloalCradle,
    &howling_abyss::HowlingAbyss,
    &super::plateau::Plateau,
    &super::library::Library,
];

/// The encounter of a zone.
pub fn for_zone(zone_id: u32) -> &'static dyn Encounter {
    match zone_id {
        armory::ZONE => &armory::Armory,
        palace_cellar::ZONE => &palace_cellar::PalaceCellar,
        hadir::ZONE => &hadir::HadirFarm,
        sharpwind::ZONE => &sharpwind::SharpwindMines,
        cradle::ZONE => &cradle::KroloalCradle,
        howling_abyss::ZONE | howling_abyss::GREATER_ZONE => &howling_abyss::HowlingAbyss,
        _ => &NoEncounter,
    }
}

/// The encounter of a world template.
pub fn for_world(name: &str) -> &'static dyn Encounter {
    match name {
        armory::WORLD => &armory::Armory,
        palace_cellar::WORLD => &palace_cellar::PalaceCellar,
        hadir::WORLD => &hadir::HadirFarm,
        sharpwind::WORLD => &sharpwind::SharpwindMines,
        cradle::WORLD => &cradle::KroloalCradle,
        howling_abyss::WORLD | howling_abyss::GREATER_WORLD => &howling_abyss::HowlingAbyss,
        _ => &NoEncounter,
    }
}

/// Every encounter, for hooks keyed by an id.
pub fn all() -> &'static [&'static dyn Encounter] {
    &ALL
}
