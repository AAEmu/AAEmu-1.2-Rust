//! Logic links and family pulses.
//!
//! The authored network is addressed by the level-design **family id** of each doodad: the
//! `Family_Id` of the 10.x client level editor's `doodad.xml` (`ArcheAge Returns 10.0.2.13 r575`,
//! `.../instance_cuttingwind_deadmine/cells/000_000/editor/02_01/doodad.xml`, 6437 B, MD5
//! 74CFA418149294B9A36E4675CC119299; the file is in neither r208022 nor r223748, only its pak
//! catalogue entry is on hand), loaded into `Doodad::family_id` from the spawn row's `FamilyId`.
//! The level is the same as 1.2 (identical heightmap / object / brush / entities / navmesh MD5
//! between r223748 and r575; the 1.2 prefab lake polygon contains all 11 water placements; five
//! old server rows stood at the stream positions). The two function tables read as follows
//! (Sharpwind Mines is the complete example, all ids matched against the editor placements;
//! **Inferred (no 1.2 server evidence):** that every other chain reads them the same way - only
//! Sharpwind's graph closes with exactly one carrier per id, and the template-level reading `main`
//! used, provider family == subscriber family, is incoherent for it):
//!
//! * `DoodadFuncLogicFamilyProvider { family_id }` — when this doodad emits, the doodads whose
//!   `family_id` equals `family_id` receive the signal (output route).
//! * `DoodadFuncLogicFamilySubscriber { family_id }` — this doodad's `DoodadFuncLogic` accepts
//!   signals only from the doodad whose `family_id` equals `family_id` (input route); each
//!   subscribed family is one input, a family that never signalled counts as `false`.
//! * `DoodadFuncLogic { operation_id, delay_id }` — combines the accepted inputs (operations
//!   1 AND, 2 NAND, 3 OR, 4 XOR, 5/6 NOT/NOR, 7 XNOR, as the QA gate templates 1754-1773 are
//!   named) and emits the result after the delay of `delay_id` (1 s / 5 s / 10 s for ids 1 / 2 /
//!   3, evidenced by the same QA names; ids 4..8 are inferred, see `logic_delay_ms`).
//! * `DoodadFuncPulse { flag }` — emits `flag` when a phase is entered with a caster (the C# ran
//!   it only with a caster), to the provider families and to the spawner's `RelatedIds`.
//! * `DoodadFuncPulseTrigger { flag, next_phase }` — a received signal equal to `flag` advances
//!   the phase, and so does the pulse of the same flag that the trigger's own group sent in this
//!   phase entry (the Sharpwind switches). Entering a phase through a signal or a timer fires
//!   nothing by itself.
//!
//! Sharpwind (`instance_cuttingwind_deadmine`, zone 262; editor families 50001-50008):
//! switches 5064 (50002) and 5068 (50003) provide 50005, the family of gate 5070, whose AND over
//! its subscribers 50002/50003 emits to 50006 (gate 5071) and 50007 (streams 5072); gate 5071
//! ANDs switch 5069 (50004) with gate 5070 (50005) and emits to 50001 (lake 5063 → water rises)
//! and 50008 (streams 5073).
//!
//! Client-created wires (`DoodadFuncLogic` link packets) and the spawner's `RelatedIds` are
//! explicit routes: they address their target directly and bypass the subscriber-family filter.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::{phase, runtime::DoodadCtx, templates::FuncTemplate};
use crate::models::doodad::doodad_owner_type;
use crate::server::GameServer;
use crate::world::{DoodadRef, PlayerRef, WorldObject};

thread_local! { static PULSE_PATH: RefCell<HashSet<usize>> = RefCell::new(HashSet::new()); }

struct PulseScope(bool);
impl Drop for PulseScope {
    fn drop(&mut self) {
        if self.0 {
            PULSE_PATH.with(|path| path.borrow_mut().clear());
        }
    }
}

fn identity(d: &DoodadRef) -> usize {
    Arc::as_ptr(d) as usize
}

fn families(ctx: &DoodadCtx<'_>, doodad: &DoodadRef, provider: bool) -> HashSet<u32> {
    let group = doodad.data().func_group_id;
    ctx.data()
        .phase_funcs(group)
        .iter()
        .filter_map(
            |pf| match ctx.data().phase_func_template(pf.func_id, &pf.func_type) {
                Some(FuncTemplate::LogicFamilyProvider(f)) if provider => Some(f.family_id),
                Some(FuncTemplate::LogicFamilySubscriber(f)) if !provider => Some(f.family_id),
                _ => None,
            },
        )
        .collect()
}

/// One doodad's owner domain (never holds two object locks at once).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Domain {
    zone: u32,
    owner: u32,
    owner_type: u8,
}

