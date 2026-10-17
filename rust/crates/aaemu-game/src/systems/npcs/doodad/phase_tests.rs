//! Phase-machine tests: synthetic func groups (Timer / Final / Tod / RatioChange / Growth /
//! PulseTrigger, the `ListGroupId` recursion guard, `NextPhase == -1`, `OverridePhase`) and the
//! real template 2336 (gated by `AAEMU_COMPACT_DB`).

use std::sync::Arc;

use super::*;
use crate::data::world_templates::WorldSpawnPosition;
use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::logic;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::spawner::{DoodadSpawner, DoodadSpawnerDef, SpawnerBase};
use crate::systems::npcs::doodad::templates::*;
use crate::test_support::*;
use crate::world::{PlayerRef, WorldInstance};

const TEMPLATE: u32 = 100;
const FAKE_SKILL: u32 = 999;

/// Synthetic `DoodadData` builder.
#[derive(Default)]
struct Builder {
    data: DoodadData,
}

impl Builder {
    fn template(mut self, id: u32, groups: &[(u32, u32)]) -> Self {
        let mut t = DoodadTemplate {
            id,
            ..DoodadTemplate::default()
        };
        for &(gid, kind) in groups {
            let g = DoodadFuncGroups {
                id: gid,
                almighty: id,
                group_kind_id: kind,
                ..DoodadFuncGroups::default()
            };
            t.func_groups.push(g.clone());
            self.data
                .all_func_groups_index
                .insert(gid, self.data.all_func_groups.len());
            self.data.all_func_groups.push(g);
        }
        self.data.template_order.push(id);
        self.data.templates.insert(id, t);
        self
    }

    /// A `DoodadFuncFakeUse` (both skill ids 0: always sets `ToNextPhase`).
    fn fake_use(mut self, group: u32, func_id: u32, next_phase: i32) -> Self {
        self.data
            .func_templates
            .entry("DoodadFuncFakeUse")
            .or_default()
            .insert(
                func_id,
                FuncTemplate::FakeUse(DoodadFuncFakeUse {
                    id: func_id,
                    ..DoodadFuncFakeUse::default()
                }),
            );
        let key = self.data.funcs_by_id.len() as u32 + 1;
        let f = DoodadFunc {
            group_id: group,
            func_id,
            func_key: key,
            func_type: "DoodadFuncFakeUse".into(),
            next_phase,
            ..DoodadFunc::default()
        };
        self.data.funcs_by_id.insert(key, f.clone());
        self.data.funcs_by_groups.entry(group).or_default().push(f);
        self
    }

    fn phase_func(mut self, group: u32, t: FuncTemplate) -> Self {
        let (class, id) = (t.func_type(), t.id());
        self.data
            .phase_func_templates
            .entry(class)
            .or_default()
            .insert(id, t);
        self.data
            .phase_funcs
            .entry(group)
            .or_default()
            .push(DoodadPhaseFunc {
                group_id: group,
                func_id: id,
                func_type: class.into(),
            });
        self
    }

    fn server(self) -> GameServer {
        let mut gd = GameData::empty();
        gd.systems.npcs.doodad = self.data;
        let s = test_server_with(gd);
        s.systems.npcs.doodads.tasks.set_manual(true);
        s
    }
}

struct Fx {
    server: GameServer,
    client: TestClient,
    player: PlayerRef,
    world: Arc<WorldInstance>,
}

fn fx(b: Builder) -> Fx {
    let server = b.server();
    let mut client = TestClient::new(&server, 1, 1001);
    let player_obj_id = server.ids.object.get_next_id().unwrap();
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        player_obj_id,
    );
    client.drain();
    let world = server.world.world(0).unwrap();
    Fx {
        server,
        client,
        player,
        world,
    }
}

impl Fx {
    /// `Create(skipPhaseInitialization: true)`, placed and spawned, then `InitDoodad`.
    fn doodad(&mut self, template: u32) -> DoodadRef {
        let d = manager::create(&self.server, &self.world, 0x500, template, None, true).unwrap();
        d.data().transform.set_xyz(120.0, 130.0, 100.0);
        manager::spawn(&self.server, &self.world, &d);
        manager::init_doodad(&self.server, &self.world, &d);
        d
    }

    fn caster(&self) -> WorldObject {
        WorldObject::from(&self.player)
    }

    fn use_(&self, d: &DoodadRef) {
        manager::use_doodad(
            &self.server,
            &self.world,
            d,
            Some(&self.caster()),
            FAKE_SKILL,
            0,
        );
    }

    fn advance(&self, ms: u64) -> usize {
        self.server
            .systems
            .npcs
            .doodads
            .tasks
            .advance(&self.server, ms)
    }

    /// Func group ids of the drained `SCDoodadPhaseChangedPacket`s.
    fn phase_packets(&mut self) -> Vec<u32> {
        self.client
            .drain()
            .into_iter()
            .filter(|f| f.0 == sc::SCDoodadPhaseChangedPacket)
            .map(|f| u32::from_le_bytes(f.1[3..7].try_into().unwrap()))
            .collect()
    }
}

fn timer(id: u32, delay: i32, next_phase: i32) -> FuncTemplate {
    FuncTemplate::Timer(DoodadFuncTimer {
        id,
        delay,
        next_phase,
        ..DoodadFuncTimer::default()
    })
}

fn ratio(id: u32, ratio: i32, next_phase: i32) -> FuncTemplate {
    FuncTemplate::RatioChange(DoodadFuncRatioChange {
        id,
        ratio,
        next_phase,
    })
}

fn ratio_respawn(id: u32, ratio: i32, spawn_doodad_id: u32) -> FuncTemplate {
    FuncTemplate::RatioRespawn(DoodadFuncRatioRespawn {
        id,
        ratio,
        spawn_doodad_id,
    })
}

