//! `Core/Managers/TimeManager` + `TimeOfDayObserver`: the game clock and the per-character
//! time-of-day subscriptions. An observer may carry a resolver for the per-character override of
//! `TimeOfDayObserver.OnNext` (`StormShipLogic.ResolveClientTimeOfDayHours`).
//!
//! **Deviation from the C#:** the C# clock started at 12:00 on every boot. The Rust clock is
//! derived from the wall clock instead, so it is continuous across restarts:
//! `game seconds = (unix_seconds * 6 + offset) mod 86400` (6 game seconds per real second, the
//! existing `TickDelay * 10` per 10 s tick: one game minute per 10 real seconds) and
//! `game day = floor((unix_seconds * 6 + offset) / 86400)`. `offset` comes from
//! `World.GameTimeOffset` (game seconds, default 0) and `Set(hours)` moves it at runtime while
//! keeping the current game day. The tower defense scheduler gates `tod` events on the same
//! epoch-derived day.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::net::connection::ConnectionHandle;
use crate::packets::world::{SCDetailedTimeOfDayPacket, SCTimeOfDayPacket};

const MAX_TIME: f64 = 86_400.0;
/// The 1.2 clock speed (`TimeManager.TickDelay = 3600 * Speed`): `TickDelay * 10` game seconds
/// every 10 real seconds is one game minute per 10 real seconds, i.e. 6 game seconds per real
/// second. The C# float constant advances 59.9976 s per tick; the wall-clock derivation uses the
/// nominal 6 s.
const GAME_SECONDS_PER_REAL_SECOND: f64 = 6.0;

/// The wall clock the game time is derived from (seconds since the Unix epoch). Tests replace it
/// through [`TimeManager::set_test_clock`].
pub(crate) type UnixClock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// `TimeOfDayObserver.OnNext` override: the hours to send instead of the game time (`None` = the
/// game time).
pub type TimeOfDayResolver = Arc<dyn Fn() -> Option<f32> + Send + Sync>;

/// A `TimeOfDayObserver`: the owner's connection and its time-of-day override.
#[derive(Clone)]
struct Observer {
    conn: ConnectionHandle,
    resolve: Option<TimeOfDayResolver>,
}

impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observer")
            .field("conn", &self.conn)
            .field("resolve", &self.resolve.is_some())
            .finish()
    }
}

struct State {
    /// The cached game time in seconds (`0..86400`), refreshed from the wall clock every tick
    /// and set exactly by [`TimeManager::set`].
    time: f64,
    /// Game seconds added to `unix_seconds * GAME_SECONDS_PER_REAL_SECOND` (the configured
    /// offset, moved by `Set`).
    offset: f64,
    clock: UnixClock,
    /// Observers by subscription id (`IObserver<float>` of a character).
    observers: HashMap<u64, Observer>,
    next_id: u64,
    /// `!_work`.
    stopped: bool,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("time", &self.time)
            .field("offset", &self.offset)
            .field("observers", &self.observers)
            .field("next_id", &self.next_id)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

/// `(unix_seconds * 6 + offset) mod 86400`: the game time of day in seconds.
fn game_seconds(unix_seconds: f64, offset: f64) -> f64 {
    (unix_seconds * GAME_SECONDS_PER_REAL_SECOND + offset).rem_euclid(MAX_TIME)
}

/// `floor((unix_seconds * 6 + offset) / 86400)`: the game day number (`Set` keeps it).
fn game_day(unix_seconds: f64, offset: f64) -> i64 {
    ((unix_seconds * GAME_SECONDS_PER_REAL_SECOND + offset) / MAX_TIME).floor() as i64
}

fn wall_clock_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// `TimeManager`.
#[derive(Debug, Clone)]
pub struct TimeManager {
    state: Arc<Mutex<State>>,
}

impl TimeManager {
    /// A clock derived from the wall clock, with `offset_seconds` game seconds added
    /// (`World.GameTimeOffset`).
    pub fn new(offset_seconds: f64) -> Self {
        Self::with_clock(offset_seconds, Arc::new(wall_clock_seconds))
    }

    pub(crate) fn with_clock(offset_seconds: f64, clock: UnixClock) -> Self {
        let time = game_seconds(clock(), offset_seconds);
        Self {
            state: Arc::new(Mutex::new(State {
                time,
                offset: offset_seconds,
                clock,
                observers: HashMap::new(),
                next_id: 1,
                stopped: false,
            })),
        }
    }

    /// Test seam: boot the manager on another wall clock with a fresh `offset_seconds`.
    #[cfg(test)]
    pub(crate) fn set_test_clock(&self, clock: UnixClock, offset_seconds: f64) {
        let mut st = self.state.lock().expect("time poisoned");
        st.clock = clock;
        st.offset = offset_seconds;
        st.time = game_seconds((st.clock)(), st.offset);
    }

    /// `GetTime` (hours).
    pub fn get_time(&self) -> f32 {
        self.state.lock().expect("time poisoned").time as f32 / 3600.0
    }

