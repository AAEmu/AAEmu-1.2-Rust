//! Period-driven doodad phase funcs: `DoodadFuncSiegePeriod` (WP-S3).
//!
//! The authored rows (`doodad_func_siege_periods`) only make sense as "stay in this phase until
//! period N ends, then go to `NextPhase`" (-1 = gone). INFERRED (strong) from the chains:
//!
//! * 4307 (siege declaration camp): 10581 `period 2 -> 10582`, 10582 `period 3 -> 10779`,
//!   10779 `period 4 -> -1`;
//! * 3304 (put-down declaration backpack): 7627 `period 2 -> 7631`, 7631 `period 3 -> -1`;
//! * 3410 (defender revive safe zone): 8173 `period 0 -> 8238`, 8238 `period 3 -> 8173`.
//!
//! The 4307/3304 camps are placed during the declare period (2): a "when period 2 starts"
//! reading would fire immediately and drop the whole story, while "until period 2 ends" walks
//! declaration -> siege position -> gone. The C# applied every destination on any character use,
//! which made the phase-only 3410 groups recurse; here the boundary is an external event.
//!
//! UNKNOWN (capture-pending):
//!
//! * 3410's `period 0` func on the down group. Period 0 is "no dominion" and never ends while a
//!   siege exists. Safe default: `down -> up` when the warmup begins (`current == 3`), so the
//!   safe zone is up during warmup and down during the siege (func 5). It never fires for a
//!   `previous == 0` change alone: period 0 ended if the territory became owned, not if a
//!   siege period started. The `defense` flag (3410 only) is not used: its meaning is unknown
//!   and 4307/3304 do not set it.
//! * 4307's authored 10-minute lifetime vs the 90-minute siege. The phase chain wins here;
//!   the authored `min_time` is not applied.
//! * Restart catch-up: a doodad reloaded in a mid-cycle phase whose period already ended waits
//!   for the next transition (the first observation of a zone is silent, as in
//!   `CalendarRuntime`).
//!
//! The calendar tick has no subscription point another subsystem may use, so this watcher polls
//! the read-only [`current_period`](crate::systems::housing::dominion::period::current_period)
//! every [`POLL_INTERVAL`]. It keeps an index of the doodads whose current phase has a
//! `SiegePeriod` func (updated on create, phase change and delete) instead of scanning every
//! world object, and skips a tick entirely while no source can change a period. A skipped
//! transition (the phase operation was busy) is left unrecorded and retried on the next tick.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::Utc;

use super::data::DoodadData;
use super::phase::{self, ExternalPhaseChange};
use super::runtime::DoodadCtx;
use super::templates::FuncTemplate;
use crate::server::GameServer;
use crate::systems::housing::dominion::calendar::{self, PERIOD_WARMUP};
use crate::systems::housing::dominion::period;
use crate::systems::housing::dominion::state::SiegeStatus;
use crate::world::{DoodadRef, WorldInstance, WorldObject};

/// How often the watcher compares the zones' periods with the last ones it saw.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The last period observed per zone group, and the doodads whose current phase waits for a
/// period boundary.
///
/// `periods` has no entry before the first observation: the first period change after a restart
/// is not mistaken for an event that happened while the server was down.
#[derive(Debug, Default)]
pub struct Watcher {
    periods: Mutex<BTreeMap<u32, u8>>,
    /// `obj_id -> handle` of the live doodads whose current phase has a `SiegePeriod` func.
    doodads: Mutex<BTreeMap<u32, DoodadRef>>,
}

impl Watcher {
    fn lock_periods(&self) -> MutexGuard<'_, BTreeMap<u32, u8>> {
        self.periods.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_doodads(&self) -> MutexGuard<'_, BTreeMap<u32, DoodadRef>> {
        self.doodads.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last period observed for a zone group.
    pub fn observed(&self, zone_group_id: u32) -> Option<u8> {
        self.lock_periods().get(&zone_group_id).copied()
    }

    /// Records a period without applying any phase change.
    pub fn record(&self, zone_group_id: u32, period: u8) {
        self.lock_periods().insert(zone_group_id, period);
    }

    /// Drops every period observation (a disabled period source or a test).
    pub fn clear(&self) {
        self.lock_periods().clear();
    }