impl Domain {
    fn of(d: &DoodadRef) -> Self {
        let d = d.data();
        Self {
            zone: d.transform.zone_id,
            owner: d.owner_id,
            owner_type: d.owner_type,
        }
    }

    fn matches(self, other: Self) -> bool {
        self == other && (self.owner_type == doodad_owner_type::SYSTEM || self.owner != 0)
    }
}

/// The delay of a `DoodadFuncLogic` (`delay_id`); `None` = unknown descriptor.
///
/// Ids 1 / 2 / 3 = 1 s / 5 s / 10 s are the only ones in shipped rows (74 `doodad_func_logics`
/// rows: 61 / 6 / 7) and are evidenced by the QA gate template names 1754-1773 ("(1초)",
/// "(5초)", "(10초)"). **Inferred (no 1.2 server evidence):** ids 4..8 = 30 s, 1 min, 5 min,
/// 10 min, 30 min; no shipped row uses them and no `enum_doodad_logic_delays` table exists in the
/// r208022, r223748, AAEmu CN or MailRU r243378 compact DB.
fn logic_delay_ms(delay_id: u32) -> Option<u64> {
    Some(match delay_id {
        1 => 1_000,
        2 => 5_000,
        3 => 10_000,
        // Inferred (no 1.2 server evidence), see above.
        4 => 30_000,
        5 => 60_000,
        6 => 300_000,
        7 => 600_000,
        8 => 1_800_000,
        _ => return None,
    })
}

/// Client-created wires require both objects to belong to the caller, in the same world and
/// within reach. The live handle identity prevents an old wire addressing a recycled ID.
pub fn link(server: &GameServer, player: &PlayerRef, provider_id: u32, subscriber_id: u32) -> bool {
    let Some(world) = server.world.world_of(&WorldObject::from(player)) else {
        return false;
    };
    let (Some(provider), Some(subscriber)) =
        (world.doodad(provider_id), world.doodad(subscriber_id))
    else {
        return false;
    };
    if provider_id == subscriber_id {
        return false;
    }
    let player_pos = world.world_pos_rot(&WorldObject::from(player));
    for doodad in [&provider, &subscriber] {
        let (owner, deleted) = {
            let d = doodad.data();
            (d.owner_id, d.phase.deleted)
        };
        if owner != player.character_id || deleted {
            return false;
        }
        let p = world.world_pos_rot(&WorldObject::from(doodad));
        if (p.x - player_pos.x).powi(2)
            + (p.y - player_pos.y).powi(2)
            + (p.z - player_pos.z).powi(2)
            > 400.0
        {
            return false;
        }
    }
    let ctx = DoodadCtx::new(server, world.clone(), provider.clone());
    let group = subscriber.data().func_group_id;
    let is_logic = ctx
        .data()
        .phase_funcs(group)
        .iter()
        .any(|pf| pf.func_type == "DoodadFuncLogic");
    if families(&ctx, &provider, true).is_empty()
        || (!is_logic && families(&ctx, &subscriber, false).is_empty())
    {
        return false;
    }
    provider
        .data()
        .phase
        .logic_links
        .insert(subscriber_id, identity(&subscriber));
    let output = provider.data().phase.logic_output;
    if let Some(value) = output {
        receive(
            &ctx,
            &subscriber,
            Some(&WorldObject::from(player)),
            value,
            true,
        );
    }
    true
}

pub fn clear_links(ctx: &DoodadCtx<'_>) {
    let active = {
        let d = ctx.doodad.data();
        !d.phase.logic_links.is_empty()
            || !d.phase.logic_inputs.is_empty()
            || d.phase.logic_output.is_some()
            || d.phase.pending_output.is_some()
    };
    if !active {
        return;
    }
    if ctx.doodad.data().phase.logic_output.is_some() {
        emit(ctx, None, false);
    }
    for provider in ctx.world.all_doodads() {
        provider
            .data()
            .phase
            .logic_links
            .retain(|id, handle| *id != ctx.doodad.obj_id || *handle != identity(&ctx.doodad));
    }
    let mut d = ctx.doodad.data();
    d.phase.logic_links.clear();
    d.phase.logic_inputs.clear();
    d.phase.logic_output = None;
    // A pending output must not emit after the cleanup; its task no longer matches.
    d.phase.pending_output = None;
    d.phase.pending_output_generation = super::tasks::next_generation();
}