/// A real spawner for `unit_id` in the test world; `id > 0` because `RatioRespawn` ignores
/// spawners without an index.
fn new_spawner(unit_id: u32, id: u32) -> Arc<DoodadSpawner> {
    Arc::new(DoodadSpawner::new(
        DoodadSpawnerDef {
            base: SpawnerBase {
                id,
                unit_id,
                position: WorldSpawnPosition {
                    world_id: 0,
                    zone_id: TEST_ZONE,
                    x: 120.0,
                    y: 130.0,
                    z: 100.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            scale: 1.0,
            count: 1,
            related_ids: None,
            respawn_doodad_template_id: 0,
        },
        Some(0),
    ))
}

fn group(d: &DoodadRef) -> u32 {
    d.data().func_group_id
}

#[tokio::test]
async fn loaded_player_doodad_is_spawned_before_its_immediate_final_phase() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Final(DoodadFuncFinal {
                id: 1,
                after: 0,
                respawn: false,
                ..Default::default()
            }),
        );
    let f = fx(b);
    let doodad = manager::create(&f.server, &f.world, 0, TEMPLATE, None, true).unwrap();
    doodad.data().is_persistent = true;
    let spawns = f.server.systems.npcs.spawns.get_or_create(&f.world);
    spawns.add_player_doodad(doodad.clone());
    spawns.spawn_player_doodads(&f.server);
    assert!(doodad.data().phase.deleted);
    assert!(f.world.doodad(doodad.obj_id).is_none());
    assert!(spawns.get_all_player_doodads().is_empty());
}

#[tokio::test]
async fn family_pulses_follow_flags_and_owner_scope_in_both_load_orders() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 1,
                family_id: 7,
            }),
        )
        .phase_func(
            20,
            FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                id: 2,
                family_id: 7,
            }),
        )
        .phase_func(
            21,
            FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                id: 2,
                family_id: 7,
            }),
        )
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 3,
                flag: true,
                next_phase: 21,
            }),
        )
        .phase_func(
            21,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 4,
                flag: false,
                next_phase: 20,
            }),
        );
    let f = fx(b);
    let make = |template| {
        let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
        d.data().transform.set_xyz(101.0, 100.0, 100.0);
        manager::spawn(&f.server, &f.world, &d);
        manager::init_doodad(&f.server, &f.world, &d);
        d
    };
    let subscriber = make(101);
    let other_owner = make(101);
    other_owner.data().owner_id = 99;
    let provider = make(100);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), provider.clone());
    assert_eq!(
        group(&subscriber),
        20,
        "registration alone must not activate a trigger"
    );
    logic::emit(&ctx, None, true);
    assert_eq!(group(&subscriber), 21);
    assert_eq!(group(&other_owner), 20);
    let late_subscriber = make(101);
    assert_eq!(
        group(&late_subscriber),
        21,
        "subscriber must adopt an existing provider"
    );
    logic::emit(&ctx, None, false);
    assert_eq!(group(&subscriber), 20);
    assert_eq!(group(&late_subscriber), 20);
}

#[tokio::test]
async fn use_then_timer_returns_to_start_phase() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, 11)
        .phase_func(11, timer(5, 30_000, 10));
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    assert_eq!(group(&d), 10);
    assert_eq!(f.phase_packets(), vec![10]); // InitDoodad -> DoChangePhase(start)

    f.use_(&d);
    assert_eq!(group(&d), 11);
    assert_eq!(f.phase_packets(), vec![11]);
    assert_eq!(
        f.server.systems.npcs.doodads.tasks.pending_delays(),
        vec![30_000]
    );
    assert!(d.data().phase.func_task != 0);
    // TimeLeft shows the growth time of the timer.
    assert!(d.data().time_left(crate::world::now_ms()) > 29_000);

    assert_eq!(f.advance(29_999), 0);
    assert_eq!(group(&d), 11);
    assert_eq!(f.advance(1), 1);
    assert_eq!(group(&d), 10);
    assert_eq!(f.phase_packets(), vec![10]);
    assert_eq!(d.data().phase.func_task, 0);
}

#[tokio::test]
async fn phase_change_cancels_pending_timer() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, 11)
        .fake_use(11, 2, 10)
        .phase_func(11, timer(5, 30_000, 10));
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.use_(&d);
    assert_eq!(group(&d), 11);
    f.use_(&d); // back to 10 by hand: the timer is cancelled
    assert_eq!(group(&d), 10);
    f.phase_packets();
    // The phase is entered once, and its canceled timer cannot change the new phase.
    assert_eq!(f.advance(30_000), 1);
    assert_eq!(group(&d), 10);
    assert!(f.phase_packets().is_empty());
}

#[tokio::test]
async fn tod_overrides_phase_and_registers_triggers() {
    // 12:00 game time: tod 600 (6:00) has passed, tod 2000 (20:00) has not.
    let tod = |id, tod: u32, next_phase| {
        FuncTemplate::Tod(DoodadFuncTod {
            id,
            tod,
            next_phase,
            tod_as_hours: crate::systems::npcs::doodad::data::tod_as_hours(id, tod),
        })
    };
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (12, 2), (13, 2)])
        .phase_func(10, tod(1, 2000, 13))
        .phase_func(10, tod(2, 600, 12))
        .phase_func(12, tod(3, 2000, 10));
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    assert_eq!(group(&d), 12);
    assert_eq!(f.phase_packets(), vec![12]);
    assert_eq!(d.data().phase.tod_triggers, vec![(20.0, 10)]);

    // Enter the triggered phase, including its functions, on the scheduled task.
    manager::on_time_of_day_change(&f.server, 20.02, 19.98);
    f.advance(0);
    assert_eq!(group(&d), 12); // the synthetic server is still at 12:00
    assert_eq!(f.phase_packets(), vec![12]);
}

