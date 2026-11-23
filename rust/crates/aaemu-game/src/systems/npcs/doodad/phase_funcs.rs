//! `DoodadPhaseFuncTemplate.Use` of every phase func (the 42 `FuncTemplate` variants with
//! [`FuncTemplate::is_phase_func`]).
//!
//! Pulse state is per live doodad, so copies of one template and different instances do not
//! suppress each other's triggers.

use std::collections::HashMap;
use std::sync::Arc;

use rand::distributions::{Distribution, WeightedIndex};
use rand::Rng;

use super::phase::{self, doodad_has_matching_climate};
use super::runtime::DoodadCtx;
use super::spawner::{DoodadSpawner, DoodadSpawnerDef, SpawnerBase};
use super::tasks::{self, FuncTaskKind};
use super::templates::*;
use crate::data::world_templates::WorldSpawnPosition;
use crate::systems::instances::encounters::hooks as encounter_hooks;
use crate::systems::instances::encounters::registry::{self as encounters, CloutClass};
use crate::systems::npcs::area_trigger::{self, AreaTrigger};
use crate::systems::npcs::fish_school;
use crate::world::{now_ms, WorldObject};

/// `Random.Shared.Next(min, max)` (`max` exclusive, `min` when `min >= max`).
fn random_next(min: i32, max: i32) -> i32 {
    if min >= max {
        min
    } else {
        rand::thread_rng().gen_range(min..max)
    }
}

/// `owner.OverridePhase = nextPhase; return true` when the caster is a character.
fn override_if_character(
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
    next_phase: i32,
) -> bool {
    if ctx.phase_requester(caster).is_some() {
        ctx.doodad.data().phase.override_phase = next_phase;
        return true;
    }
    false
}

/// The `OverridePhaseTime` handling of `Timer` / `Growth`: returns the remaining delay.
fn time_left_with_override(ctx: &DoodadCtx<'_>, delay: f64) -> f64 {
    let mut d = ctx.doodad.data();
    let mut time_left = delay;
    if d.phase.override_phase_time_ms > 0 {
        // Reset the override
        d.phase_time_ms = d.phase.override_phase_time_ms;
        d.phase.override_phase_time_ms = 0;
        d.update_final_phase_end();
        let since_phase_start = (now_ms() - d.phase_time_ms) as f64;
        time_left = delay - since_phase_start;
    }
    if time_left < 1.0 {
        time_left = 1.0;
    }
    d.growth_time_ms = now_ms() + time_left as i64;
    time_left
}

/// Puzzle inputs and outcomes are weighted within their phase. Compact groups 14852 and 14845
/// author 85/10/5 and four equal weights respectively, so weights need not sum to 100.
fn weighted_puzzle_row(rows: &[(u32, f32)]) -> Option<u32> {
    let distribution = WeightedIndex::new(rows.iter().map(|row| row.1)).ok()?;
    Some(rows[distribution.sample(&mut rand::thread_rng())].0)
}

pub(super) fn select_puzzle_input(ctx: &DoodadCtx<'_>, phase: &[DoodadPhaseFunc]) -> Option<u32> {
    if !phase
        .iter()
        .any(|row| row.func_type == "DoodadFuncPuzzleIn")
    {
        return None;
    }
    let rows: Vec<_> = phase
        .iter()
        .filter_map(
            |row| match ctx.data().phase_func_template(row.func_id, &row.func_type) {
                Some(FuncTemplate::PuzzleIn(input)) => Some((input.id, input.ratio)),
                _ => None,
            },
        )
        .collect();
    weighted_puzzle_row(&rows)
}

pub(super) fn select_puzzle_outcome(ctx: &DoodadCtx<'_>, phase: &[DoodadPhaseFunc]) -> Option<u32> {
    if !phase
        .iter()
        .any(|row| row.func_type == "DoodadFuncPuzzleOut")
    {
        return None;
    }
    let group = ctx.doodad.data().puzzle_group;
    let rows: Vec<_> = phase
        .iter()
        .filter_map(
            |row| match ctx.data().phase_func_template(row.func_id, &row.func_type) {
                Some(FuncTemplate::PuzzleOut(outcome)) if outcome.group_id as i32 == group => {
                    Some((outcome.id, outcome.ratio))
                }
                _ => None,
            },
        )
        .collect();
    weighted_puzzle_row(&rows)
}

