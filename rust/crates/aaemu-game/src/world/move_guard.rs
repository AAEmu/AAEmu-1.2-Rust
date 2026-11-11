//! Server-side movement guard: the server transform is the baseline of every client-reported
//! unit position, and a move may only cost what the unit's speed budget has accumulated.
//!
//! * [`MoveGuard`] holds one token bucket per moved object (the player on foot, an owned mate, a
//!   client-driven land vehicle): a horizontal budget and an upward budget in metres, refilled
//!   at the configured speed of *server* time and capped at `speed * burst`. The arithmetic
//!   lives in the pure [`MoveGuard::check`], which takes `now`, so it is tested without
//!   sleeping. Falling costs nothing.
//! * [`guard_unit_move`] / [`guard_vehicle_move`] measure the move a packet asks for (before any
//!   side effect of the move) and, on a violation, apply nothing, log (rate limited) and
//!   correct the client.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};

use dashmap::DashMap;

use crate::models::movement::{MoveType, MoveTypeActorFlags, UnitMove, VehicleMove};
use crate::models::transform::PositionAndRotation;
use crate::server::GameServer;
use crate::systems::skills::packets_special::SCTeleportUnitPacket;
use crate::systems::vehicles::slaves::attach_reason;
use crate::world::{PlayerRef, WorldInstance, WorldObject};

/// How far (m) a unit may be from an object it newly claims to stand on (ships and airships are
/// large).
pub const NEW_SUPPORT_MAX_DISTANCE: f32 = 150.0;
/// At most one warning / audit line per unit within this many milliseconds.
const REPORT_INTERVAL_MS: i64 = 10_000;
/// At most one correction packet per unit within this many milliseconds.
const CORRECTION_INTERVAL_MS: i64 = 1_000;
/// An entry idle for this long (ms) is dropped by the lazy prune.
const IDLE_PRUNE_MS: i64 = 600_000;
/// The prune runs only above this many entries, at most once per [`PRUNE_INTERVAL_MS`].
const PRUNE_MIN_ENTRIES: usize = 256;
const PRUNE_INTERVAL_MS: i64 = 60_000;

/// The speed limits one check is judged against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Sustained horizontal speed, m/s.
    pub max_speed: f32,
    /// Sustained upward speed, m/s.
    pub max_up_speed: f32,
    /// Seconds of speed the budgets can hold.
    pub burst_seconds: f32,
}

/// What a move costs: its horizontal distance and its positive height gain, in metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    pub horizontal: f32,
    pub up: f32,
}

/// What a budget was seeded for: a different value starts a fresh (full) budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seed {
    /// `Character::teleport_seq` (bumped by every server teleport).
    pub teleport_seq: u32,
    pub instance_id: u32,
}

/// The kick settings of one check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KickPolicy {
    /// Refused moves within the window that disconnect the player; `0` = never.
    pub after: u32,
    pub window_ms: i64,
}

#[derive(Debug)]
struct State {
    horizontal: f32,
    up: f32,
    last_ms: i64,
    seed: Seed,
    violations: VecDeque<i64>,
    last_report_ms: i64,
    suppressed: u32,
    last_correction_ms: i64,
}

/// The outcome of [`MoveGuard::check`].
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Ok,
    Reject(Rejection),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub cost: Cost,
    /// The budgets (after the refill) the move did not fit in.
    pub budget: Cost,
    /// `Some(n)`: emit the warning / audit line now; `n` similar ones were suppressed since the
    /// last. `None`: stay quiet (rate limit).
    pub report: Option<u32>,
    /// Send the client its server position (rate limited).
    pub correct: bool,
    /// The violation count within the window reached the kick threshold.
    pub kick: bool,
}

/// One token bucket per moved object (`obj_id`).
#[derive(Debug, Default)]
pub struct MoveGuard {
    states: DashMap<u32, State>,
    last_prune_ms: AtomicI64,
}