    /// Adds or removes `doodad` from the boundary index according to its current phase.
    /// Called after a create and after every func-group change.
    pub fn refresh(&self, server: &GameServer, doodad: &DoodadRef) {
        let group = doodad.data().func_group_id;
        let data = &server.game_data.systems.npcs.doodad;
        let mut doodads = self.lock_doodads();
        if has_siege_period_func(data, group) {
            doodads.insert(doodad.obj_id, doodad.clone());
        } else {
            doodads.remove(&doodad.obj_id);
        }
    }

    /// Forgets a deleted doodad.
    pub fn untrack(&self, obj_id: u32) {
        self.lock_doodads().remove(&obj_id);
    }

    fn candidates(&self) -> Vec<DoodadRef> {
        self.lock_doodads().values().cloned().collect()
    }
}

/// A doodad that currently sits in a phase with at least one `SiegePeriod` func, with the world
/// that owns it.
type SiegeDoodad = (Arc<WorldInstance>, DoodadRef);
type ZoneDoodads = BTreeMap<u32, Vec<SiegeDoodad>>;

/// The outcome of applying one observed period change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeriodChange {
    /// Number of doodads that actually changed phase.
    pub changed: usize,
    /// A doodad was busy with another phase operation; the caller must retry the transition.
    pub busy: bool,
}

fn has_siege_period_func(data: &DoodadData, func_group_id: u32) -> bool {
    data.phase_funcs(func_group_id).iter().any(|pf| {
        matches!(
            data.phase_func_template(pf.func_id, &pf.func_type),
            Some(FuncTemplate::SiegePeriod(_))
        )
    })
}

/// The zone group of a doodad: its transform's zone key, else the zone key of its position.
fn zone_group_of(server: &GameServer, world: &WorldInstance, doodad: &DoodadRef) -> Option<u32> {
    let (zone_key, x, y) = {
        let d = doodad.data();
        (d.transform.zone_id, d.transform.x, d.transform.y)
    };
    let zone_key = if zone_key > 0 {
        zone_key
    } else {
        world.template.zone_id_at(x, y)
    };
    let zone = server.game_data.zones.zone_by_key(zone_key)?;
    (zone.group_id > 0).then_some(zone.group_id)
}

/// Whether a period source exists at all. Without the calendar, only an unfinished explicit
/// schedule can change a period; without either, the watcher has nothing to compare.
fn any_period_source(server: &GameServer) -> bool {
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    if settings.enabled {
        return true;
    }
    let now = Utc::now().timestamp();
    server.systems.housing.dominions.all().iter().any(|state| {
        state
            .siege
            .as_ref()
            .is_some_and(|siege| siege.status != SiegeStatus::Finished && now < siege.ends_at)
    })
}

/// The index's candidate doodads grouped by territory zone.
fn index_by_zone(server: &GameServer, watcher: &Watcher) -> ZoneDoodads {
    let data = &server.game_data.systems.npcs.doodad;
    let mut by_zone: ZoneDoodads = BTreeMap::new();
    let mut stale = Vec::new();
    for doodad in watcher.candidates() {
        let (func_group_id, deleted) = {
            let d = doodad.data();
            (d.func_group_id, d.phase.deleted)
        };
        if deleted || !has_siege_period_func(data, func_group_id) {
            stale.push(doodad.obj_id);
            continue;
        }
        let Some(world) = server.world.world_of(&WorldObject::from(&doodad)) else {
            continue;
        };
        let Some(zone) = zone_group_of(server, &world, &doodad) else {
            continue;
        };
        by_zone.entry(zone).or_default().push((world, doodad));
    }
    for obj_id in stale {
        watcher.untrack(obj_id);
    }
    by_zone
}

/// Compares every indexed zone's current period with the last observed one and applies the funcs
/// of the periods that ended. Returns the number of applied phase changes.
pub fn tick(server: &GameServer) -> usize {
    if !any_period_source(server) {
        return 0;
    }
    let watcher = &server.systems.npcs.doodads.siege_phase;
    let by_zone = index_by_zone(server, watcher);
    if by_zone.is_empty() {
        return 0;
    }
    let mut transitions = Vec::new();
    {
        let mut periods = watcher.lock_periods();
        for zone in by_zone.keys().copied() {
            let Some(current) = period::current_period(server, zone) else {
                continue;
            };
            match periods.get(&zone).copied() {
                None => {
                    periods.insert(zone, current);
                }
                Some(previous) if previous == current => {}
                Some(previous) => transitions.push((zone, previous, current)),
            }
        }
    }
    let mut applied = 0;
    for (zone, previous, current) in transitions {
        let outcome = apply_with(server, &by_zone, zone, previous, current);
        applied += outcome.changed;
        // A busy doodad leaves the transition pending: the next tick re-applies it. Doodads that
        // already moved are skipped by the expected-group check, so a retry is idempotent.
        if !outcome.busy {
            watcher.record(zone, current);
        }
    }
    applied
}