#[tokio::test]
async fn ratio_change_by_roll() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (21, 2)])
        .phase_func(10, ratio(1, 500, 21));
    let mut f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(9000));
    let d = f.doodad(TEMPLATE);
    assert_eq!(group(&d), 10);

    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(500));
    let d2 = manager::create(&f.server, &f.world, 0x501, TEMPLATE, None, true).unwrap();
    manager::init_doodad(&f.server, &f.world, &d2);
    assert_eq!(group(&d2), 21);
    assert_eq!(d2.data().phase.override_phase, 0);
    f.phase_packets();
}

#[tokio::test]
async fn recursion_guard_stops_ping_pong() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, 11)
        .phase_func(10, ratio(1, 10_000, 11))
        .phase_func(11, ratio(2, 10_000, 10));
    let mut f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let d = f.doodad(TEMPLATE);
    // 10 -> 11 -> 10 (in ListGroupId, has funcs) -> stop
    assert_eq!(group(&d), 10);
    assert!(d.data().phase.list_group_id.is_empty());
    assert_eq!(f.phase_packets(), vec![10]);
}

#[tokio::test]
async fn ratio_respawn_self_reference_keeps_one_live_doodad_and_resets_guard() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, ratio_respawn(1, 10_000, TEMPLATE));
    let f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 501);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(!Arc::ptr_eq(&first, &last));
    assert!(first.data().phase.deleted);
    assert!(!f.world.all_doodads().iter().any(|d| Arc::ptr_eq(d, &first)));
    assert!(!last.data().phase.deleted);
    assert_eq!(last.data().func_group_id, 10);
    assert!(f
        .world
        .doodad(last.obj_id)
        .is_some_and(|d| Arc::ptr_eq(&d, &last)));
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.spawned().len(), 1);
    assert!(Arc::ptr_eq(&spawner.spawned()[0], &last));
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

#[tokio::test]
async fn ratio_respawn_two_template_cycle_is_bounded_with_a_live_last_doodad() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .template(TEMPLATE + 1, &[(20, 1)])
        .phase_func(10, ratio_respawn(1, 10_000, TEMPLATE + 1))
        .phase_func(20, ratio_respawn(2, 10_000, TEMPLATE));
    let f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 502);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(!Arc::ptr_eq(&first, &last));
    assert!(first.data().phase.deleted);
    assert!(!f.world.all_doodads().iter().any(|d| Arc::ptr_eq(d, &first)));
    // T1 -> T2 -> T1 is rejected on the repeated (T1, 10) source, so the newest T1 survives.
    assert_eq!(last.data().template_id, TEMPLATE);
    assert_eq!(last.data().func_group_id, 10);
    assert!(!last.data().phase.deleted);
    assert!(f
        .world
        .doodad(last.obj_id)
        .is_some_and(|d| Arc::ptr_eq(&d, &last)));
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.spawned().len(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

#[tokio::test]
async fn ratio_respawn_finite_chain_reaches_the_last_template() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .template(TEMPLATE + 1, &[(20, 1)])
        .template(TEMPLATE + 2, &[(30, 1)])
        .phase_func(10, ratio_respawn(1, 10_000, TEMPLATE + 1))
        .phase_func(20, ratio_respawn(2, 10_000, TEMPLATE + 2));
    let f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 503);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(!Arc::ptr_eq(&first, &last));
    assert!(first.data().phase.deleted);
    assert!(!f.world.all_doodads().iter().any(|d| Arc::ptr_eq(d, &first)));
    assert_eq!(last.data().template_id, TEMPLATE + 2);
    assert_eq!(last.data().func_group_id, 30);
    assert!(!last.data().phase.deleted);
    assert!(f
        .world
        .doodad(last.obj_id)
        .is_some_and(|d| Arc::ptr_eq(&d, &last)));
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

#[tokio::test]
async fn ratio_respawn_repeat_through_a_chance_hop_runs_until_the_depth_limit() {
    // A self-respawn with a chance below 1 is not an unconditional cycle: the C# lets it recurse
    // and the roll ends it. With every roll passing, only the depth limit stops it.
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, ratio_respawn(1, 5_000, TEMPLATE));
    let f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 506);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(!Arc::ptr_eq(&first, &last));
    assert!(first.data().phase.deleted);
    assert_eq!(spawner.ratio_respawn_peak_depth(), 8);
    assert_eq!(last.data().template_id, TEMPLATE);
    assert!(!last.data().phase.deleted);
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

#[tokio::test]
async fn ratio_respawn_chance_cycle_ends_when_the_roll_fails() {
    // The 2768 shape: the initial phase reaches the respawning phase by chance, and the
    // respawning phase respawns the same template at 20%, otherwise into another template.
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 2)])
        .template(TEMPLATE + 1, &[(30, 1)])
        .phase_func(10, ratio(1, 500, 20))
        .phase_func(20, ratio_respawn(2, 2_000, TEMPLATE))
        .phase_func(20, ratio_respawn(3, 2_000, TEMPLATE + 1));
    let f = fx(b);
    // Per phase func: 10:RatioChange hits -> 20; 20:self-respawn hits -> new TEMPLATE, its 10 hits
    // -> 20 again (the repeated source); self-respawn misses, the second hits -> TEMPLATE + 1.
    f.server
        .systems
        .npcs
        .doodads
        .queue_phase_ratios(&[100, 100, 100, 9000, 100]);
    let spawner = new_spawner(TEMPLATE, 507);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(first.data().phase.deleted);
    assert_eq!(spawner.ratio_respawn_peak_depth(), 2);
    assert_eq!(last.data().template_id, TEMPLATE + 1);
    assert_eq!(last.data().func_group_id, 30);
    assert!(!last.data().phase.deleted);
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

fn tod_func(id: u32, tod: u32, next_phase: i32) -> FuncTemplate {
    FuncTemplate::Tod(DoodadFuncTod {
        id,
        tod,
        next_phase,
        tod_as_hours: crate::systems::npcs::doodad::data::tod_as_hours(id, tod),
    })
}