impl MoveGuard {
    /// Judges one move of object `key` at server time `now_ms`. A move that fits is charged; a
    /// move that does not is refused without being charged. A new entry or a changed `seed`
    /// starts with full budgets. A non-finite cost is a violation.
    pub fn check(
        &self,
        key: u32,
        seed: Seed,
        now_ms: i64,
        cost: Cost,
        limits: Limits,
        kick: KickPolicy,
    ) -> Verdict {
        self.prune(now_ms);
        let cap_h = limits.max_speed * limits.burst_seconds;
        let cap_u = limits.max_up_speed * limits.burst_seconds;
        let mut st = self.states.entry(key).or_insert_with(|| State {
            horizontal: cap_h,
            up: cap_u,
            last_ms: now_ms,
            seed,
            violations: VecDeque::new(),
            last_report_ms: i64::MIN / 2,
            suppressed: 0,
            last_correction_ms: i64::MIN / 2,
        });
        if st.seed != seed {
            st.seed = seed;
            st.horizontal = cap_h;
            st.up = cap_u;
        } else {
            let dt = (now_ms - st.last_ms).max(0) as f32 / 1000.0;
            st.horizontal = (st.horizontal + dt * limits.max_speed).min(cap_h);
            st.up = (st.up + dt * limits.max_up_speed).min(cap_u);
        }
        st.last_ms = now_ms;
        let fits = cost.horizontal.is_finite()
            && cost.up.is_finite()
            && cost.horizontal <= st.horizontal
            && cost.up <= st.up;
        if fits {
            st.horizontal -= cost.horizontal;
            st.up -= cost.up;
            return Verdict::Ok;
        }
        let budget = Cost {
            horizontal: st.horizontal,
            up: st.up,
        };
        st.violations.push_back(now_ms);
        while st
            .violations
            .front()
            .is_some_and(|&t| now_ms - t > kick.window_ms)
        {
            st.violations.pop_front();
        }
        let kick_now = kick.after > 0 && st.violations.len() >= kick.after as usize;
        let report = if now_ms - st.last_report_ms >= REPORT_INTERVAL_MS {
            st.last_report_ms = now_ms;
            Some(std::mem::take(&mut st.suppressed))
        } else {
            st.suppressed = st.suppressed.saturating_add(1);
            None
        };
        let correct = now_ms - st.last_correction_ms >= CORRECTION_INTERVAL_MS;
        if correct {
            st.last_correction_ms = now_ms;
        }
        Verdict::Reject(Rejection {
            cost,
            budget,
            report,
            correct,
            kick: kick_now,
        })
    }

    /// Drops entries idle for [`IDLE_PRUNE_MS`] once the map is large (lazy cleanup: a unit
    /// that left the world simply stops checking in).
    fn prune(&self, now_ms: i64) {
        if self.states.len() < PRUNE_MIN_ENTRIES {
            return;
        }
        let last = self.last_prune_ms.load(Ordering::Relaxed);
        if now_ms - last < PRUNE_INTERVAL_MS
            || self
                .last_prune_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        self.states
            .retain(|_, s| now_ms - s.last_ms < IDLE_PRUNE_MS);
    }

