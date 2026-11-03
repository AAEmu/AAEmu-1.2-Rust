//! WP-S3: `DoodadFuncSiegePeriod` / `DoodadFuncDeclareSiege` behaviour.
//!
//! The fixtures mirror the authored 1.2 rows (template ids, phase-only groups, func periods and
//! destinations); the real compact database is checked by the `#[ignore]` test at the bottom.

use std::sync::Arc;

use chrono::{NaiveDateTime, TimeZone, Utc};

use super::*;
use crate::data::world_data::Zone;
use crate::data::GameData;
use crate::server::GameServer;
use crate::systems::housing::data::{SiegePlan, SiegeZone};
use crate::systems::housing::dominion::period;
use crate::systems::housing::dominion::state::TowerIdentity;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::templates::*;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, PlayerRef, WorldInstance, WorldObject};

/// `o_nuimari`, a shipped siege zone group (plan weeks 2014-01-05 / 01-26).
const ZONE: u32 = 34;
const OWNER: u32 = 77;
const DECLARE_SKILL: u32 = 15067;

const CAMP: u32 = 4307;
const BACKPACK: u32 = 3304;
const DEFENSE_HQ: u32 = 3410;

/// Synthetic `DoodadData` builder (the rows of one authored chain).
#[derive(Default)]
struct Fixture {
    data: DoodadData,
}

impl Fixture {
    fn template(mut self, id: u32, groups: &[(u32, u32)]) -> Self {
        let mut template = DoodadTemplate {
            id,
            ..DoodadTemplate::default()
        };
        for &(group_id, kind) in groups {
            let group = DoodadFuncGroups {
                id: group_id,
                almighty: id,
                group_kind_id: kind,
                ..DoodadFuncGroups::default()
            };
            template.func_groups.push(group.clone());
            self.data
                .all_func_groups_index
                .insert(group_id, self.data.all_func_groups.len());
            self.data.all_func_groups.push(group);
        }
        self.data.template_order.push(id);
        self.data.templates.insert(id, template);
        self
    }