/// `DoodadPhaseFunc.Use(caster, owner)` → the template's `Use`. Returns `true` to stop the phase
/// funcs of the current phase (C# return value).
pub fn use_phase_func(t: &FuncTemplate, ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>) -> bool {
    let server = ctx.server;
    match t {
        FuncTemplate::Timer(f) => {
            if f.next_phase > 0 {
                tracing::trace!(
                    "DoodadFuncTimer: Delay {}, NextPhase {}, KeepRequester {}, ShowTip {}, ShowEndTime {}, Tip {}",
                    f.delay, f.next_phase, f.keep_requester, f.show_tip, f.show_end_time, f.tip
                );
                let time_left = time_left_with_override(ctx, f64::from(f.delay));
                tasks::start_func_task(
                    server,
                    &ctx.doodad,
                    caster,
                    FuncTaskKind::Timer {
                        next_phase: f.next_phase,
                        keep_requester: f.keep_requester && ctx.carries_phase_requester,
                    },
                    tasks::ms(time_left),
                );
            }
            // we never interrupt the sequence of phase functions
            false
        }
        FuncTemplate::Final(f) => {
            let respawn = f.respawn || {
                let doodad = ctx.doodad.data();
                fish_school::is_school_expiry(doodad.template_id, doodad.func_group_id)
            };
            tracing::trace!(
                "DoodadFuncFinal: After {}, Respawn {}, MinTime {}, MaxTime {}",
                f.after,
                respawn,
                f.min_time,
                f.max_time
            );
            // Clamp the range: Random.Shared.Next throws when min > max
            let min_time = f.min_time.max(0);
            let max_time = f.max_time.max(min_time);
            let delay = random_next(min_time, max_time);
            let kind = FuncTaskKind::Final {
                respawn,
                delay,
                respawn_stage: None,
            };
            if f.after > 0 {
                let mut after_delay = f.after as u32;
                {
                    let mut d = ctx.doodad.data();
                    if d.phase.override_phase_time_ms > 0 {
                        d.phase_time_ms = d.phase.override_phase_time_ms;
                        d.phase.override_phase_time_ms = 0;
                        d.update_final_phase_end();
                        after_delay = d.time_left(now_ms());
                    }
                }
                // After ms remove the object from visibility
                tasks::start_func_task(
                    server,
                    &ctx.doodad,
                    caster,
                    kind,
                    tasks::ms(f64::from(after_delay)),
                );
            } else {
                let spawner = server
                    .systems
                    .npcs
                    .spawns
                    .doodad_spawner_of(ctx.doodad.obj_id);
                phase::despawn_or_delete(ctx);
                if !respawn {
                    return false;
                }
                let Some(spawner) = spawner else {
                    return false;
                };
                tasks::start_func_task(
                    server,
                    &ctx.doodad,
                    caster,
                    FuncTaskKind::Final {
                        respawn: true,
                        delay,
                        respawn_stage: Some(spawner),
                    },
                    tasks::ms(f64::from(delay)),
                );
            }
            true
        }
        FuncTemplate::Growth(f) => {
            // C# `owner.Scale = StartScale / 1000f` writes BaseUnit.Scale, which the get-only
            // Doodad.Scale override never reads: no visible effect (mirrored).
            let mut custom_delay = f64::from(f.delay) / server.systems.npcs.doodads.growth_rate(); // decrease delay
            if doodad_has_matching_climate(ctx) {
                custom_delay *= f64::from(0.73f32);
            }
            let time_left = time_left_with_override(ctx, custom_delay);
            tracing::trace!(
                "DoodadFuncGrowth: Delay {}, StartScale {}, EndScale {}, NextPhase {}",
                f.delay,
                f.start_scale,
                f.end_scale,
                f.next_phase
            );
            tasks::start_func_task(
                server,
                &ctx.doodad,
                caster,
                FuncTaskKind::Growth {
                    next_phase: f.next_phase,
                    end_scale: f.end_scale as f32 / 1000.0,
                },
                tasks::ms(time_left),
            );
            false
        }
        FuncTemplate::Tod(f) => {
            // Deviation from the C#: a `DoodadFuncRatioRespawn` replacement was created at this
            // time of day, so its Tod refers to the next occurrence, which the time-of-day watcher
            // fires on crossing. The `curTime >= Tod` catch-up would send day/night board pairs
            // (Tod 20:00 / 06:00) straight back and forth for the whole evening.
            if ctx.doodad.data().phase.suppress_tod_catch_up {
                return false;
            }
            let cur_time = server.time.get_time();
            if cur_time >= f.tod_as_hours {
                tracing::trace!(
                    "DoodadFuncTod: curTime {cur_time}, Tod {}, OverridePhase {}",
                    f.tod,
                    f.next_phase
                );
                ctx.doodad.data().phase.override_phase = f.next_phase;
                return true;
            }
            false
        }
        FuncTemplate::RatioChange(f) => {
            let mut d = ctx.doodad.data();
            let prev = d.phase.cumulative_phase_ratio;
            let cumulative = prev.saturating_add(f.ratio.max(0));
            d.phase.cumulative_phase_ratio = cumulative;
            if d.phase.phase_ratio >= prev && d.phase.phase_ratio < cumulative {
                d.phase.override_phase = f.next_phase;
                return true; // interrupt the phase functions and switch to NextPhase
            }
            false
        }
        FuncTemplate::RatioRespawn(f) => {
            let spawner = server
                .systems
                .npcs
                .spawns
                .doodad_spawner_of(ctx.doodad.obj_id)
                .filter(|s| s.id > 0);
            let hit = {
                let mut d = ctx.doodad.data();
                let prev = d.phase.cumulative_phase_ratio;
                let cumulative = prev.saturating_add(f.ratio.max(0));
                d.phase.cumulative_phase_ratio = cumulative;
                d.phase.phase_ratio >= prev && d.phase.phase_ratio < cumulative
            };
            if let (true, Some(spawner)) = (hit, spawner) {
                if !ctx.data().exist(f.spawn_doodad_id) {
                    tracing::error!(
                        "DoodadFuncRatioRespawn: Spawn template {} does not exist (spawner={}).",
                        f.spawn_doodad_id,
                        spawner.id
                    );
                    return false;
                }
                let (template_id, func_group_id) = {
                    let d = ctx.doodad.data();
                    (d.template_id, d.func_group_id)
                };
                // The replacement re-enters its phase funcs synchronously inside `spawn`; the
                // guard rejects an unconditional cycle through a source/group already active in
                // this spawner's chain (or an over-deep chain) before the current doodad is
                // despawned. It is dropped when the nested call has finished, also on early
                // return or unwind.
                let _chain = match spawner.enter_ratio_respawn(template_id, func_group_id, f.ratio)
                {
                    Ok(guard) => guard,
                    Err(reject) => {
                        tracing::warn!(
                            spawner = spawner.id,
                            doodad = ctx.doodad.obj_id,
                            template = template_id,
                            group = func_group_id,
                            replacement = f.spawn_doodad_id,
                            repeated = reject.repeated,
                            depth = reject.depth,
                            limit = reject.limit,
                            "DoodadFuncRatioRespawn: replacement chain rejected; keeping the current doodad"
                        );
                        return false;
                    }
                };
                spawner.set_respawn_doodad_template_id(f.spawn_doodad_id);
                spawner.despawn(server, &ctx.doodad);
                if spawner.spawn(server, 0).is_none() {
                    tracing::error!(
                        "DoodadFuncRatioRespawn: Spawn failed for template {} at spawner {}.",
                        f.spawn_doodad_id,
                        spawner.id
                    );
                }
                return true; // Interrupt the PhaseFunc because we swapped the doodad
            }
            false
        }
        FuncTemplate::RequireItem(f) => match ctx.phase_requester(caster) {
            Some(p) => {
                let count = server
                    .systems
                    .npcs
                    .hooks
                    .economy
                    .get()
                    .map_or(0, |h| h.item_count(server, p, f.item_id));
                count <= 0
            }
            None => true,
        },
        FuncTemplate::RequireQuest(f) => match ctx.phase_requester(caster) {
            Some(p) => !server
                .systems
                .npcs
                .hooks
                .quests
                .get()
                .is_some_and(|h| h.has_quest(server, p, f.quest_id)),
            None => false, // caster is not a player, allow execution as we can't check
        },
        FuncTemplate::Pulse(f) => {
            // C# `DoodadFuncPulse.Use`: `if (caster is not null)`. A world spawn runs its entry
            // phase with no caster, so the initial `Pulse(flag = false)` rows only take effect
            // when a phase is entered with a caster (a player's use, or a timer / signal that
            // carries one). The pulse is recorded first, so the group's own trigger sees it.
            if caster.is_none() {
                return false;
            }
            ctx.doodad.data().phase.pulsed = Some(f.flag);
            super::logic::emit(ctx, caster, f.flag);
            false
        }
        // A `DoodadFuncPulseTrigger` fires on the pulse its own group's `DoodadFuncPulse` sent in
        // this phase entry, when the flags match: the Sharpwind switches (group 13161: `Pulse(t)`
        // + `PulseTrigger(t -> final)`) use this to reach their `DoodadFuncFinal` after the use
        // pulse (the trigger rows run after the group's other phase funcs, see
        // `phase::do_phase_funcs`). It does not fire because a player caster was propagated along
        // a signal chain or a timer: Sal Temple's Ice Trap 4751 (`PulseTrigger(t -> B)` /
        // `PulseTrigger(f -> A)`) must stay off after the Ice Machine's false pulse, and Jolly
        // Roger Tower 3382 group 7885 (`Pulse(f)` + `PulseTrigger(t -> 8260)`) must stay put. A
        // pulse received from another doodad is handled by `logic::receive`.
        //
        // **Inferred (no 1.2 server evidence):** the C# AAEmu `DoodadFuncPulseTrigger.Use`
        // returns `true` for a caster that is not a `Character` (the sequence stops) and
        // otherwise, when `Flag` is true and its shared `PulseTriggered` is false, sets
        // `OverridePhase = NextPhase`; only a source `DoodadFuncPulse` with `Flag == true` resets
        // `PulseTriggered`, for its `RelatedIds` targets. That fires a trigger on the first
        // character-caused entry of its group; here the player caster also travels along signals
        // and timers, so every toggle (`A: t -> B`, `B: f -> A`) would bounce back. Neither
        // reading is backed by the 1.2 server; this one is the narrowest that reaches the
        // Sharpwind `Final` and leaves the chains that work on `main` alone.
        FuncTemplate::PulseTrigger(f) => {
            let (pulsed, group) = {
                let d = ctx.doodad.data();
                (d.phase.pulsed, d.func_group_id)
            };
            if f.next_phase <= 0 || pulsed != Some(f.flag) {
                return false;
            }
            let next = super::phase::pulse_trigger_next_phase(ctx, group, f.next_phase);
            ctx.doodad.data().phase.override_phase = next;
            true
        }
        // These describe routing/operations; the logic runs on a received signal.
        FuncTemplate::Logic(_) | FuncTemplate::LogicFamilyProvider(_) => false,
        FuncTemplate::LogicFamilySubscriber(f) => super::logic::subscribe(ctx, caster, f.family_id),
        // PrefabDoodadModel::PlayFlowGraph is client-owned; the phase and visibility packets
        // already cause the 1.2 client to execute these events from its own compact data.
        FuncTemplate::PlayFlowGraph(_) => false,
        FuncTemplate::Respawn(f) => {
            tracing::trace!(
                "DoodadFuncRespawn: MinTime {}, MaxTime {}",
                f.min_time,
                f.max_time
            );
            if let Some(p) = ctx.phase_requester(caster) {
                use_respawn(ctx, p);
            }
            false
        }
        FuncTemplate::ClimateReact(f) => {
            if !doodad_has_matching_climate(ctx) || f.next_phase <= 0 {
                return false;
            }
            ctx.doodad.data().phase.override_phase = f.next_phase;
            true
        }
        FuncTemplate::CraftDirect(f) => override_if_character(ctx, caster, f.next_phase),
        FuncTemplate::Hunger(f) => override_if_character(ctx, caster, f.next_phase),
        FuncTemplate::PuzzleIn(f) => {
            // The r208022 phase handler at 0x3b6c60 passes this selector to model loading.
            ctx.doodad.data().puzzle_group = f.group_id as i32;
            false
        }
        FuncTemplate::PuzzleOut(f) => {
            if ctx.phase_requester(caster).is_none() {
                return false;
            }
            if !f.anim.is_empty() || f.projectile_id != 0 {
                ctx.broadcast(&super::packets::SCDoodadPuzzleScenePacket {
                    obj_id: ctx.doodad.obj_id,
                    projectile_id: f.projectile_id,
                    projectile_delay: f.projectile_delay.max(0) as u32,
                    anim: f.anim.clone(),
                });
            }
            if f.sound_id != 0 {
                ctx.broadcast(&super::packets::SCDoodadSoundPacket {
                    obj_id: ctx.doodad.obj_id,
                    sound_id: f.sound_id,
                });
            }
            tasks::start_puzzle_outcome(ctx, caster, f);
            false
        }
        // A wait, not an action: stay in this phase until period `siege_period_id` ends, then
        // move to `next_phase` (-1 = gone). The boundary is an external event, so the func is
        // passive when the phase is entered; `super::siege_phase` applies the change when the
        // zone's period changes. 3410's unknown `period 0` rule and the `defense` flag are
        // documented there (capture-pending).
        FuncTemplate::SiegePeriod(_) => false,
        FuncTemplate::SpawnGimmick(f) => {
            if let Some(used) = encounter_hooks::all()
                .iter()
                .find_map(|encounter| encounter.spawn_gimmick(server, ctx, f))
            {
                return used;
            }
            if crate::systems::npcs::gimmick::manager::spawn_from_doodad(
                server,
                &ctx.world,
                &ctx.obj(),
                f,
            )
            .is_some()
            {
                if f.next_phase > 0 {
                    ctx.doodad.data().phase.override_phase = f.next_phase;
                    return true;
                }
                if f.next_phase == -1 {
                    phase::despawn_or_delete(ctx);
                    return true;
                }
            }
            false
        }
        FuncTemplate::SpawnMgmt(f) => {
            if let Some(spawns) = server.systems.npcs.spawns.get(ctx.world.id) {
                if spawns.set_management_group(server, f.zone_id, f.group_id, f.spawn) == 0 {
                    tracing::warn!(
                        doodad = ctx.doodad.obj_id,
                        group = f.group_id,
                        zone = f.zone_id,
                        "Doodad spawn management group has no authored SpawnGroupId positions"
                    );
                }
            }
            false
        }
        FuncTemplate::ZoneReact(f) => {
            let zone_id = ctx.doodad.data().transform.zone_id;
            let group = server
                .game_data
                .zones
                .zone_by_key(zone_id)
                .map(|z| z.group_id);
            if group == Some(f.zone_group_id) {
                ctx.doodad.data().phase.override_phase = f.next_phase;
                return true;
            }
            false
        }
        FuncTemplate::Coffer(_) => {
            ctx.doodad.data().phase.to_next_phase = false;
            if let Some(p) = ctx.phase_requester(caster) {
                let (is_coffer, opened_by) = {
                    let d = ctx.doodad.data();
                    (
                        d.coffer.is_some(),
                        d.coffer.as_ref().and_then(|c| c.opened_by),
                    )
                };
                if is_coffer {
                    let char_id = p.data().id;
                    if opened_by == Some(char_id) {
                        super::coffer::close_coffer_doodad(server, p, ctx.doodad.obj_id);
                    } else {
                        super::coffer::open_coffer_doodad(server, p, ctx.doodad.obj_id);
                    }
                }
            }
            false
        }
        FuncTemplate::Clout(f) => {
            use_clout(ctx, caster, f);
            false
        }
        FuncTemplate::WaterVolume(f)
            if encounter_hooks::all()
                .iter()
                .any(|encounter| encounter.water_volume(server, ctx, f)) =>
        {
            false
        }
        FuncTemplate::NaviRemoveTimer(f) => {
            // Inferred from the 1.2 Navi doodads (1219: row 1, 600000 ms). The removal task
            // belongs to this phase, so exiting cancels it and entry starts a fresh delay.
            tasks::start_func_task(
                server,
                &ctx.doodad,
                None,
                FuncTaskKind::Delete,
                tasks::ms(f64::from(f.after)),
            );
            false
        }
        // Phase funcs whose C# `Use` only logs (and returns false).
        FuncTemplate::Animate(_)
        | FuncTemplate::BuyFishItem(_)
        | FuncTemplate::BuyFishModel(_)
        | FuncTemplate::ConsumeChangerModel(_)
        | FuncTemplate::ConsumeChangerModelItem(_)
        | FuncTemplate::ConsumeItem(_)
        | FuncTemplate::ConvertFishItem(_)
        | FuncTemplate::CraftStartCraft(_)
        | FuncTemplate::DeclareSiege(_)
        | FuncTemplate::Exchange(_)
        | FuncTemplate::FishSchool(_)
        | FuncTemplate::HouseFarm(_)
        | FuncTemplate::Parrot(_)
        | FuncTemplate::Sign(_)
        | FuncTemplate::WaterVolume(_) => {
            tracing::trace!("{}", t.func_type());
            false
        }
        // Not a DoodadPhaseFuncTemplate (never returned by GetPhaseFuncTemplate).
        _ => false,
    }
}

