//! `Core/Managers/SusManager`: the suspicious-activity log (`audit_char_sus`) and the movement
//! speed analysis of players and mounts, run every 5 s of `Transform.FinalizeTransform`
//! ([`SusManager::finalize_transform`], called by the unit movement handler).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

/// `CategoryBot`.
pub const CATEGORY_BOT: &str = "Bot";
/// `CategoryBotReport`.
pub const CATEGORY_BOT_REPORT: &str = "BotReport";
/// `CategoryCheating`.
pub const CATEGORY_CHEATING: &str = "Cheat";
/// `CategoryRmt`.
pub const CATEGORY_RMT: &str = "RMT";

/// The flat speed above which a player is reported (m/s).
pub const PLAYER_CHECK_SPEED: f64 = 25.0;
/// The flat speed above which a mount is reported (m/s).
pub const MOUNT_CHECK_SPEED: f64 = 27.5;
/// Seconds a report suppresses further reports.
const SKIP_TIME_AFTER_REPORT: f32 = 15.0;
/// A baseline older than this (s) saw no continuous movement samples (riding, a loading
/// screen, a teleport, standing still): the sample re-seeds it instead of being judged. While a
/// character moves the analysis runs every ~5 s.
const MAX_BASELINE_AGE: f32 = 10.0;

/// What a position sample is judged against: when and where (instance / teleport counter) the
/// baseline was recorded. `Default` (all zero) disables the staleness checks and keeps the
/// C# fixed-interval behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampleCtx {
    /// Unix ms of the sample.
    pub now_ms: i64,
    /// World instance the object was in.
    pub instance_id: u32,
    /// `Character::teleport_seq` (0 for mates).
    pub teleport_seq: u32,
}

/// The stored baseline of one analysed object.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    pos: [f32; 3],
    skip_time: f32,
    ctx: SampleCtx,
}

type Baselines = Mutex<HashMap<u32, Baseline>>;

/// `SusManager`.
#[derive(Debug, Default)]
pub struct SusManager {
    /// `LastPlayerPositions` (player id -> pos, skipTime and when / where it was taken).
    last_player_positions: Baselines,
    /// `LastPetPositions` (mate id -> pos, skipTime and when / where it was taken).
    last_pet_positions: Baselines,
    /// `Transform._lastFinalizeTime` (unix ms) / `_skipCheckTime` (s) per object.
    finalize_checks: Mutex<HashMap<u32, (i64, f32)>>,
}

impl SusManager {
    /// The `_skipCheckTime` part of `Transform.FinalizeTransform` for an object whose world
    /// position changed: `true` once every 5 s (the first finalize of an object included).
    pub fn finalize_check(&self, obj_id: u32, now_ms: i64) -> bool {
        let mut m = self.finalize_checks.lock().expect("sus poisoned");
        let e = m.entry(obj_id).or_insert((now_ms, 0.0));
        let delta = (now_ms - e.0) as f32 / 1000.0;
        e.0 = now_ms;
        e.1 -= delta;
        if e.1 <= 0.0 {
            e.1 = 5.0;
            true
        } else {
            false
        }
    }

    /// Forgets an object's `FinalizeTransform` timer (the object left the world).
    pub fn forget_object(&self, obj_id: u32) {
        self.finalize_checks
            .lock()
            .expect("sus poisoned")
            .remove(&obj_id);
    }