    /// Re-bases an entry on `seed` without refilling it: used after the guard's own correction
    /// teleport, which bumps `teleport_seq` and must not hand the cheater a full budget.
    pub fn rebase(&self, key: u32, seed: Seed) {
        if let Some(mut st) = self.states.get_mut(&key) {
            st.seed = seed;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.states.len()
    }
}

/// The cost of moving from `from` to `to` (world, or one shared local frame).
pub fn cost_between(from: (f32, f32, f32), to: (f32, f32, f32)) -> Cost {
    Cost {
        horizontal: (to.0 - from.0).hypot(to.1 - from.1),
        up: (to.2 - from.2).max(0.0),
    }
}

fn limits_of(server: &GameServer, max_speed: f32) -> Limits {
    let w = &server.config.world;
    Limits {
        max_speed,
        max_up_speed: w.move_guard_max_up_speed,
        burst_seconds: w.move_guard_burst_seconds,
    }
}

fn kick_policy(server: &GameServer) -> KickPolicy {
    let w = &server.config.world;
    KickPolicy {
        after: w.move_guard_kick_after_violations,
        window_ms: (w.move_guard_violation_window_seconds * 1000.0) as i64,
    }
}

/// Whether `me` is checked at all (guard enabled, access level below the exemption).
fn applies_to(server: &GameServer, me: &PlayerRef) -> bool {
    let w = &server.config.world;
    w.move_guard_enabled && me.data().access_level < w.move_guard_exempt_access_level
}

fn seed_of(me: &PlayerRef, target: &WorldObject) -> Seed {
    // Two statements: when `target` is `me`, both lock the same character.
    let teleport_seq = me.data().teleport_seq;
    let instance_id = target.transform().instance_id;
    Seed {
        teleport_seq,
        instance_id,
    }
}

fn xyz(p: &PositionAndRotation) -> (f32, f32, f32) {
    (p.x, p.y, p.z)
}

/// The cost of the unit move `u` of `target` against the server transform, with the positions
/// it was measured between (for the log).
fn measure_unit_move(
    world: &WorldInstance,
    target: &WorldObject,
    mt: &MoveType,
    u: &UnitMove,
) -> (Cost, (f32, f32, f32), (f32, f32, f32)) {
    let (current_parent, support) =
        crate::systems::vehicles::movement::resolve_support(world, target, mt, u);
    let before_world = world.world_pos_rot(target);
    let before_local = {
        let t = target.transform();
        (t.x, t.y, t.z)
    };
    let local = (u.x, u.y, u.z);
    // The supporting object despawned under the unit: the server position is a local offset
    // without a frame, so there is no baseline to measure against. Let this move through (the
    // caller detaches the unit); it is a rare, self-inflicted gap, not something to refuse.
    if current_parent != 0 && world.object(current_parent).is_none() {
        let none = Cost {
            horizontal: 0.0,
            up: 0.0,
        };
        return (none, before_local, local);
    }
    match support {
        // Same supporting object: the delta in its frame (a moving deck is not the player's
        // speed).
        Some(parent) if parent.obj_id() == current_parent => {
            (cost_between(before_local, local), before_local, local)
        }
        // A new supporting object: it has to be close, and the move is measured in the world.
        Some(parent) => {
            let parent_world = world.world_pos_rot(&parent);
            let gap = (parent_world.x - before_world.x)
                .hypot(parent_world.y - before_world.y)
                .hypot(parent_world.z - before_world.z);
            let candidate = PositionAndRotation::compose(
                &parent_world,
                parent.scale(),
                &PositionAndRotation::new(u.x, u.y, u.z, 0.0, 0.0, 0.0),
            );
            let mut cost = cost_between(xyz(&before_world), xyz(&candidate));
            if gap.is_nan() || gap > NEW_SUPPORT_MAX_DISTANCE {
                cost.horizontal = f32::INFINITY;
            }
            (cost, xyz(&before_world), xyz(&candidate))
        }
        None => {
            // The `missing_parent` fallback of `move_unit`: the world backup position.
            let to = if u
                .actor_flags
                .contains(MoveTypeActorFlags::STANDING_ON_OBJECT)
            {
                (u.x2, u.y2, u.z2)
            } else {
                (u.x, u.y, u.z)
            };
            (cost_between(xyz(&before_world), to), xyz(&before_world), to)
        }
    }
}

/// Checks a `UnitMoveType` of the player `me` (or its owned mate) `target` before the move has
/// any side effect. `false` = the move was refused and the client corrected; the caller drops
/// the packet.
pub fn guard_unit_move(
    server: &GameServer,
    world: &WorldInstance,
    me: &PlayerRef,
    target: &WorldObject,
    mt: &MoveType,
    u: &UnitMove,
) -> bool {
    if !applies_to(server, me) {
        return true;
    }
    let (cost, from, to) = measure_unit_move(world, target, mt, u);
    let verdict = server.systems.services.move_guard.check(
        target.obj_id(),
        seed_of(me, target),
        crate::world::now_ms(),
        cost,
        limits_of(server, server.config.world.move_guard_max_speed),
        kick_policy(server),
    );
    let Verdict::Reject(r) = verdict else {
        return true;
    };
    let what = if target.as_mate().is_some() {
        "mate"
    } else {
        "player"
    };
    report(server, me, what, from, to, &r);
    if r.correct {
        if target.as_mate().is_some() {
            // Take the owner off the mount and put it at the mate's server position.
            crate::systems::vehicles::force_dismount(server, me, attach_reason::PREFAB_CHANGED);
        }
        teleport_back(server, me, &world.world_pos_rot(target));
        server
            .systems
            .services
            .move_guard
            .rebase(target.obj_id(), seed_of(me, target));
    }
    kick_if_due(me, &r);
    false
}

/// Checks a client-driven land vehicle move (`VehicleMoveType`) of the verified `driver`.
pub fn guard_vehicle_move(
    server: &GameServer,
    world: &WorldInstance,
    driver: &PlayerRef,
    car: &WorldObject,
    v: &VehicleMove,
) -> bool {
    if !applies_to(server, driver) {
        return true;
    }
    let before = world.world_pos_rot(car);
    let to = (v.x, v.y, v.z);
    let cost = cost_between(xyz(&before), to);
    let verdict = server.systems.services.move_guard.check(
        car.obj_id(),
        seed_of(driver, car),
        crate::world::now_ms(),
        cost,
        limits_of(server, server.config.world.move_guard_vehicle_max_speed),
        kick_policy(server),
    );
    let Verdict::Reject(r) = verdict else {
        return true;
    };
    report(server, driver, "vehicle", xyz(&before), to, &r);
    if r.correct {
        // The move is dropped; the driver is taken off the vehicle and put next to it (the
        // vehicle itself is not rebroadcast: there is no packet for it outside a movement).
        crate::systems::vehicles::force_dismount(server, driver, attach_reason::PREFAB_CHANGED);
        teleport_back(server, driver, &before);
        server
            .systems
            .services
            .move_guard
            .rebase(car.obj_id(), seed_of(driver, car));
    }
    kick_if_due(driver, &r);
    false
}

fn teleport_back(server: &GameServer, me: &PlayerRef, at: &PositionAndRotation) {
    crate::world::session::send_teleport_unit_ending_trade(
        server,
        me,
        SCTeleportUnitPacket {
            reason: 0,
            error_message: 0,
            x: at.x,
            y: at.y,
            z: at.z,
            z2: at.rot_z,
        },
    );
}

fn kick_if_due(me: &PlayerRef, r: &Rejection) {
    if r.kick {
        tracing::warn!(
            "Disconnecting '{}': too many refused moves",
            me.data().unit.name
        );
        me.conn.close();
    }
}

/// The rate-limited `tracing::warn!` + `SusManager` audit entry of a refused move.
fn report(
    server: &GameServer,
    me: &PlayerRef,
    what: &str,
    from: (f32, f32, f32),
    to: (f32, f32, f32),
    r: &Rejection,
) {
    let Some(suppressed) = r.report else {
        return;
    };
    let name = me.data().unit.name.clone();
    let text = format!(
        "Refused {what} move of {name}: from {from:?} to {to:?}, {:.1} m horizontal / {:.1} m up, budget {:.1} m / {:.1} m ({suppressed} similar suppressed)",
        r.cost.horizontal, r.cost.up, r.budget.horizontal, r.budget.up
    );
    tracing::warn!("{text}");
    server.systems.services.sus.log_activity_player_deferred(
        server,
        crate::systems::services::sus::CATEGORY_CHEATING,
        Some(me),
        &text,
    );
}