/// `DoodadFuncRespawn.Use` for a character caster: a one-off spawner 1 m in front of them.
fn use_respawn(ctx: &DoodadCtx<'_>, player: &crate::world::PlayerRef) {
    let server = ctx.server;
    let pobj = WorldObject::from(player);
    let mut pos = ctx.world.world_pos_rot(&pobj);
    let zone_id = player.data().unit.transform.zone_id;
    // AddDistanceToFront(1f)
    pos.x += -pos.rot_z.sin();
    pos.y += pos.rot_z.cos();
    // C#: GeoData.GetHeight (exception when outside the map; the old height is kept here)
    // (collision service: the ground under the point, static + terrain)
    if let Some(h) =
        crate::systems::ai::collision::ground_height(server, ctx.world.id, pos.x, pos.y, pos.z)
    {
        pos.z = h;
    } else if let Ok(h) =
        crate::systems::npcs::heightmap::world_height(server, &ctx.world, pos.x, pos.y)
    {
        pos.z = h;
    }
    let (template_id, obj_id) = (ctx.doodad.data().template_id, ctx.doodad.obj_id);
    let def = DoodadSpawnerDef {
        base: SpawnerBase {
            id: obj_id,
            unit_id: template_id,
            position: WorldSpawnPosition {
                world_id: ctx.world.template.id,
                zone_id,
                x: pos.x,
                y: pos.y,
                z: pos.z,
                yaw: pos.rot_z,
                pitch: pos.rot_y,
                roll: pos.rot_x,
            },
            ..SpawnerBase::default()
        },
        scale: 1.0,
        count: 1,
        related_ids: None,
        family_id: 0,
        respawn_doodad_template_id: 0,
    };
    let spawner = Arc::new(DoodadSpawner::new(def, Some(ctx.world.id)));
    spawner.spawn(server, 0);
}