    /// `Transform.FinalizeTransform` every 5 s: `AnalyzePlayerDeltaMovement(player, 5f)` for a
    /// character, `AnalyzeMountDeltaMovement(mount, 5f)` for a mate (the database log runs on
    /// a task).
    pub fn finalize_transform(&self, server: &GameServer, obj: &WorldObject) {
        if !matches!(obj, WorldObject::Player(_) | WorldObject::Mate(_)) {
            return;
        }
        if !self.finalize_check(obj.obj_id(), crate::world::now_ms()) {
            return;
        }
        let (Some(arc), Ok(rt)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let obj = obj.clone();
        rt.spawn(async move {
            let sus = &arc.systems.services.sus;
            match &obj {
                WorldObject::Player(p) => {
                    let riding = p.data().is_riding;
                    sus.analyze_player(&arc, p, riding, 5.0).await;
                }
                WorldObject::Mate(m) => sus.analyze_mount(&arc, m, 5.0).await,
                _ => {}
            }
        });
    }

    /// `AnalyzeMountDeltaMovement(pet, deltaTime)` including the `audit_char_sus` entry of the
    /// pet's owner.
    pub async fn analyze_mount(
        &self,
        server: &GameServer,
        pet: &crate::world::MateRef,
        delta_time: f32,
    ) {
        let obj = WorldObject::from(pet);
        let pos = match server.world.world_of(&obj) {
            Some(w) => {
                let p = w.world_pos_rot(&obj);
                [p.x, p.y, p.z]
            }
            None => {
                let t = obj.transform();
                [t.x, t.y, t.z]
            }
        };
        let (id, name, disabled, owner_obj_id, instance_id) = {
            let m = pet.data();
            (
                m.id,
                m.unit.name.clone(),
                false,
                m.owner_obj_id,
                m.unit.transform.instance_id,
            )
        };
        let ctx = SampleCtx {
            now_ms: crate::world::now_ms(),
            instance_id,
            teleport_seq: 0,
        };
        if let Some(speed) =
            self.analyze_mount_delta_movement_at(id, pos, disabled, delta_time, ctx)
        {
            let owner = server.world.character_by_obj_id(owner_obj_id);
            let owner_name = owner.as_ref().map(|o| o.data().unit.name.clone());
            let text = Self::mount_report(&name, owner_name.as_deref(), speed);
            self.log_activity_player(server, CATEGORY_CHEATING, owner.as_ref(), &text)
                .await;
        }
    }

    /// `LogActivity(category, accountId, playerId, zoneGroup, position, description)`.
    #[allow(clippy::too_many_arguments)]
    pub async fn log_activity(
        &self,
        server: &GameServer,
        category: &str,
        account_id: u32,
        player_id: u32,
        zone_group: u32,
        position: [f32; 3],
        description: &str,
    ) -> bool {
        let res = sqlx::query(
            "INSERT INTO audit_char_sus (sus_category, sus_account, sus_character, zone_group, x, y, z, description) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(category)
        .bind(account_id)
        .bind(player_id)
        .bind(zone_group)
        .bind(position[0])
        .bind(position[1])
        .bind(position[2])
        .bind(description)
        .execute(&server.db)
        .await;
        match res {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::warn!("{description}");
                true
            }
            Ok(_) => {
                tracing::error!("Saving suspisious activity failed: {category} - {description}");
                false
            }
            Err(e) => {
                tracing::error!(
                    "Saving suspisious activity failed: {category} - {description} - {e}"
                );
                false
            }
        }
    }

    /// `LogActivity(category, player, description)` (the C# passes the player's `ZoneId` as the
    /// zone group).
    pub async fn log_activity_player(
        &self,
        server: &GameServer,
        category: &str,
        player: Option<&PlayerRef>,
        description: &str,
    ) -> bool {
        let (account, id, zone, pos) = match player {
            Some(p) => {
                let pos = world_position(server, p);
                let c = p.data();
                (c.account_id, c.id, c.unit.transform.zone_id, pos)
            }
            None => (0, 0, 0, [0.0; 3]),
        };
        self.log_activity(server, category, account, id, zone, pos, description)
            .await
    }

    /// `LogActivity(category, description)`.
    pub async fn log_activity_text(
        &self,
        server: &GameServer,
        category: &str,
        description: &str,
    ) -> bool {
        self.log_activity(server, category, 0, 0, 0, [0.0; 3], description)
            .await
    }

    /// The speed check of `AnalyzePlayerDeltaMovement` / `AnalyzeMountDeltaMovement`: records
    /// the position and returns the observed flat speed when it exceeds `max_speed`.
    ///
    /// Deviation from the C#: the speed is taken over the real time since the baseline (never
    /// less than `delta_time`), and a stale baseline (older than [`MAX_BASELINE_AGE`], another
    /// instance, another teleport counter) re-seeds instead of being judged. The C# kept
    /// baselines fresh through child-transform finalizes and the `DisabledSetPosition` setter;
    /// without them a ride or a teleport read as a 40 to 3000 m/s run.
    fn analyze(
        positions: &Baselines,
        id: u32,
        pos: [f32; 3],
        can_check: bool,
        max_speed: f64,
        delta_time: f32,
        ctx: SampleCtx,
    ) -> Option<f64> {
        let mut map = positions.lock().expect("sus poisoned");
        let fresh = Baseline {
            pos,
            skip_time: 0.0,
            ctx,
        };
        let Some(&last) = map.get(&id) else {
            map.insert(id, fresh);
            return None;
        };
        let elapsed = (ctx.now_ms - last.ctx.now_ms) as f32 / 1000.0;
        let stale = ctx.instance_id != last.ctx.instance_id
            || ctx.teleport_seq != last.ctx.teleport_seq
            || !(0.0..=MAX_BASELINE_AGE).contains(&elapsed);
        if stale {
            map.insert(id, fresh);
            return None;
        }
        let interval = elapsed.max(delta_time);
        let mut skip_time = last.skip_time;
        let (dx, dy) = (pos[0] - last.pos[0], pos[1] - last.pos[1]);
        let mut flagged = None;
        if can_check && (dx != 0.0 || dy != 0.0) && skip_time <= 0.0 {
            let observed = f64::from((dx * dx + dy * dy).sqrt() / interval);
            if observed > max_speed {
                skip_time = SKIP_TIME_AFTER_REPORT;
                flagged = Some(observed);
            }
        }
        skip_time -= interval;
        map.insert(
            id,
            Baseline {
                pos,
                skip_time,
                ctx,
            },
        );
        flagged
    }

    /// `AnalyzePlayerDeltaMovement(player, deltaTime)` (the analysis part): `Some(speed)` when
    /// the player is reported.
    pub fn analyze_player_delta_movement(
        &self,
        player_id: u32,
        world_pos: [f32; 3],
        is_riding: bool,
        disabled_set_position: bool,
        delta_time: f32,
    ) -> Option<f64> {
        self.analyze_player_delta_movement_at(
            player_id,
            world_pos,
            is_riding,
            disabled_set_position,
            delta_time,
            SampleCtx::default(),
        )
    }

    /// [`Self::analyze_player_delta_movement`] with the sample's time / instance / teleport
    /// counter (the staleness checks).
    pub fn analyze_player_delta_movement_at(
        &self,
        player_id: u32,
        world_pos: [f32; 3],
        is_riding: bool,
        disabled_set_position: bool,
        delta_time: f32,
        ctx: SampleCtx,
    ) -> Option<f64> {
        Self::analyze(
            &self.last_player_positions,
            player_id,
            world_pos,
            !is_riding && !disabled_set_position,
            PLAYER_CHECK_SPEED,
            delta_time,
            ctx,
        )
    }

    /// `AnalyzePlayerDeltaMovement` including the `audit_char_sus` entry.
    pub async fn analyze_player(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        is_riding: bool,
        delta_time: f32,
    ) {
        let pos = world_position(server, player);
        let (id, name, disabled, ctx) = {
            let c = player.data();
            (
                c.id,
                c.unit.name.clone(),
                c.disabled_set_position,
                SampleCtx {
                    now_ms: crate::world::now_ms(),
                    instance_id: c.unit.transform.instance_id,
                    teleport_seq: c.teleport_seq,
                },
            )
        };
        if let Some(speed) =
            self.analyze_player_delta_movement_at(id, pos, is_riding, disabled, delta_time, ctx)
        {
            self.log_activity_player(
                server,
                CATEGORY_CHEATING,
                Some(player),
                &format!(
                    "Player {name} seems to be moving a bit fast {speed:.1} m/s (max {PLAYER_CHECK_SPEED:.1})"
                ),
            )
            .await;
        }
    }

    /// `ResetAnalyzePlayerDeltaMovement(playerId)`.
    pub fn reset_analyze_player_delta_movement(&self, player_id: u32) {
        self.last_player_positions
            .lock()
            .expect("sus poisoned")
            .remove(&player_id);
    }

    /// `AnalyzeMountDeltaMovement(pet, deltaTime)` (the analysis part).
    pub fn analyze_mount_delta_movement(
        &self,
        pet_id: u32,
        world_pos: [f32; 3],
        disabled_set_position: bool,
        delta_time: f32,
    ) -> Option<f64> {
        self.analyze_mount_delta_movement_at(
            pet_id,
            world_pos,
            disabled_set_position,
            delta_time,
            SampleCtx::default(),
        )
    }

    /// [`Self::analyze_mount_delta_movement`] with the sample's time / instance.
    pub fn analyze_mount_delta_movement_at(
        &self,
        pet_id: u32,
        world_pos: [f32; 3],
        disabled_set_position: bool,
        delta_time: f32,
        ctx: SampleCtx,
    ) -> Option<f64> {
        Self::analyze(
            &self.last_pet_positions,
            pet_id,
            world_pos,
            !disabled_set_position,
            MOUNT_CHECK_SPEED,
            delta_time,
            ctx,
        )
    }

    /// The report text of a mount (`Pet {name} from {owner} seems to be moving ...`).
    pub fn mount_report(pet_name: &str, owner_name: Option<&str>, speed: f64) -> String {
        format!(
            "Pet {pet_name} from {} seems to be moving a bit fast {speed:.1} m/s (max {MOUNT_CHECK_SPEED:.1})",
            owner_name.unwrap_or("")
        )
    }

    /// `ResetAnalyzeMountDeltaMovement(petId)`.
    pub fn reset_analyze_mount_delta_movement(&self, pet_id: u32) {
        self.last_pet_positions
            .lock()
            .expect("sus poisoned")
            .remove(&pet_id);
    }
}

/// `player.Transform.World.Position`.
fn world_position(server: &GameServer, player: &PlayerRef) -> [f32; 3] {
    let obj = WorldObject::from(player);
    match server.world.world_of(&obj) {
        Some(w) => {
            let p = w.world_pos_rot(&obj);
            [p.x, p.y, p.z]
        }
        None => {
            let c = player.data();
            [c.unit.transform.x, c.unit.transform.y, c.unit.transform.z]
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn finalize_check_runs_every_five_seconds() {
        let s = SusManager::default();
        assert!(s.finalize_check(7, 1_000)); // first finalize
        assert!(!s.finalize_check(7, 3_000));
        assert!(!s.finalize_check(7, 5_999));
        assert!(s.finalize_check(7, 6_000));
        assert!(!s.finalize_check(7, 6_100));
        s.forget_object(7);
        assert!(s.finalize_check(7, 6_200));
    }

    use super::*;

    #[test]
    fn speed_analysis() {
        let s = SusManager::default();
        // First call only records.
        assert_eq!(
            s.analyze_player_delta_movement(1, [0.0; 3], false, false, 5.0),
            None
        );
        // 100 m in 5 s = 20 m/s: fine.
        assert_eq!(
            s.analyze_player_delta_movement(1, [100.0, 0.0, 0.0], false, false, 5.0),
            None
        );
        // 200 m in 5 s = 40 m/s (Z ignored): reported once, then 15 s skip.
        let r = s.analyze_player_delta_movement(1, [300.0, 0.0, 500.0], false, false, 5.0);
        assert_eq!(r, Some(40.0));
        assert_eq!(
            s.analyze_player_delta_movement(1, [600.0, 0.0, 0.0], false, false, 5.0),
            None
        );
        // Riding players are not checked.
        s.reset_analyze_player_delta_movement(2);
        s.analyze_player_delta_movement(2, [0.0; 3], true, false, 5.0);
        assert_eq!(
            s.analyze_player_delta_movement(2, [1000.0, 0.0, 0.0], true, false, 5.0),
            None
        );
        // Mounts: 27.5 m/s threshold.
        s.analyze_mount_delta_movement(7, [0.0; 3], false, 5.0);
        assert_eq!(
            s.analyze_mount_delta_movement(7, [137.0, 0.0, 0.0], false, 5.0),
            None
        );
        assert!(s
            .analyze_mount_delta_movement(7, [300.0, 0.0, 0.0], false, 5.0)
            .is_some());
        assert_eq!(
            SusManager::mount_report("Horse", None, 32.6),
            "Pet Horse from  seems to be moving a bit fast 32.6 m/s (max 27.5)"
        );
    }

    fn at(now_ms: i64, instance_id: u32, teleport_seq: u32) -> SampleCtx {
        SampleCtx {
            now_ms,
            instance_id,
            teleport_seq,
        }
    }

    #[test]
    fn a_stale_baseline_reseeds_instead_of_reporting() {
        let s = SusManager::default();
        let check =
            |pos: [f32; 3], ctx| s.analyze_player_delta_movement_at(1, pos, false, false, 5.0, ctx);
        assert_eq!(check([0.0; 3], at(0, 0, 0)), None);
        // 300 m after 30 s of no samples (riding, a loading screen, standing still): reseed.
        assert_eq!(check([300.0, 0.0, 0.0], at(30_000, 0, 0)), None);
        // The baseline is fresh again: 300 m in 5 s is reported.
        assert!(check([600.0, 0.0, 0.0], at(35_000, 0, 0)).is_some());
    }

    #[test]
    fn an_instance_change_or_teleport_reseeds_instead_of_reporting() {
        let s = SusManager::default();
        let check =
            |pos: [f32; 3], ctx| s.analyze_player_delta_movement_at(1, pos, false, false, 5.0, ctx);
        assert_eq!(check([0.0; 3], at(0, 0, 0)), None);
        // Dungeon entry: another instance, 15 km away.
        assert_eq!(check([15_000.0, 0.0, 0.0], at(5_000, 100, 0)), None);
        // A teleport inside the instance (`set_disabled_set_position(true)` bumped the counter).
        assert_eq!(check([15_040.0, 0.0, 0.0], at(10_000, 100, 0)), None);
        assert_eq!(check([16_000.0, 0.0, 0.0], at(15_000, 100, 1)), None);
        // Without a teleport the same jump is a real report.
        assert!(check([17_000.0, 0.0, 0.0], at(20_000, 100, 1)).is_some());
    }

    #[test]
    fn the_speed_is_taken_over_the_real_interval_but_real_speeding_is_still_reported() {
        let s = SusManager::default();
        let check =
            |pos: [f32; 3], ctx| s.analyze_player_delta_movement_at(1, pos, false, false, 5.0, ctx);
        assert_eq!(check([0.0; 3], at(0, 0, 0)), None);
        // 200 m in 8 s = 25 m/s is not above the limit (it would be 40 m/s over a fixed 5 s).
        assert_eq!(check([200.0, 0.0, 0.0], at(8_000, 0, 0)), None);
        // 150 m in 5 s = 30 m/s.
        let speed = check([350.0, 0.0, 0.0], at(13_000, 0, 0)).expect("reported");
        assert!((speed - 30.0).abs() < 1e-6, "{speed}");
    }

    #[test]
    fn a_reset_after_riding_or_a_mate_respawn_drops_the_baseline() {
        let s = SusManager::default();
        s.analyze_mount_delta_movement(7, [0.0; 3], false, 5.0);
        s.reset_analyze_mount_delta_movement(7);
        assert_eq!(
            s.analyze_mount_delta_movement(7, [5_000.0, 0.0, 0.0], false, 5.0),
            None
        );
    }
}