/// The West Gate board shape (4345 / 4349): a day board that swaps to its night variant at 20:00
/// and the variant back at 06:00, both through a certain `DoodadFuncRatioRespawn`.
fn day_night_pair() -> Builder {
    Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .template(TEMPLATE + 1, &[(20, 1), (21, 2)])
        .phase_func(10, tod_func(1, 2000, 11))
        .phase_func(11, ratio_respawn(2, 10_000, TEMPLATE + 1))
        .phase_func(20, tod_func(3, 600, 21))
        .phase_func(21, ratio_respawn(4, 10_000, TEMPLATE))
}

#[tokio::test]
async fn ratio_respawn_day_night_pair_settles_on_the_variant_at_night() {
    let f = fx(day_night_pair());
    f.server.time.set(22.0);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 520);
    let first = spawner.spawn(&f.server, 0).expect("spawns the day board");
    let last = spawner.last().expect("the replacement is tracked");
    // 22:00 >= 20:00 catches the day board up to its respawn phase; the variant, created at
    // this time of day, must not take its own 06:00 catch-up and swap straight back.
    assert!(first.data().phase.deleted);
    assert_eq!(last.data().template_id, TEMPLATE + 1);
    assert_eq!(group(&last), 20);
    assert!(!last.data().phase.deleted);
    assert_eq!(spawner.ratio_respawn_peak_depth(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
    assert_eq!(f.world.all_doodads().len(), 1);
    assert!(!last.data().phase.suppress_tod_catch_up);
    assert_eq!(last.data().phase.tod_triggers, vec![(6.0, 21)]);
}

#[tokio::test]
async fn ratio_respawn_variant_returns_to_the_day_board_when_the_clock_crosses_its_tod() {
    let f = fx(day_night_pair());
    f.server.time.set(22.0);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 521);
    spawner.spawn(&f.server, 0).expect("spawns the day board");
    let variant = spawner.last().expect("the variant is tracked");
    assert_eq!(variant.data().template_id, TEMPLATE + 1);

    f.server.time.set(6.01);
    manager::on_time_of_day_change(&f.server, 6.01, 5.99);
    f.advance(0);
    let day = spawner.last().expect("the day board is tracked");
    assert!(variant.data().phase.deleted);
    assert_eq!(day.data().template_id, TEMPLATE);
    assert_eq!(group(&day), 10);
    assert!(!day.data().phase.deleted);
    assert_eq!(spawner.ratio_respawn_peak_depth(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
    assert_eq!(f.world.all_doodads().len(), 1);
}

#[tokio::test]
async fn ratio_respawn_tod_fan_out_picks_one_variant_or_keeps_the_board() {
    // The 6175 shape: the board fans out at 20% per variant after its 20:00 Tod; the variants
    // return at 02:00 with a certain respawn.
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .template(TEMPLATE + 1, &[(20, 1), (21, 2)])
        .template(TEMPLATE + 2, &[(30, 1), (31, 2)])
        .phase_func(10, tod_func(1, 2000, 11))
        .phase_func(11, ratio_respawn(2, 2_000, TEMPLATE + 1))
        .phase_func(11, ratio_respawn(3, 2_000, TEMPLATE + 2))
        .phase_func(20, tod_func(4, 200, 21))
        .phase_func(21, ratio_respawn(5, 10_000, TEMPLATE))
        .phase_func(30, tod_func(6, 200, 31))
        .phase_func(31, ratio_respawn(7, 10_000, TEMPLATE));
    let f = fx(b);
    f.server.time.set(21.0);
    // One roll per phase func: the board's Tod, then the two respawns (first misses, second
    // hits -> TEMPLATE + 2), then the variant's Tod.
    f.server
        .systems
        .npcs
        .doodads
        .queue_phase_ratios(&[0, 9000, 100, 0]);
    let hit = new_spawner(TEMPLATE, 522);
    hit.spawn(&f.server, 0).expect("spawns the board");
    let variant = hit.last().expect("the variant is tracked");
    assert_eq!(variant.data().template_id, TEMPLATE + 2);
    assert_eq!(group(&variant), 30);
    assert_eq!(hit.ratio_respawn_peak_depth(), 1);
    assert_eq!(hit.ratio_respawn_chain_depth(), 0);

    // Both rolls miss: the board stays in its respawning phase, with no replacement and no
    // rejected chain.
    f.server
        .systems
        .npcs
        .doodads
        .queue_phase_ratios(&[0, 9000, 9000]);
    let miss = new_spawner(TEMPLATE, 523);
    let board = miss.spawn(&f.server, 0).expect("spawns the board");
    assert_eq!(board.data().template_id, TEMPLATE);
    assert_eq!(group(&board), 11);
    assert!(!board.data().phase.deleted);
    assert_eq!(miss.ratio_respawn_peak_depth(), 0);
    assert_eq!(f.world.all_doodads().len(), 2);
}

#[tokio::test]
async fn ratio_respawn_deep_acyclic_chain_stops_at_the_depth_limit() {
    let mut b = Builder::default();
    for i in 0..10u32 {
        b = b.template(TEMPLATE + i, &[(10 + i, 1)]);
    }
    for i in 0..9u32 {
        b = b.phase_func(10 + i, ratio_respawn(i + 1, 10_000, TEMPLATE + i + 1));
    }
    let f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 505);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(!Arc::ptr_eq(&first, &last));
    assert!(first.data().phase.deleted);
    assert!(!f.world.all_doodads().iter().any(|d| Arc::ptr_eq(d, &first)));
    // Sources T0..T7 occupy the 8 entries; T8 survives rejection and T9 is never reached.
    assert_eq!(last.data().template_id, TEMPLATE + 8);
    assert_eq!(last.data().func_group_id, 18);
    assert!(!last.data().phase.deleted);
    assert!(f
        .world
        .doodad(last.obj_id)
        .is_some_and(|d| Arc::ptr_eq(&d, &last)));
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.spawned().len(), 1);
    assert!(Arc::ptr_eq(&spawner.spawned()[0], &last));
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

#[tokio::test]
async fn ratio_respawn_guard_cleans_up_for_a_later_independent_respawn() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .template(TEMPLATE + 1, &[(20, 1)])
        .phase_func(10, ratio_respawn(1, 10_000, TEMPLATE + 1))
        .phase_func(20, ratio_respawn(2, 10_000, TEMPLATE));
    let f = fx(b);
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    let spawner = new_spawner(TEMPLATE, 504);
    let first = spawner.spawn(&f.server, 0).expect("spawns the template");
    assert!(!Arc::ptr_eq(&first, &spawner.last().unwrap()));
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);

    // Despawn the retained survivor and respawn independently: the same cycle must run again
    // (a leaked chain entry would reject the first hop and keep the first doodad).
    let stopped = spawner.last().unwrap();
    spawner.despawn(&f.server, &stopped);
    assert!(stopped.data().phase.deleted);
    assert!(!f
        .world
        .all_doodads()
        .iter()
        .any(|d| Arc::ptr_eq(d, &stopped)));
    let first_again = spawner.spawn(&f.server, 0).expect("respawns the template");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(!Arc::ptr_eq(&first_again, &last));
    assert!(!last.data().phase.deleted);
    assert_eq!(last.data().template_id, TEMPLATE);
    assert!(f
        .world
        .doodad(last.obj_id)
        .is_some_and(|d| Arc::ptr_eq(&d, &last)));
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

