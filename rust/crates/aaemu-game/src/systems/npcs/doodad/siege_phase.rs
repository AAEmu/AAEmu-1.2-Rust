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
//!   siege exists. Safe default: `down -> up` when the next warmup starts (`3` begins), so the
//!   safe zone is up during warmup and down during the siege (func 5). The `defense` flag
//!   (3410 only) is not used: its meaning is unknown and 4307/3304 do not set it.
//! * 4307's authored 10-minute lifetime vs the 90-minute siege. The phase chain wins here;
//!   the authored `min_time` is not applied.
//!
//! The calendar tick has no subscription point another subsystem may use, so this watcher polls
//! the read-only [`current_period`](crate::systems::housing::dominion::period::current_period)
//! every [`POLL_INTERVAL`]. The first observation of a zone is silent (a client that enters the
//! world receives the current state), exactly like `CalendarRuntime`. A doodad that enters a
//! phase after its period already ended waits for the next transition: catching up from a
//! persisted mid-cycle phase is capture-pending.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::data::DoodadData;
use super::phase;
use super::runtime::DoodadCtx;
use super::templates::FuncTemplate;
use crate::server::GameServer;
use crate::systems::housing::dominion::calendar::PERIOD_WARMUP;
use crate::systems::housing::dominion::period;
use crate::world::{DoodadRef, WorldInstance};

/// How often the watcher compares the zones' periods with the last ones it saw.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The last period observed per zone group. `None` = no observation yet, so the first period
/// change after a restart is not mistaken for an event that happened while the server was down.
#[derive(Debug, Default)]
pub struct Watcher {
    periods: Mutex<BTreeMap<u32, u8>>,
}

impl Watcher {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<u32, u8>> {
        self.periods.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last period observed for a zone group.
    pub fn observed(&self, zone_group_id: u32) -> Option<u8> {
        self.lock().get(&zone_group_id).copied()
    }

    /// Records a period without applying any phase change.
    pub fn record(&self, zone_group_id: u32, period: u8) {
        self.lock().insert(zone_group_id, period);
    }

    /// Drops every observation (tests / a disabled period source).
    pub fn clear(&self) {
        self.lock().clear();
    }
}

/// A doodad that currently sits in a phase with at least one `SiegePeriod` func, with the world
/// that owns it.
type SiegeDoodad = (Arc<WorldInstance>, DoodadRef);
type ZoneDoodads = BTreeMap<u32, Vec<SiegeDoodad>>;

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

/// Every live doodad whose current phase carries a `SiegePeriod` func, grouped by zone group.
fn collect(server: &GameServer) -> ZoneDoodads {
    let data = &server.game_data.systems.npcs.doodad;
    let mut by_zone: ZoneDoodads = BTreeMap::new();
    for world in server.world.worlds() {
        for doodad in world.all_doodads() {
            let (func_group_id, deleted) = {
                let d = doodad.data();
                (d.func_group_id, d.phase.deleted)
            };
            if deleted || !has_siege_period_func(data, func_group_id) {
                continue;
            }
            let Some(zone) = zone_group_of(server, &world, &doodad) else {
                continue;
            };
            by_zone
                .entry(zone)
                .or_default()
                .push((world.clone(), doodad));
        }
    }
    by_zone
}

/// Compares every zone's current period with the last observed one and applies the funcs of the
/// period that ended. Returns the number of applied phase changes.
pub fn tick(server: &GameServer) -> usize {
    let by_zone = collect(server);
    let watcher = &server.systems.npcs.doodads.siege_phase;
    let mut transitions = Vec::new();
    {
        let mut periods = watcher.lock();
        for zone in by_zone.keys().copied() {
            let Some(current) = period::current_period(server, zone) else {
                continue;
            };
            match periods.get(&zone).copied() {
                None => {
                    periods.insert(zone, current);
                }
                Some(previous) if previous == current => {}
                Some(previous) => {
                    periods.insert(zone, current);
                    transitions.push((zone, previous, current));
                }
            }
        }
    }
    let mut applied = 0;
    for (zone, previous, current) in transitions {
        if let Some(doodads) = by_zone.get(&zone) {
            applied += apply_to(server, doodads, previous, current);
        }
    }
    applied
}

/// Applies one observed period change `previous -> current` to the doodads of a zone group.
///
/// The phase func fires when `previous` ends. The 3410 `period 0` rule is folded in as the
/// documented default: it fires when the warmup period begins.
pub fn apply_period_change(
    server: &GameServer,
    zone_group_id: u32,
    previous: u8,
    current: u8,
) -> usize {
    collect(server)
        .get(&zone_group_id)
        .map_or(0, |doodads| apply_to(server, doodads, previous, current))
}

fn apply_to(server: &GameServer, doodads: &[SiegeDoodad], previous: u8, current: u8) -> usize {
    let data = &server.game_data.systems.npcs.doodad;
    let mut applied = 0;
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
            let ended = u32::from(previous) == f.siege_period_id;
            // UNKNOWN (capture-pending): the 3410 `period 0 -> up` func. Treat it as "the
            // next warmup starts", the documented safe default; `defense` is not used.
            let safe_zone_default = f.siege_period_id == 0 && current == PERIOD_WARMUP;
            (ended || safe_zone_default).then_some(f.next_phase)
        }) else {
            continue;
        };
        let ctx = DoodadCtx::new(server, world.clone(), doodad.clone());
        let changed = if next_phase < 0 {
            phase::despawn_or_delete_if_group(&ctx, func_group_id)
        } else {
            phase::do_change_phase_if_group(&ctx, None, func_group_id, next_phase)
        };
        if changed {
            applied += 1;
        }
    }
    applied
}

/// Starts the 2 s period watcher (`DoodadManager` runtime).
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