/// `DoodadFuncClout.Use`.
/// The effect ids of `effects` that are a `BuffEffect` of a Good (`buffs.kind_id` 1) buff.
pub(crate) fn good_buff_effects(server: &crate::server::GameServer, effects: &[u32]) -> Vec<u32> {
    use crate::data::skills::BuffKind;
    use crate::systems::skills::data::EffectTemplate;
    let skills = &server.game_data.systems.skills;
    effects
        .iter()
        .copied()
        .filter(|&id| match skills.effect_by_type_id(id) {
            Some((_, EffectTemplate::Buff(b))) => b.buff_id.is_some_and(|buff| {
                server
                    .game_data
                    .skills
                    .buffs
                    .get(&buff)
                    .is_some_and(|t| t.kind == BuffKind::Good)
            }),
            _ => false,
        })
        .collect()
}

/// A `use_origin_source` clout of a doodad that an NPC's skill summoned (its owner unit), run
/// with that NPC as the caster. The NPC is the origin source: the area's target relation and
/// its effects' source are the NPC's, not the faction-less doodad's. With the doodad as the
/// caster a Hostile area hit every unit, the summoner included (Miasma's 3884 on the Putrid
/// Demon itself), and a damage effect scaled by the doodad's zero DPS. Such an area also stays
/// at its own doodad: the 4 m doodad lookup around the caster could move a Poison Belch
/// placed under the victim back to whatever prop stood beside the NPC.
pub(crate) fn npc_origin_clout(
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
    f: &DoodadFuncClout,
) -> bool {
    f.use_origin_source
        && caster
            .is_some_and(|c| c.as_npc().is_some() && c.obj_id() == ctx.doodad.data().owner_obj_id)
}