    /// `Get()`: the game time in seconds.
    pub fn get(&self) -> f32 {
        self.state.lock().expect("time poisoned").time as f32
    }

    /// The game day number (`floor((unix_seconds * 6 + offset) / 86400)`): the tower defense
    /// schedule's `tod_day_interval` and `scheduled_days` gate on it, so a restart keeps the day
    /// parity.
    pub fn get_day(&self) -> i64 {
        let (clock, offset) = {
            let st = self.state.lock().expect("time poisoned");
            (st.clock.clone(), st.offset)
        };
        game_day(clock(), offset)
    }

    /// `Set(hours)`: moves the wall-clock mapping so the time of day is `hours`, keeping the
    /// current game day (the C# only had a float and no day).
    pub fn set(&self, hours: f32) {
        let mut st = self.state.lock().expect("time poisoned");
        let now = (st.clock)();
        let day = game_day(now, st.offset);
        st.offset =
            day as f64 * MAX_TIME + f64::from(hours) * 3600.0 - now * GAME_SECONDS_PER_REAL_SECOND;
        st.time = game_seconds(now, st.offset);
    }

    /// `Subscribe(connection, observer)`: sends `SCDetailedTimeOfDayPacket` and registers the
    /// observer; the returned id is the `IDisposable` to pass to [`TimeManager::unsubscribe`].
    pub fn subscribe(&self, conn: &ConnectionHandle) -> u64 {
        self.subscribe_with(conn, None)
    }

    /// [`TimeManager::subscribe`] with the observer's time-of-day override (`resolve`), asked on
    /// every push (`StormShipLogic.ResolveClientTimeOfDayHours(owner) ?? value`). The initial
    /// `SCDetailedTimeOfDayPacket` always carries the game time, as in the C#.
    pub fn subscribe_with(
        &self,
        conn: &ConnectionHandle,
        resolve: Option<TimeOfDayResolver>,
    ) -> u64 {
        conn.send(SCDetailedTimeOfDayPacket {
            time: self.get_time(),
        });
        let mut st = self.state.lock().expect("time poisoned");
        let id = st.next_id;
        st.next_id += 1;
        st.observers.insert(
            id,
            Observer {
                conn: conn.clone(),
                resolve,
            },
        );
        id
    }

    /// `Unsubscriber.Dispose`.
    pub fn unsubscribe(&self, id: u64) {
        self.state
            .lock()
            .expect("time poisoned")
            .observers
            .remove(&id);
    }

    /// One `Tick` + `Push`: refresh the clock from the wall clock and notify every observer.
    pub fn tick(&self) {
        let (time, observers) = {
            let mut st = self.state.lock().expect("time poisoned");
            st.time = game_seconds((st.clock)(), st.offset);
            (
                st.time as f32 / 3600.0,
                st.observers.values().cloned().collect::<Vec<_>>(),
            )
        };
        for o in observers {
            let tod = o.resolve.as_ref().and_then(|f| f()).unwrap_or(time);
            o.conn.send(SCTimeOfDayPacket { time: tod });
        }
    }

    /// `Start()`: the 10 s tick thread.
    pub fn start(&self) {
        let me = self.clone();
        me.state.lock().expect("time poisoned").stopped = false;
        tokio::spawn(async move {
            // C# loop: advance + push, then sleep 10 s (`while (_work)`).
            let mut iv = tokio::time::interval(Duration::from_secs(10));
            loop {
                iv.tick().await;
                if me.state.lock().expect("time poisoned").stopped {
                    break;
                }
                me.tick();
            }
        });
    }