/// Applies one observed period change `previous -> current` to the indexed doodads of a zone
/// group. The phase func fires when a period ends; skipped periods are walked in calendar order
/// (2 -> 4 ends 2 and 3), so a missed observation cannot strand a chain. The 3410 `period 0`
/// rule is folded in as the documented default and fires only when warmup begins.
pub fn apply_period_change(
    server: &GameServer,
    zone_group_id: u32,
    previous: u8,
    current: u8,
) -> PeriodChange {
    if !any_period_source(server) {
        return PeriodChange::default();
    }
    let by_zone = index_by_zone(server, &server.systems.npcs.doodads.siege_phase);
    apply_with(server, &by_zone, zone_group_id, previous, current)
}

fn apply_with(
    server: &GameServer,
    by_zone: &ZoneDoodads,
    zone_group_id: u32,
    previous: u8,
    current: u8,
) -> PeriodChange {
    let Some(doodads) = by_zone.get(&zone_group_id) else {
        return PeriodChange::default();
    };
    let mut total = PeriodChange::default();
    for ended in ended_periods(previous, current) {
        let step = apply_step(server, doodads, ended);
        total.changed += step.changed;
        total.busy |= step.busy;
    }
    if current == PERIOD_WARMUP {
        // UNKNOWN (capture-pending): 3410's `period 0 -> up`. Treated as "the next warmup
        // starts", the documented safe default; `defense` is not used. It never fires for a
        // `previous == 0` ownership change alone.
        let step = apply_step(server, doodads, calendar::PERIOD_NONE);
        total.changed += step.changed;
        total.busy |= step.busy;
    }
    total
}

/// The periods that ended between two observations, in calendar order. Period 0 never counts:
/// the 3410 period-0 func is the separate warmup default above.
fn ended_periods(previous: u8, current: u8) -> Vec<u8> {
    if previous == 0 || previous == current {
        return Vec::new();
    }
    if previous < current {
        (previous..current)
            .filter(|period| (1..=calendar::PERIOD_PEACE).contains(period))
            .collect()
    } else {
        // A wrap (peace -> auction) or an explicit-siege override: only the period that was
        // current ended in this direction.
        vec![previous]
    }
}

fn apply_step(server: &GameServer, doodads: &[SiegeDoodad], ended_period: u8) -> PeriodChange {
    let data = &server.game_data.systems.npcs.doodad;
    let mut outcome = PeriodChange::default();
    for (world, doodad) in doodads {
        let (func_group_id, deleted) = {
            let d = doodad.data();
            (d.func_group_id, d.phase.deleted)
        };
        if deleted {
            continue;
        }
        let Some(next_phase) = data.phase_funcs(func_group_id).iter().find_map(|pf| {
            let Some(FuncTemplate::SiegePeriod(f)) =
                data.phase_func_template(pf.func_id, &pf.func_type)
            else {
                return None;
            };
            (u32::from(ended_period) == f.siege_period_id).then_some(f.next_phase)
        }) else {
            continue;
        };
        let ctx = DoodadCtx::new(server, world.clone(), doodad.clone());
        let result = if next_phase < 0 {
            phase::despawn_or_delete_if_group(&ctx, func_group_id)
        } else {
            phase::do_change_phase_if_group(&ctx, None, func_group_id, next_phase)
        };
        match result {
            ExternalPhaseChange::Applied => outcome.changed += 1,
            ExternalPhaseChange::Busy => outcome.busy = true,
            ExternalPhaseChange::Skipped => {}
        }
    }
    outcome
}

/// Starts the 2 s period watcher (`DoodadManager` runtime). Called after the dominions are
/// loaded (the end of `housing::System::start`), so a boot never observes the empty period 0.
pub fn start(server: &Arc<GameServer>) {
    let server = Arc::downgrade(server);
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    rt.spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            interval.tick().await;
            let Some(server) = server.upgrade() else {
                return;
            };
            tick(&server);
        }
    });
}

#[cfg(test)]
#[path = "siege_phase_tests.rs"]
mod tests;