#[tokio::test]
async fn clout_with_origin_source_changes_only_its_own_phase() {
    // Doodad 5007 (NPC 10119's summon) next to the Amber Boulder 4252 (quest 3447): the clout's
    // area may start at a nearby doodad, but its NextPhase / Final belong to the summon.
    const BYSTANDER: u32 = 200;
    let b = Builder::default()
        .template(BYSTANDER, &[(20, 1)])
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_func(
            10,
            FuncTemplate::Clout(DoodadFuncClout {
                id: 1,
                duration: 1000,
                next_phase: 11,
                use_origin_source: true,
                ..DoodadFuncClout::default()
            }),
        )
        .phase_func(
            11,
            FuncTemplate::Final(DoodadFuncFinal {
                id: 2,
                after: 2000,
                ..DoodadFuncFinal::default()
            }),
        );
    let f = fx(b);
    let bystander = manager::create(&f.server, &f.world, 0x600, BYSTANDER, None, true).unwrap();
    bystander.data().transform.set_xyz(101.0, 101.0, 100.0);
    manager::spawn(&f.server, &f.world, &bystander);
    manager::init_doodad(&f.server, &f.world, &bystander);

    // Summoned by the player 1.4 m away from the bystander, then initialised before it spawns.
    let summon = manager::create(
        &f.server,
        &f.world,
        0x500,
        TEMPLATE,
        Some(&f.caster()),
        true,
    )
    .unwrap();
    summon.data().transform.set_xyz(100.0, 100.0, 100.0);
    manager::init_doodad(&f.server, &f.world, &summon);
    manager::spawn(&f.server, &f.world, &summon);
    assert_eq!(group(&summon), 10);

    f.advance(1000);
    assert_eq!(group(&summon), 11);
    assert_eq!(group(&bystander), 20);
    f.advance(2000);
    assert!(summon.data().phase.deleted);
    assert!(!bystander.data().phase.deleted);
    assert!(f.world.doodad(0x600).is_some());
}

#[tokio::test]
async fn next_phase_minus_one_deletes() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, -1);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.client.drain();
    f.use_(&d);
    assert!(d.data().phase.deleted);
    assert!(f.world.doodad(0x500).is_none());
    assert!(f.client.drain_ops().contains(&sc::SCDoodadRemovedPacket));

    // A doodad with only a start group stays (Windstone id=1473).
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .fake_use(10, 1, -1);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.use_(&d);
    assert!(!d.data().phase.deleted);
    assert_eq!(group(&d), 10);
}

#[tokio::test]
async fn override_phase_wins_over_next_phase() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2), (30, 2)])
        .fake_use(10, 1, 11);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    d.data().phase.override_phase = 30; // e.g. DoodadFuncConditionalUse
    f.use_(&d);
    assert_eq!(group(&d), 30);
    assert_eq!(d.data().phase.override_phase, 0);
    assert_eq!(f.phase_packets(), vec![10, 30]);
}

#[tokio::test]
async fn final_deletes_now_or_after() {
    let fin = |id, after| {
        FuncTemplate::Final(DoodadFuncFinal {
            id,
            after,
            ..DoodadFuncFinal::default()
        })
    };
    // After = 0: deleted at once, no phase packet.
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, fin(1, 0));
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    assert!(d.data().phase.deleted);
    assert!(f.phase_packets().is_empty());
    assert_eq!(f.server.systems.npcs.doodads.tasks.pending(), 0);

    // After = 5000: TimeLeft counts down, deleted when the task fires.
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, fin(1, 5000));
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    assert!(!d.data().phase.deleted);
    let left = d.data().time_left(crate::world::now_ms());
    assert!((4000..=5000).contains(&left), "{left}");
    assert_eq!(
        f.server.systems.npcs.doodads.tasks.pending_delays(),
        vec![5000]
    );
    f.advance(5000);
    assert!(d.data().phase.deleted);
    assert!(f.world.doodad(0x500).is_none());
}