/// Emit `value` to every route of the doodad: explicit wires, the `family_id`s named by its
/// `DoodadFuncLogicFamilyProvider` rows, and the spawner's `RelatedIds` templates.
///
/// Client wires and the spawner's `RelatedIds` are *explicit* routes: they were authored for
/// this source (a wire between two handles; a spawn row naming the target template), so they
/// skip the subscriber-family source filter of `receive`. Only the family-routed signals go
/// through that filter.
pub fn emit(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>, value: bool) {
    let root = PULSE_PATH.with(|path| {
        let mut path = path.borrow_mut();
        let root = path.is_empty();
        path.insert(identity(&ctx.doodad));
        root
    });
    let _scope = PulseScope(root);
    ctx.doodad.data().phase.logic_output = Some(value);
    let links = ctx.doodad.data().phase.logic_links.clone();
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for (id, handle) in links {
        if let Some(target) = ctx.world.doodad(id).filter(|d| identity(d) == handle) {
            if seen.insert(identity(&target)) {
                targets.push((target, true));
            }
        }
    }
    let provided = families(ctx, &ctx.doodad, true);
    if !provided.is_empty() {
        let domain = Domain::of(&ctx.doodad);
        for target in ctx.world.all_doodads() {
            if Arc::ptr_eq(&target, &ctx.doodad) {
                continue;
            }
            let (family, deleted) = {
                let d = target.data();
                (d.family_id, d.phase.deleted)
            };
            if family != 0
                && !deleted
                && provided.contains(&family)
                && domain.matches(Domain::of(&target))
                && seen.insert(identity(&target))
            {
                targets.push((target, false));
            }
        }
    }
    if let Some(spawner) = ctx
        .server
        .systems
        .npcs
        .spawns
        .doodad_spawner_of(ctx.doodad.obj_id)
    {
        if let Some(related) = &spawner.related_ids {
            for target in ctx
                .world
                .around(&ctx.obj(), |o| o.as_doodad().cloned())
                .into_iter()
                .filter(|d| related.contains(&d.data().template_id))
            {
                if seen.insert(identity(&target)) {
                    // The spawn row names the target template: an authored wire like a client
                    // wire. The shipped rows carry no `FamilyId`, so the family filter would
                    // drop the first signal into every target that has a subscriber row
                    // (Howling Abyss 2 6345 -> 6346).
                    targets.push((target, true));
                }
            }
        }
    }
    for (target, explicit) in targets {
        let fresh = PULSE_PATH.with(|path| {
            let mut path = path.borrow_mut();
            path.len() < 256 && path.insert(identity(&target))
        });
        if fresh {
            receive(ctx, &target, caster, value, explicit);
        }
    }
}

fn evaluate(operation: u32, inputs: impl Iterator<Item = bool>) -> Option<bool> {
    let inputs: Vec<_> = inputs.collect();
    if inputs.is_empty() {
        return Some(false);
    }
    let all = inputs.iter().all(|v| *v);
    let any = inputs.iter().any(|v| *v);
    let odd = inputs.iter().filter(|v| **v).count() % 2 == 1;
    Some(match operation {
        1 => all,
        2 => !all,
        3 => any,
        4 => odd,
        5 | 6 => !any,
        7 => !odd,
        _ => return None,
    })
}