fn use_clout(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>, f: &DoodadFuncClout) {
    tracing::trace!(
        "DoodadFuncClout : Duration {}, Tick {}, BuffId {}, NextPhase {}, AoeShapeId {}",
        f.duration,
        f.tick,
        f.buff_id,
        f.next_phase,
        f.aoe_shape_id
    );
    let server = ctx.server;
    let shape = server
        .game_data
        .systems
        .npcs
        .area_trigger
        .get_area_shape_by_id(f.aoe_shape_id);
    let template_id = ctx.doodad.data().template_id;
    let is_fish_school = super::super::fish_school::is_school_template(template_id);
    let clout = encounters::clout_class(template_id, f.id, f.buff_id);
    let howl_ward = clout == Some(CloutClass::HowlWard);
    let howling_hazard = clout == Some(CloutClass::HowlingHazard);
    // The poison puddles and tombstones are anchored at the encounter's
    // summoned doodad. Timed phases can arrive without an explicit caster.
    let hadir_hazard = clout == Some(CloutClass::HadirHazard);
    let frozen_zone = clout == Some(CloutClass::FrozenZone);
    // The Cellar flame follows the brazier's phase, even when the bucket is used
    // beside a different prop. Phase changes must remove the previous fire area.
    let palace_flame = clout == Some(CloutClass::PalaceFlame);
    // An NPC skill summoned this doodad, and the clout uses that NPC as its source
    // (Kroloal Cradle's Miasma, Poison Belch and Toxic Globe). See `npc_origin_clout`.
    // Encounter areas above keep their own caster rules. The Burnt Castle Howl ward is
    // summoned by its marker NPC and casts protection from the doodad.
    let npc_origin = !(is_fish_school
        || howl_ward
        || howling_hazard
        || hadir_hazard
        || frozen_zone
        || palace_flame)
        && npc_origin_clout(ctx, caster, f);
    let owner = if f.use_origin_source
        && !npc_origin
        && !is_fish_school
        && !howl_ward
        && !howling_hazard
        && !hadir_hazard
        && !palace_flame
        && ctx.doodad.core().is_visible()
    {
        // `WorldManager.GetAround<Doodad>(caster, 4f)`: a doodad within 4 m of the caster
        // anchors the trigger. A doodad initialised before its `Spawn` (the summon path, also
        // a delayed `SpawnDoodad`) is not visible yet and keeps its own transform: the C#
        // anchor moved the area away from the summon, which left the caster outside it when a
        // planted doodad stood next to them (Comet's Boon 5017: caster 2.002 m off the clout's
        // 2.0 m half-length).
        caster
            .and_then(|c| {
                let cpos = ctx.world.world_pos_rot(c);
                ctx.world
                    .around(c, |o| o.as_doodad().cloned())
                    .into_iter()
                    .find(|d| {
                        let p = ctx.world.world_pos_rot(&WorldObject::from(d));
                        let (dx, dy) = (p.x - cpos.x, p.y - cpos.y);
                        dx * dx + dy * dy <= 16.0
                    })
            })
            .unwrap_or_else(|| ctx.doodad.clone())
    } else {
        // Chumming from a ship must not attach the school's aura to a nearby fitting.
        // The school owns the area and releases it when its chummed phase expires.
        ctx.doodad.clone()
    };
    let caster = if hadir_hazard || frozen_zone {
        let owner_obj_id = ctx.doodad.data().owner_obj_id;
        ctx.world
            .object(owner_obj_id)
            .filter(|o| o.as_npc().is_some())
    } else {
        caster.cloned()
    };
    if frozen_zone && caster.is_none() {
        return;
    }
    let supplied_unit_id = caster
        .as_ref()
        .filter(|unit| unit.is_unit())
        .map(WorldObject::obj_id);
    let trigger_caster = match caster {
        Some(c) if hadir_hazard || frozen_zone => c,
        Some(c)
            if c.as_player().is_some()
                || npc_origin
                || (howling_hazard && c.as_npc().is_some()) =>
        {
            c
        }
        _ => ctx.obj(),
    };
    let inside_buff_id = server
        .game_data
        .skills
        .buffs
        .contains_key(&f.buff_id)
        .then_some(f.buff_id);
    let (func_group_id, owner_obj_id) = {
        let doodad = ctx.doodad.data();
        (doodad.func_group_id, doodad.owner_obj_id)
    };
    let mut trigger = AreaTrigger::default();
    trigger.shape = shape;
    trigger.owner = Some(owner.clone());
    trigger.caster = Some(trigger_caster.clone());
    trigger.func_group_id = func_group_id;
    trigger.target_relation = if howl_ward {
        area_trigger::skill_target_relation::ANY
    } else {
        f.target_relation
    };
    trigger.player_sphere = howl_ward;
    trigger.inside_buff_id = inside_buff_id;
    // Miasma and spear injuries last beyond the contact that applies them, and so
    // does a Fire Pillar hit. A unit inside Miasma or a spear window keeps its buff.
    // Fire Pillar (300 ms) hits once per entry, as in the C#: each flame phase of a
    // jet is a new trigger, so a player who stays on the grate is hit once per burst.
    // The Cradle stair barriers 5423 / 5424 (clouts 1266 / 1267, 200 ms) root with
    // Impaled 4351 for its 2 s like the spears' Injury.
    let reapplied_contact = encounters::clout_reapplies_contact(f.id);
    trigger.contact_buff = reapplied_contact || encounters::is_contact_buff(f.buff_id);
    trigger.reapply_while_inside = reapplied_contact;
    // The Cradle pool hazards: Toxicity (green pool), Cursed Blood (red pools),
    // Poison (stepping stone pools) and Lava (Arkanis's room).
    trigger.requires_submersion =
        encounters::clout_requires_submersion(&ctx.world.template.name, f.buff_id);
    // The linked effects are independent of the inside buff. For unit-owned clouts, require
    // the owner's faction and a sided relation: Any admits allies, and Hostile with a
    // faction-less caster admits even the summoner. A placed environmental hazard can pulse
    // without a unit source. Restrict an explicitly supplied unit that was lost as a source
    // too, since the doodad cannot tell its side.
    use area_trigger::skill_target_relation::{FRIENDLY, HOSTILE};
    let uses_owner_relation = trigger_caster.obj_id() == owner_obj_id
        && matches!(trigger.target_relation, FRIENDLY | HOSTILE)
        && crate::systems::social::nation::faction_exists(
            server,
            trigger_caster.with_base_unit(|unit| unit.faction_id()),
        );
    let lost_unit_source = supplied_unit_id.is_some_and(|id| id != trigger_caster.obj_id());
    trigger.effects_per_buff = match inside_buff_id {
        Some(_) => HashMap::from([(f.buff_id, f.effects.clone())]),
        None if (owner_obj_id != 0 && !uses_owner_relation) || lost_unit_source => {
            HashMap::from([(0, good_buff_effects(server, &f.effects))])
        }
        None => HashMap::from([(0, f.effects.clone())]),
    };
    trigger.tick_rate = f.tick;
    let trigger = Arc::new(trigger);
    area_trigger::add_area_trigger(server, trigger.clone());
    // The spear's active window is 100 ms, shorter than the 200 ms area tick.
    // Test contact the instant a placed trap enters its active phase.
    if f.duration > 0
        && (f.duration as u128) <= area_trigger::TICK_DELAY.as_millis()
        && ctx.doodad.core().is_visible()
    {
        trigger.tick(server);
    }
    if f.duration > 0 {
        // Deviation: the C# task (`new DoodadFuncCloutTask(areaTrigger.Caster,
        // areaTrigger.Owner, ...)`) acts on the trigger owner, which with UseOriginSource is any
        // doodad near the caster. `NextPhase` is a func group of this doodad's template, so the
        // borrowed doodad was moved into a foreign phase (and deleted by its Final): e.g. the
        // summoned doodad 5007 of NPC 10119 took the Amber Boulder (4252, quest 3447) with it.
        // The borrowed doodad stays the area origin; the phase change is this doodad's.
        tasks::start_func_task(
            server,
            &ctx.doodad,
            Some(&trigger_caster),
            FuncTaskKind::Clout {
                next_phase: f.next_phase,
                trigger,
            },
            // Howl takes eight seconds to cast. Keep the ward through the
            // impact even if the auxiliary spawn runs at the start of the cast.
            tasks::ms(f64::from(if howl_ward {
                f.duration.max(9_000)
            } else {
                f.duration
            })),
        );
    }
}