    fn phase_func(mut self, group: u32, template: FuncTemplate) -> Self {
        let (class, id) = (template.func_type(), template.id());
        self.data
            .phase_func_templates
            .entry(class)
            .or_default()
            .insert(id, template);
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

    fn siege_period(
        self,
        group: u32,
        id: u32,
        siege_period_id: u32,
        next_phase: i32,
        defense: bool,
    ) -> Self {
        self.phase_func(
            group,
            FuncTemplate::SiegePeriod(DoodadFuncSiegePeriod {
                id,
                siege_period_id,
                next_phase,
                defense,
            }),
        )
    }

    fn play_flow_graph(self, group: u32, id: u32) -> Self {
        self.phase_func(
            group,
            FuncTemplate::PlayFlowGraph(DoodadFuncPlayFlowGraph {
                id,
                ..Default::default()
            }),
        )
    }

    /// A normal `doodad_funcs` row plus its template (the `DeclareSiege` row of 3304).
    fn func(mut self, group: u32, template: FuncTemplate, next_phase: i32, skill_id: u32) -> Self {
        let (class, id) = (template.func_type(), template.id());
        self.data
            .func_templates
            .entry(class)
            .or_default()
            .insert(id, template);
        let key = self.data.funcs_by_id.len() as u32 + 1;
        let func = DoodadFunc {
            group_id: group,
            func_id: id,
            func_key: key,
            func_type: class.into(),
            next_phase,
            skill_id,
            ..DoodadFunc::default()
        };
        self.data.funcs_by_id.insert(key, func.clone());
        self.data
            .funcs_by_groups
            .entry(group)
            .or_default()
            .push(func);
        self
    }
}

fn camp() -> Fixture {
    Fixture::default()
        .template(
            CAMP,
            &[
                (10581, doodad_func_group_kind::START),
                (10582, doodad_func_group_kind::NORMAL),
                (10779, doodad_func_group_kind::NORMAL),
            ],
        )
        .siege_period(10581, 6, 2, 10582, false)
        .siege_period(10582, 7, 3, 10779, false)
        .play_flow_graph(10582, 11)
        .siege_period(10779, 8, 4, -1, false)
        .play_flow_graph(10779, 12)
}

fn backpack() -> Fixture {
    Fixture::default()
        .template(
            BACKPACK,
            &[
                (7622, doodad_func_group_kind::START),
                (7627, doodad_func_group_kind::NORMAL),
                (7631, doodad_func_group_kind::NORMAL),
            ],
        )
        .func(
            7622,
            FuncTemplate::DeclareSiege(DoodadFuncDeclareSiege { id: 1 }),
            7627,
            DECLARE_SKILL,
        )
        .siege_period(7627, 1, 2, 7631, false)
        .siege_period(7631, 2, 3, -1, false)
        .play_flow_graph(7631, 8)
}

fn defense_hq() -> Fixture {
    Fixture::default()
        .template(
            DEFENSE_HQ,
            &[
                (8173, doodad_func_group_kind::START),
                (8238, doodad_func_group_kind::NORMAL),
            ],
        )
        .siege_period(8173, 4, 0, 8238, true)
        .play_flow_graph(8173, 6)
        .siege_period(8238, 5, 3, 8173, true)
        .play_flow_graph(8238, 7)
}

/// The shipped zone 34 schedule and two 2014 plan weeks (UTC, calendar timezone `+00:00`).
fn calendar_data() -> GameData {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            zone_key: TEST_ZONE,
            group_id: ZONE,
            ..Default::default()
        },
    );
    data.systems.housing.castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            id: 2,
            zone_group_id: ZONE,
            start_auction_weekday: 3,
            start_auction_hour: 22,
            start_auction_min: 30,
            start_declare_weekday: 5,
            start_declare_hour: 22,
            start_declare_min: 30,
            start_warmup_weekday: 0,
            start_warmup_hour: 20,
            start_warmup_min: 30,
            start_siege_weekday: 0,
            start_siege_hour: 21,
            start_siege_min: 0,
            siege_days: 0,
            siege_hours: 1,
            siege_mins: 30,
            pay_weekday: 0,
            pay_hour: 23,
            pay_min: 55,
            ..Default::default()
        },
    );
    for (id, week) in [(1, "2014-01-05"), (2, "2014-01-26")] {
        data.systems
            .housing
            .castles
            .siege_plans
            .entry(ZONE)
            .or_default()
            .push(SiegePlan {
                id,
                zone_group_id: ZONE,
                week_start: NaiveDateTime::parse_from_str(
                    &format!("{week} 00:00:00"),
                    "%Y-%m-%d %H:%M:%S",
                )
                .unwrap(),
            });
    }
    data
}

struct World {
    server: GameServer,
    world: Arc<WorldInstance>,
    player: PlayerRef,
    _client: TestClient,
}

fn world_with(data: GameData) -> World {
    let server = test_server_with(data);
    server.systems.npcs.doodads.tasks.set_manual(true);
    server.config.update(|config| {
        config.housing.siege_calendar = true;
        config.housing.siege_calendar_timezone = "+00:00".into();
    });
    let client = TestClient::new(&server, 1, 1001);
    let player_obj_id = server.ids.object.get_next_id().unwrap();
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "SiegePeriod", 100.0, 100.0),
        player_obj_id,
    );
    let world = server.world.world(0).unwrap();
    World {
        server,
        world,
        player,
        _client: client,
    }
}

fn world(fixture: Fixture) -> World {
    let mut data = calendar_data();
    data.systems.npcs.doodad = fixture.data;
    world_with(data)
}

impl World {
    /// `current_period` only reports the calendar for an owned territory.
    fn own(&self) {
        let now = chrono::Utc::now().timestamp();
        self.server
            .systems
            .housing
            .dominions
            .declare_unowned_with(
                ZONE,
                TowerIdentity {
                    house_id: 1,
                    instance_id: 0,
                    guard_tower_setting_id: 1,
                },
                OWNER,
                now,
                || true,
            )
            .unwrap();
    }