/// Deliver one signal from `source` to `target`.
///
/// An `explicit` signal travels a wire authored for exactly this pair (a client-created link or
/// an entry of the spawner's `RelatedIds`) and skips the family filter. A family-routed signal
/// (`explicit == false`) reaches a target with `DoodadFuncLogicFamilySubscriber` rows only when
/// the source's `family_id` is one it subscribes to. `DoodadFuncLogic` targets evaluate their
/// inputs and emit after the authored delay; other targets run a matching
/// `DoodadFuncPulseTrigger`.
pub(crate) fn receive(
    source: &DoodadCtx<'_>,
    target: &DoodadRef,
    caster: Option<&WorldObject>,
    value: bool,
    explicit: bool,
) {
    let Some(_operation) = phase::Operation::enter(target) else {
        if !target.data().phase.deleted {
            super::tasks::schedule_pulse(source, target, caster, value, explicit);
        }
        return;
    };
    let ctx = DoodadCtx::new(source.server, source.world.clone(), target.clone());
    let group = target.data().func_group_id;
    let funcs = ctx.data().phase_funcs(group);
    let subscribed = families(&ctx, target, false);
    let key = if !explicit && !subscribed.is_empty() {
        let family = source.doodad.data().family_id;
        if family == 0 || !subscribed.contains(&family) {
            return;
        }
        family as usize
    } else {
        identity(&source.doodad)
    };
    target.data().phase.logic_inputs.insert(key, value);

    let logic = funcs.iter().find_map(|pf| {
        match ctx.data().phase_func_template(pf.func_id, &pf.func_type) {
            Some(FuncTemplate::Logic(f)) => Some(f.clone()),
            _ => None,
        }
    });
    if let Some(f) = logic {
        let Some(delay) = logic_delay_ms(f.delay_id) else {
            tracing::warn!(
                doodad = target.obj_id,
                delay = f.delay_id,
                "Unsupported DoodadFuncLogic delay descriptor; refusing to substitute immediate behavior"
            );
            return;
        };
        let output = {
            let d = target.data();
            if !explicit && !subscribed.is_empty() {
                let mut families: Vec<u32> = subscribed.into_iter().collect();
                families.sort_unstable();
                evaluate(
                    f.operation_id,
                    families.into_iter().map(|family| {
                        d.phase
                            .logic_inputs
                            .get(&(family as usize))
                            .copied()
                            .unwrap_or(false)
                    }),
                )
            } else {
                evaluate(f.operation_id, d.phase.logic_inputs.values().copied())
            }
        };
        if let Some(output) = output {
            // The pending result (if any) is what the next change is compared against; the last
            // emitted value only matters when nothing is pending. Each scheduled output gets its
            // own generation so a replaced one cannot emit after its delay.
            let generation = {
                let mut d = target.data();
                let last = d.phase.pending_output.or(d.phase.logic_output);
                if last == Some(output) {
                    None
                } else if d.phase.logic_output == Some(output) {
                    // The inputs returned to the value that was already emitted while a different
                    // one was pending: cancel that output (its task no longer matches) instead of
                    // emitting the same value again, which would run the downstream triggers twice.
                    d.phase.pending_output = None;
                    d.phase.pending_output_generation = super::tasks::next_generation();
                    None
                } else {
                    let generation = super::tasks::next_generation();
                    d.phase.pending_output = Some(output);
                    d.phase.pending_output_generation = generation;
                    Some(generation)
                }
            };
            if let Some(generation) = generation {
                super::tasks::schedule_logic_emit(
                    &ctx,
                    caster,
                    output,
                    generation,
                    Duration::from_millis(delay),
                );
            }
        }
        return;
    }
    for pf in funcs {
        if let Some(FuncTemplate::PulseTrigger(f)) =
            ctx.data().phase_func_template(pf.func_id, &pf.func_type)
        {
            if f.flag == value && f.next_phase > 0 {
                phase::do_change_phase(&ctx, caster.or(Some(&source.obj())), f.next_phase);
                return;
            }
        }
    }
}

/// A subscriber adopts already active sources, independently of spawn file order: every doodad
/// whose `family_id` is `family` and that has an output replays it into this doodad.
pub fn subscribe(ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>, family: u32) -> bool {
    let group = ctx.doodad.data().func_group_id;
    let domain = Domain::of(&ctx.doodad);
    for provider in ctx.world.all_doodads() {
        if Arc::ptr_eq(&provider, &ctx.doodad) {
            continue;
        }
        let (provider_family, output, deleted) = {
            let d = provider.data();
            (d.family_id, d.phase.logic_output, d.phase.deleted)
        };
        if deleted || provider_family != family || !domain.matches(Domain::of(&provider)) {
            continue;
        }
        let Some(output) = output else {
            continue;
        };
        let source = DoodadCtx::new(ctx.server, ctx.world.clone(), provider);
        receive(&source, &ctx.doodad, caster, output, false);
        if ctx.doodad.data().func_group_id != group {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authored_logic_operations_cover_true_false_and_mixed_inputs() {
        for (op, both, mixed, neither) in [
            (1, true, false, false),
            (2, false, true, true),
            (3, true, true, false),
            (4, false, true, false),
            (5, false, false, true),
            (6, false, false, true),
            (7, true, false, true),
        ] {
            assert_eq!(evaluate(op, [true, true].into_iter()), Some(both));
            assert_eq!(evaluate(op, [true, false].into_iter()), Some(mixed));
            assert_eq!(evaluate(op, [false, false].into_iter()), Some(neither));
        }
        assert_eq!(evaluate(99, [true].into_iter()), None);
    }

    /// Ids 1..3 follow the QA gate template names; 4..8 are the inferred table (no evidence).
    #[test]
    fn logic_delay_table_follows_the_qa_names_and_the_inferred_ids() {
        assert_eq!(logic_delay_ms(1), Some(1_000));
        assert_eq!(logic_delay_ms(2), Some(5_000));
        assert_eq!(logic_delay_ms(3), Some(10_000));
        assert_eq!(logic_delay_ms(4), Some(30_000));
        assert_eq!(logic_delay_ms(5), Some(60_000));
        assert_eq!(logic_delay_ms(6), Some(300_000));
        assert_eq!(logic_delay_ms(7), Some(600_000));
        assert_eq!(logic_delay_ms(8), Some(1_800_000));
        assert_eq!(logic_delay_ms(0), None);
        assert_eq!(logic_delay_ms(9), None);
    }
}