#[tokio::test]
async fn growth_waits_then_changes_phase() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_func(
            10,
            FuncTemplate::Growth(DoodadFuncGrowth {
                id: 1,
                delay: 8000,
                start_scale: 500,
                end_scale: 1000,
                next_phase: 11,
            }),
        );
    let mut f = fx(b);
    f.server.systems.npcs.doodads.set_growth_rate(2.0);
    let d = f.doodad(TEMPLATE);
    assert_eq!(group(&d), 10);
    assert_eq!(
        f.server.systems.npcs.doodads.tasks.pending_delays(),
        vec![4000]
    );
    f.advance(4000);
    assert_eq!(group(&d), 11);
    assert_eq!(d.data().scale, 1.0); // C# Scale writes have no effect
    assert_eq!(f.phase_packets(), vec![10, 11]);
}

#[tokio::test]
async fn pulse_triggers_require_matching_signal_and_do_not_affect_other_objects() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(11, 1, 10)
        .phase_func(
            10,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 7,
                flag: true,
                next_phase: 11,
            }),
        );
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    // Entering a phase registers a listener; a character's presence is not a pulse.
    assert_eq!(group(&d), 10);
    let caster = f.caster();
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), d.clone());
    do_change_phase(&ctx, Some(&caster), 10);
    assert_eq!(group(&d), 10);
    super::super::logic::receive(&ctx, &d, Some(&caster), true);
    assert_eq!(group(&d), 11);
    f.use_(&d); // 11 -> 10: the row was triggered already, stays in 10
    assert_eq!(group(&d), 10);
    let other = manager::create(&f.server, &f.world, 0x501, TEMPLATE, None, true).unwrap();
    let other_ctx = DoodadCtx::new(&f.server, f.world.clone(), other.clone());
    do_change_phase(&other_ctx, Some(&caster), 10);
    assert_eq!(group(&other), 10);
    super::super::logic::receive(&ctx, &other, Some(&caster), true);
    assert_eq!(group(&other), 11);
}

#[tokio::test]
async fn tod_enters_phase_functions_cancels_old_timer_and_crosses_midnight() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2), (12, 2), (13, 2)])
        .phase_func(
            10,
            FuncTemplate::Timer(DoodadFuncTimer {
                id: 1,
                delay: 5000,
                next_phase: 13,
                ..Default::default()
            }),
        )
        .phase_func(
            11,
            FuncTemplate::Timer(DoodadFuncTimer {
                id: 2,
                delay: 1000,
                next_phase: 12,
                ..Default::default()
            }),
        );
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    d.data().phase.tod_triggers.push((23.99, 11));
    manager::on_time_of_day_change(&f.server, 0.01, 23.98);
    f.advance(0);
    assert_eq!(group(&d), 11);
    f.advance(1000);
    assert_eq!(group(&d), 12);
    f.advance(4000);
    assert_eq!(group(&d), 12);
}

#[tokio::test]
async fn interaction_count_waits_without_restarting_phase_timer() {
    let mut b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2), (12, 2)])
        .fake_use(10, 1, 11)
        .phase_func(
            10,
            FuncTemplate::Timer(DoodadFuncTimer {
                id: 1,
                delay: 5000,
                next_phase: 12,
                ..Default::default()
            }),
        );
    b.data.funcs_by_groups.get_mut(&10).unwrap()[0].count = 2;
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    let timer = d.data().phase.func_task;
    f.use_(&d);
    assert_eq!(group(&d), 10);
    assert_eq!(d.data().phase.func_task, timer);
    f.use_(&d);
    assert_eq!(group(&d), 11);
    f.advance(5000);
    assert_eq!(group(&d), 11);
}

#[tokio::test]
async fn old_timer_and_delete_do_not_mutate_replacement_object() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_func(
            10,
            FuncTemplate::Timer(DoodadFuncTimer {
                id: 1,
                delay: 1000,
                next_phase: 11,
                ..Default::default()
            }),
        );
    let mut f = fx(b);
    let old = f.doodad(TEMPLATE);
    tasks::schedule_delete(&f.server, &old);
    f.world.delete(&old);
    let replacement =
        manager::create(&f.server, &f.world, old.obj_id, TEMPLATE, None, true).unwrap();
    manager::spawn(&f.server, &f.world, &replacement);
    f.advance(1000);
    assert_eq!(group(&replacement), 10);
    assert!(f
        .world
        .doodad(replacement.obj_id)
        .is_some_and(|d| Arc::ptr_eq(&d, &replacement)));
    manager::delete(&f.server, &f.world, &old);
    assert!(f.world.doodad(replacement.obj_id).is_some());
}

#[tokio::test]
async fn phase_reservation_rejects_concurrent_use_but_allows_nested_calls() {
    let mut f = fx(Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, 11));
    let d = f.doodad(TEMPLATE);
    let guard = Operation::enter(&d).unwrap();
    assert!(Operation::enter(&d).is_some());
    std::thread::scope(|scope| {
        scope.spawn(|| f.use_(&d)).join().unwrap();
    });
    assert_eq!(group(&d), 10);
    drop(guard);
    f.use_(&d);
    assert_eq!(group(&d), 11);
}

/// Template 2336 (water the flowerbed, see the header of `Doodad.cs`) from the real DB.
#[tokio::test]
#[ignore]
async fn real_compact_db_template_2336() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    let fake_skill = match data.func_template(543, "DoodadFuncFakeUse") {
        Some(FuncTemplate::FakeUse(t)) if t.fake_skill_id > 0 => t.fake_skill_id,
        _ => FAKE_SKILL,
    };
    let mut f = fx(Builder { data });
    let d = f.doodad(2336);
    // Start phase 4651; its ToD (20:00) has not passed at 12:00.
    assert_eq!(group(&d), 4651);
    assert_eq!(f.phase_packets(), vec![4651]);
    assert!(d.data().phase.tod_triggers.contains(&(20.0, 5136)));

    manager::use_doodad(&f.server, &f.world, &d, Some(&f.caster()), fake_skill, 0);
    assert_eq!(group(&d), 4652);
    assert_eq!(f.phase_packets(), vec![4652]);
    assert_eq!(
        f.server.systems.npcs.doodads.tasks.pending_delays(),
        vec![30_000]
    );

    f.advance(30_000);
    assert_eq!(group(&d), 4651);
    assert_eq!(f.phase_packets(), vec![4651]);
}