    fn doodad(&self, template: u32) -> DoodadRef {
        let doodad = manager::create(&self.server, &self.world, 0, template, None, true).unwrap();
        doodad.data().transform.set_xyz(100.0, 103.0, 100.0);
        doodad.data().transform.zone_id = TEST_ZONE;
        manager::spawn(&self.server, &self.world, &doodad);
        manager::init_doodad(&self.server, &self.world, &doodad);
        doodad
    }

    fn group(&self, doodad: &DoodadRef) -> u32 {
        doodad.data().func_group_id
    }

    fn use_(&self, doodad: &DoodadRef, skill_id: u32) {
        manager::use_doodad(
            &self.server,
            &self.world,
            doodad,
            Some(&WorldObject::from(&self.player)),
            skill_id,
            0,
        );
    }

    fn clock(&self, now: i64) {
        self.server.systems.housing.calendar.set_clock(Some(now));
    }
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap()
        .timestamp()
}

#[tokio::test]
async fn siege_period_stays_in_its_phase_until_the_period_ends() {
    let f = world(camp());
    f.own();
    let doodad = f.doodad(CAMP);
    assert_eq!(f.group(&doodad), 10581);

    // First observation is silent: the declare period is current, nothing changes.
    f.clock(utc(2014, 1, 10, 23, 0));
    assert_eq!(tick(&f.server), 0);
    assert_eq!(f.group(&doodad), 10581);

    // Still the same period: no change.
    f.clock(utc(2014, 1, 10, 23, 30));
    assert_eq!(tick(&f.server), 0);
    assert_eq!(f.group(&doodad), 10581);

    // Period 2 ends (no declaration: peace follows): the authored next phase is applied once.
    f.clock(utc(2014, 1, 12, 20, 31));
    assert_eq!(tick(&f.server), 1);
    assert_eq!(f.group(&doodad), 10582);

    // The other side of the same observation does not fire anything again.
    assert_eq!(tick(&f.server), 0);
    assert_eq!(f.group(&doodad), 10582);
}

#[tokio::test]
async fn authored_4307_chain_walks_period_2_to_5_and_disappears() {
    let f = world(camp());
    let doodad = f.doodad(CAMP);
    assert_eq!(f.group(&doodad), 10581);

    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&doodad), 10582);
    assert_eq!(apply_period_change(&f.server, ZONE, 3, 4), 1);
    assert_eq!(f.group(&doodad), 10779);
    assert_eq!(apply_period_change(&f.server, ZONE, 4, 5), 1);
    assert!(doodad.data().phase.deleted);
    assert!(f.world.object(doodad.obj_id).is_none());

    // The remaining boundaries of the cycle are no-ops.
    for (previous, current) in [(5, 1), (1, 2)] {
        assert_eq!(apply_period_change(&f.server, ZONE, previous, current), 0);
    }
}

#[tokio::test]
async fn authored_3304_chain_advances_on_its_bound_skill_then_walks_the_periods() {
    let f = world(backpack());
    let doodad = f.doodad(BACKPACK);
    assert_eq!(f.group(&doodad), 7622);

    // A different skill does not declare (the row is bound to 15067).
    f.use_(&doodad, 15062);
    assert_eq!(f.group(&doodad), 7622);

    // The authored row's action: advance to the declare phase.
    f.use_(&doodad, DECLARE_SKILL);
    assert_eq!(f.group(&doodad), 7627);

    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&doodad), 7631);
    assert_eq!(apply_period_change(&f.server, ZONE, 3, 4), 1);
    assert!(doodad.data().phase.deleted);
}

#[tokio::test]
async fn defender_hq_safe_zone_toggles_once_per_boundary_and_never_loops() {
    let f = world(defense_hq());
    let doodad = f.doodad(DEFENSE_HQ);
    assert_eq!(f.group(&doodad), 8173);

    // Period 0 (UNKNOWN) safe default: up when the warmup begins.
    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&doodad), 8238);
    // Reinitializing the retained object runs the phase funcs again; they must stay passive.
    manager::init_doodad(&f.server, &f.world, &doodad);
    assert_eq!(f.group(&doodad), 8238);
    assert!(!doodad.data().phase.deleted);

    // Warmup ends: down for the siege (func 5).
    assert_eq!(apply_period_change(&f.server, ZONE, 3, 4), 1);
    assert_eq!(f.group(&doodad), 8173);
    manager::init_doodad(&f.server, &f.world, &doodad);
    assert_eq!(f.group(&doodad), 8173);
    assert!(!doodad.data().phase.deleted);

    // Peace and the next auction/declare never bounce the object.
    for (previous, current) in [(4, 5), (5, 1), (1, 2)] {
        assert_eq!(apply_period_change(&f.server, ZONE, previous, current), 0);
        assert_eq!(f.group(&doodad), 8173);
        assert!(!doodad.data().phase.deleted);
    }

    // The next warmup brings the safe zone back once.
    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&doodad), 8238);
}