    /// `Stop()`: ends the tick loop.
    pub fn stop(&self) {
        self.state.lock().expect("time poisoned").stopped = true;
    }
}

/// A wall clock a test moves by hand (see [`TimeManager::set_test_clock`]).
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct ManualClock(Arc<std::sync::atomic::AtomicU64>);

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new(unix_seconds: f64) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU64::new(
            unix_seconds.to_bits(),
        )))
    }

    pub(crate) fn set(&self, unix_seconds: f64) {
        self.0
            .store(unix_seconds.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn advance(&self, real_seconds: f64) {
        self.set(self.unix_seconds() + real_seconds);
    }

    pub(crate) fn unix_seconds(&self) -> f64 {
        f64::from_bits(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub(crate) fn clock(&self) -> UnixClock {
        let me = self.clone();
        Arc::new(move || me.unix_seconds())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::data::GameData;
    use crate::opcodes::sc;
    use crate::systems::ai::physics::forces::STORM_BUFF_ID;
    use crate::systems::skills::data::BuffTemplate;
    use crate::test_support::*;
    use crate::world::WorldObject;

    fn pushed_times(client: &mut TestClient) -> Vec<f32> {
        client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCTimeOfDayPacket)
            .map(|(_, b)| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    /// Two servers booted at different real times see the same game time at the same instant:
    /// the clock is a function of the wall clock, not of how long the process has been up.
    #[test]
    fn two_boots_at_different_wall_clock_times_agree_at_the_same_instant() {
        let clock = super::ManualClock::new(1_000.0);
        let first = super::TimeManager::with_clock(0.0, clock.clock());
        assert_eq!(first.get(), 6_000.0);
        clock.set(2_500.0);
        let second = super::TimeManager::with_clock(0.0, clock.clock());
        assert_eq!(second.get(), 15_000.0);
        clock.set(3_000.0);
        first.tick();
        second.tick();
        assert_eq!(first.get(), 18_000.0);
        assert_eq!(second.get(), first.get());
        assert_eq!(second.get_time(), first.get_time());
        assert_eq!(second.get_day(), first.get_day());
        // `World.GameTimeOffset` shifts the whole formula (game seconds).
        let shifted = super::TimeManager::with_clock(3_600.0, clock.clock());
        shifted.tick();
        assert_eq!(shifted.get_time(), 6.0);
    }

    /// `Set(hours)` (GM `/time set`): moves the time of day at runtime and keeps the game day,
    /// so a `tod_day_interval` gate does not shift after a GM action.
    #[test]
    fn gm_set_moves_the_time_of_day_and_keeps_the_game_day() {
        // Day 5, 03:00 (`unix * 6 = 5 * 86400 + 3 * 3600`).
        let clock = super::ManualClock::new((5.0 * 86_400.0 + 3.0 * 3_600.0) / 6.0);
        let time = super::TimeManager::with_clock(0.0, clock.clock());
        assert_eq!(time.get_day(), 5);
        assert_eq!(time.get_time(), 3.0);
        time.set(6.5);
        assert_eq!(time.get_time(), 6.5);
        assert_eq!(time.get_day(), 5, "Set keeps the game day");
        clock.advance(100.0); // 100 real s = 600 game s = 10 game minutes
        time.tick();
        assert!((time.get_time() - (6.5 + 10.0 / 60.0)).abs() < 1e-4);
    }

    #[tokio::test]
    async fn resolver_overrides_the_pushed_time() {
        let server = test_server();
        let mut client = TestClient::new(&server, 1, 1);
        let handle = client.conn.handle();
        let plain = server.time.subscribe(&handle);
        let forced = server
            .time
            .subscribe_with(&handle, Some(Arc::new(|| Some(2.0))));
        let none = server.time.subscribe_with(&handle, Some(Arc::new(|| None)));
        // Subscribing sends the detailed (real) time to each observer.
        let detailed = client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCDetailedTimeOfDayPacket)
            .count();
        assert_eq!(detailed, 3);
        server.time.tick();
        let now = server.time.get_time();
        let mut got = pushed_times(&mut client);
        got.sort_by(f32::total_cmp);
        let mut want = vec![now, now, 2.0];
        want.sort_by(f32::total_cmp);
        assert_eq!(got, want);
        for id in [plain, forced, none] {
            server.time.unsubscribe(id);
        }
        server.time.tick();
        assert!(pushed_times(&mut client).is_empty());
    }

    /// `TimeOfDayObserver.OnNext` with `StormShipLogic.ResolveClientTimeOfDayHours`: 2:00 only
    /// with the realistic sea weather model and the storm buff.
    #[tokio::test]
    async fn storm_buff_forces_night_with_realistic_sea_weather() {
        let mut data = GameData::empty();
        data.systems.skills.add_buff(BuffTemplate {
            id: STORM_BUFF_ID,
            kind: 2,
            duration: 0,
            ..BuffTemplate::default()
        });
        let server = Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::skills::bind(&server);
        let mut client = TestClient::new(&server, 1, 1);
        let p = spawn_player(
            &server,
            &client,
            test_character(1, "Sailor", 10.0, 10.0),
            0x100,
        );
        let resolver = crate::world::session::storm_time_of_day_resolver(&server, &p);
        let sub = server.time.subscribe_with(&p.conn, Some(resolver));
        client.drain();

        // Official model: the game time.
        server.time.tick();
        assert_eq!(pushed_times(&mut client), vec![server.time.get_time()]);
        // Realistic, no storm buff: the game time.
        server.config.update(|c| {
            c.world.sea_weather_model = crate::config::SeaWeatherModelType::Realistic;
        });
        server.time.tick();
        assert_eq!(pushed_times(&mut client), vec![server.time.get_time()]);
        // Realistic + storm buff: 2:00.
        let obj = WorldObject::from(&p);
        crate::systems::skills::buffs::add_buff_by_id(&server, &obj, STORM_BUFF_ID, &obj, 0);
        assert!(p.data().unit.buffs.check_buff(STORM_BUFF_ID));
        client.drain();
        server.time.tick();
        assert_eq!(pushed_times(&mut client), vec![2.0]);
        // Back to official: the game time even with the buff.
        server.config.update(|c| {
            c.world.sea_weather_model = crate::config::SeaWeatherModelType::Official;
        });
        server.time.tick();
        assert_eq!(pushed_times(&mut client), vec![server.time.get_time()]);
        server.time.unsubscribe(sub);
    }
}