/// Weather marker 2768 (func group 7055 respawns into 2768 / 3085 / 3086 / 3225 / 3226 at 20% each
/// via DoodadFuncRatioRespawn): the initial phase 7026 can reach 7055 synchronously through its
/// RatioChange funcs, so the replacement 2768 re-enters the same (template, group) source. That
/// legitimate chance cycle used to be rejected ("replacement chain rejected"), leaving the
/// replacement stuck in 7055 until its Final respawned the base template.
#[tokio::test]
#[ignore]
async fn real_compact_db_weather_marker_2768_chain_transitions() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    let f = fx(Builder { data });
    // One roll per phase func, in actual_func_id order. The first 2768: 7026 (RatioChange 186 @7000
    // and 187 @2500 miss, 188 @500 hits) -> 7055, where 383 (self-respawn @2000) hits. The
    // replacement 2768 does the same, then its 7055 misses 383 / 384 / 385 and hits 387 (-> 3225),
    // which the guard used to reject as a repeated (2768, 7055) source.
    f.server.systems.npcs.doodads.queue_phase_ratios(&[
        8000, 8000, 100, 100, // first 2768: 7026 -> 7055, 383 -> 2768
        8000, 8000, 100, 9000, 9000, 9000, 100, // replacement: 7026 -> 7055, 387 -> 3225
    ]);
    let spawner = new_spawner(2768, 508);
    let first = spawner.spawn(&f.server, 0).expect("spawns template 2768");
    let last = spawner.last().expect("the replacement is tracked");
    assert!(first.data().phase.deleted);
    assert_eq!(spawner.ratio_respawn_peak_depth(), 2);
    assert_eq!(last.data().template_id, 3225);
    assert_eq!(group(&last), 7357); // the cloud's start phase
    assert!(!last.data().phase.deleted);
    assert_eq!(f.world.all_doodads().len(), 1);
    assert_eq!(spawner.ratio_respawn_chain_depth(), 0);
}

/// The 20 real day/night board templates (West Gate 4345-4352, Blue Salt 6175 / 6213 and their
/// variants) swapped back and forth for the whole evening because every replacement took its own
/// `curTime >= Tod` catch-up; each must settle on its night variant at 22:00.
#[tokio::test]
#[ignore]
async fn real_compact_db_day_night_boards_settle_on_the_variant_at_night() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    let f = fx(Builder { data });
    f.server.time.set(22.0);
    // The first respawn func of each board hits.
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(0));
    // (spawner id, board template, expected variant, variant start group)
    let cases = [
        (530, 4345, 4349, 10679),
        (531, 4346, 4350, 10691),
        (532, 6175, 6182, 16358),
        (533, 6213, 6214, 16450),
    ];
    for (id, board, variant, start_group) in cases {
        let spawner = new_spawner(board, id);
        spawner.spawn(&f.server, 0).expect("spawns the board");
        let last = spawner.last().expect("the replacement is tracked");
        assert_eq!(last.data().template_id, variant, "board {board}");
        assert_eq!(group(&last), start_group, "board {board}");
        assert!(!last.data().phase.deleted);
        assert_eq!(spawner.ratio_respawn_peak_depth(), 1, "board {board}");
        assert_eq!(spawner.ratio_respawn_chain_depth(), 0, "board {board}");
    }
    assert_eq!(f.world.all_doodads().len(), cases.len());
}

// ---- growth state of world-spawned doodads (`natural`) ----

mod natural_flow {
    use super::*;
    use crate::systems::npcs::doodad::natural::{self, StateRow};

    const LONG: i32 = 600_000; // 10 min
    const A: u32 = TEMPLATE;
    const B: u32 = TEMPLATE + 1;
    const STATIC: u32 = TEMPLATE + 2;

    /// Template `A` (group 10 -> 11 after 10 min), `B` (group 20 grows 10 min into 21) and the
    /// static `STATIC` (only a short timer).
    fn builder() -> Builder {
        Builder::default()
            .template(A, &[(10, 1), (11, 2)])
            .template(B, &[(20, 1), (21, 2)])
            .template(STATIC, &[(30, 1)])
            .phase_func(10, timer(1, LONG, 11))
            .phase_func(
                20,
                FuncTemplate::Growth(DoodadFuncGrowth {
                    id: 2,
                    delay: LONG,
                    start_scale: 500,
                    end_scale: 1000,
                    next_phase: 21,
                }),
            )
            .phase_func(30, timer(3, 1000, 30))
    }

    fn key_of(template: u32) -> String {
        natural::spawn_key(template, 120.0, 130.0, 100.0)
    }

    fn row(spawn_template: u32, template: u32, group: u32, phase_age_ms: i64) -> StateRow {
        StateRow {
            world_id: 0,
            spawn_key: key_of(spawn_template),
            template_id: template,
            func_group_id: group,
            plant_time: 1_700_000_000,
            growth_time_ms: 0,
            phase_time_ms: now_ms() - phase_age_ms,
            data: 7,
            interaction_counts: "{}".into(),
            updated: 0,
        }
    }

    fn store(f: &Fx) -> &natural::NaturalStates {
        &f.server.systems.npcs.doodads.persistence.natural
    }

    fn saves(f: &Fx) -> Vec<((u32, String), StateRow)> {
        store(f)
            .pending_for_test()
            .into_iter()
            .filter_map(|(k, r)| r.map(|r| (k, r)))
            .collect()
    }