#[tokio::test]
async fn current_period_reports_owned_peace_and_explicit_warmup() {
    let f = world(camp());
    // No dominion row at all: period 0.
    assert_eq!(period::current_period(&f.server, ZONE), Some(0));

    f.own();
    // The GM calendar clock says declare; the calendar is enabled.
    f.clock(utc(2014, 1, 10, 23, 0));
    assert_eq!(period::current_period(&f.server, ZONE), Some(2));

    // An unfinished explicit schedule overrides the calendar with warmup (real clock).
    let real = chrono::Utc::now().timestamp();
    f.server
        .systems
        .housing
        .dominions
        .schedule_siege(ZONE, 88, real + 3600, real + 7200, real)
        .unwrap();
    assert_eq!(period::current_period(&f.server, ZONE), Some(3));

    // A territory with the calendar disabled and no schedule is peace.
    let other = world(camp());
    other.own();
    other.server.config.update(|config| {
        config.housing.siege_calendar = false;
    });
    assert_eq!(period::current_period(&other.server, ZONE), Some(5));
}

#[tokio::test]
#[ignore = "requires the original read-only 1.2 compact database (AAEMU_COMPACT_DB)"]
async fn real_authored_siege_period_rows_walk_the_chains() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    for (id, period, next, defense) in [
        (6, 2, 10582, false),
        (7, 3, 10779, false),
        (8, 4, -1, false),
        (1, 2, 7631, false),
        (2, 3, -1, false),
        (4, 0, 8238, true),
        (5, 3, 8173, true),
    ] {
        assert!(
            matches!(
                data.phase_func_template(id, "DoodadFuncSiegePeriod"),
                Some(FuncTemplate::SiegePeriod(function))
                    if function.siege_period_id == period
                        && function.next_phase == next
                        && function.defense == defense
            ),
            "authored SiegePeriod row {id}"
        );
    }
    // The 3304 declaration row is reachable through the regular func lookup as well.
    assert!(data.func_template(1, "DoodadFuncDeclareSiege").is_some());
    assert!(data
        .phase_func_template(1, "DoodadFuncDeclareSiege")
        .is_some());

    let mut game_data = calendar_data();
    game_data.systems.npcs.doodad = data;
    let f = world_with(game_data);

    let camp = f.doodad(CAMP);
    assert_eq!(f.group(&camp), 10581);
    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&camp), 10582);
    assert_eq!(apply_period_change(&f.server, ZONE, 3, 4), 1);
    assert_eq!(f.group(&camp), 10779);
    assert_eq!(apply_period_change(&f.server, ZONE, 4, 5), 1);
    assert!(camp.data().phase.deleted);

    let backpack = f.doodad(BACKPACK);
    f.use_(&backpack, DECLARE_SKILL);
    assert_eq!(f.group(&backpack), 7627);
    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&backpack), 7631);
    assert_eq!(apply_period_change(&f.server, ZONE, 3, 4), 1);
    assert!(backpack.data().phase.deleted);

    let hq = f.doodad(DEFENSE_HQ);
    assert_eq!(f.group(&hq), 8173);
    assert_eq!(apply_period_change(&f.server, ZONE, 2, 3), 1);
    assert_eq!(f.group(&hq), 8238);
    manager::init_doodad(&f.server, &f.world, &hq);
    assert_eq!(f.group(&hq), 8238);
    assert_eq!(apply_period_change(&f.server, ZONE, 3, 4), 1);
    assert_eq!(f.group(&hq), 8173);
}