    #[tokio::test]
    async fn saved_phase_resumes_the_remaining_delay() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![row(A, A, 10, 200_000)]);
        let d = new_spawner(A, 501).spawn(&f.server, 0).expect("spawns");
        assert_eq!(group(&d), 10);
        assert_eq!(d.data().plant_time, 1_700_000_000);
        assert_eq!(d.data().data, 7);
        assert_eq!(d.data().natural_key, Some((0, key_of(A))));
        // 10 min delay, 200 s into it.
        let delays = f.server.systems.npcs.doodads.tasks.pending_delays();
        assert_eq!(delays.len(), 1);
        assert!(
            (399_000..=400_500).contains(&delays[0]),
            "remaining {delays:?}"
        );
        f.advance(delays[0]);
        assert_eq!(group(&d), 11);
        // The phase change is queued for saving under the spawn key.
        let saved = saves(&f);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].0, (0, key_of(A)));
        assert_eq!(saved[0].1.func_group_id, 11);
        assert_eq!(saved[0].1.template_id, A);
    }

    #[tokio::test]
    async fn expired_phase_fires_promptly() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![row(A, A, 10, 700_000)]);
        let d = new_spawner(A, 501).spawn(&f.server, 0).expect("spawns");
        assert_eq!(
            f.server.systems.npcs.doodads.tasks.pending_delays(),
            vec![1]
        );
        f.advance(1);
        assert_eq!(group(&d), 11);
    }

    #[tokio::test]
    async fn replacement_template_is_restored() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![row(A, B, 20, 100_000)]);
        let d = new_spawner(A, 501).spawn(&f.server, 0).expect("spawns");
        assert_eq!(d.data().template_id, B);
        assert_eq!(group(&d), 20);
        // Growth, 100 s elapsed.
        let delays = f.server.systems.npcs.doodads.tasks.pending_delays();
        assert!(
            (499_000..=500_500).contains(&delays[0]),
            "remaining {delays:?}"
        );
        // The key stays the spawn entry's, not the replacement's.
        assert_eq!(d.data().natural_key, Some((0, key_of(A))));
    }

    #[tokio::test]
    async fn invalid_rows_are_ignored_and_dropped() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![
            row(A, 999, 10, 0),         // unknown template
            row(B, B, 10, 0),           // group of another template
            row(STATIC, STATIC, 30, 0), // untimed template
        ]);
        for (template, id) in [(A, 501), (B, 502), (STATIC, 503)] {
            let d = new_spawner(template, id)
                .spawn(&f.server, 0)
                .expect("spawns");
            // Fresh Start phase, nothing restored.
            assert!(d.data().plant_time > 1_700_000_000);
            assert_eq!(d.data().data, 0);
        }
        // The untimed entry's stale row is dropped; the timed ones are overwritten by the fresh
        // doodads' own state.
        let mut pending = store(&f).pending_for_test();
        pending.sort_by_key(|(k, _)| k.1.clone());
        assert_eq!(pending.len(), 3);
        for (key, r) in pending {
            match r {
                None => assert_eq!(key.1, key_of(STATIC)),
                Some(r) => assert!(r.func_group_id == 10 || r.func_group_id == 20, "{r:?}"),
            }
        }
        assert_eq!(store(&f).finish_initial_spawn(&f.server, 0, "test"), (0, 0));
    }

    #[tokio::test]
    async fn static_templates_are_not_tracked() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![]);
        let d = new_spawner(STATIC, 501).spawn(&f.server, 0).unwrap();
        assert_eq!(d.data().natural_key, None);
        f.advance(1000);
        assert!(store(&f).pending_for_test().is_empty());
    }

    #[tokio::test]
    async fn removing_the_doodad_drops_its_row_and_a_respawn_starts_fresh() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![row(A, A, 11, 0)]);
        let spawner = new_spawner(A, 501);
        let d = spawner.spawn(&f.server, 0).expect("spawns");
        assert_eq!(group(&d), 11);
        spawner.despawn(&f.server, &d);
        assert_eq!(store(&f).pending_for_test(), vec![((0, key_of(A)), None)]);
        // The next spawn of the entry does not reuse any state.
        let fresh = spawner.spawn(&f.server, 0).expect("respawns");
        assert_eq!(group(&fresh), 10);
        assert_eq!(fresh.data().data, 0);
        assert_eq!(fresh.data().natural_key, Some((0, key_of(A))));
    }

    #[tokio::test]
    async fn a_second_entry_with_the_same_key_is_not_tracked() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![]);
        let first = new_spawner(A, 501).spawn(&f.server, 0).unwrap();
        let second = new_spawner(A, 502).spawn(&f.server, 0).unwrap();
        assert!(first.data().natural_key.is_some());
        assert_eq!(second.data().natural_key, None);
    }

    #[tokio::test]
    async fn another_world_neither_tracks_nor_discards_the_rows() {
        let f = fx(builder());
        store(&f).enable_for_test(vec![row(A, A, 11, 0)]);
        assert!(store(&f).enabled_for(0) && !store(&f).enabled_for(1));
        // The initial spawn of a second world finishes first: it must keep the main world's rows.
        assert_eq!(
            store(&f).finish_initial_spawn(&f.server, 1, "other"),
            (0, 0)
        );
        assert!(store(&f).pending_for_test().is_empty());
        let d = new_spawner(A, 501).spawn(&f.server, 0).expect("spawns");
        assert_eq!(group(&d), 11);
        assert_eq!(store(&f).finish_initial_spawn(&f.server, 0, "main"), (1, 0));
    }

    #[tokio::test]
    async fn tracking_is_off_until_the_rows_are_loaded() {
        let f = fx(builder());
        let d = new_spawner(A, 501).spawn(&f.server, 0).unwrap();
        assert_eq!(d.data().natural_key, None);
        assert!(store(&f).pending_for_test().is_empty());
    }
}
