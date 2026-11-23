//! Phase-machine tests: synthetic func groups (Timer / Final / Tod / RatioChange / Growth /
//! PulseTrigger, the `ListGroupId` recursion guard, `NextPhase == -1`, `OverridePhase`) and the
//! real template 2336 (gated by `AAEMU_COMPACT_DB`).

use std::sync::Arc;

use super::*;
use crate::data::world_templates::{WorldSpawnPosition, WorldTemplate, XmlWorldZone};
use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::npcs::area_trigger::{AreaShape, AREA_SHAPE_CUBOID};
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::logic;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::spawner::{
    parse_list, DoodadSpawner, DoodadSpawnerDef, SpawnerBase,
};
use crate::systems::npcs::doodad::templates::*;
use crate::systems::npcs::fish_school::{FRESHWATER_FISH_SCHOOL, SALTWATER_FISH_SCHOOL};
use crate::test_support::*;
use crate::world::{PlayerRef, WorldInstance};

const TEMPLATE: u32 = 100;
const FAKE_SKILL: u32 = 999;
const HAZARD_OWNER_FACTION: u32 = 115;

/// Synthetic `DoodadData` builder.
#[derive(Default)]
struct Builder {
    data: DoodadData,
    skills: Vec<(u32, i32)>,
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

    /// A func row of `func_type` (no template needed by the phase packet handler).
    fn func(mut self, group: u32, func_type: &str, next_phase: i32, skill_id: u32) -> Self {
        let key = self.data.funcs_by_id.len() as u32 + 1;
        let f = DoodadFunc {
            group_id: group,
            func_id: key,
            func_key: key,
            func_type: func_type.into(),
            next_phase,
            skill_id,
            ..DoodadFunc::default()
        };
        self.data.funcs_by_id.insert(key, f.clone());
        self.data.funcs_by_groups.entry(group).or_default().push(f);
        self
    }

    /// A game-data skill with `max_range` (the phase packet's reach check).
    fn skill(mut self, id: u32, max_range: i32) -> Self {
        self.skills.push((id, max_range));
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

    /// Sets `phase_msg` / `is_msg_to_zone` on func group `group` (template copy and the
    /// `_allFuncGroups` copy the packet send looks up).
    fn phase_msg(mut self, group_id: u32, message: &str, to_zone: bool) -> Self {
        for g in self
            .data
            .all_func_groups
            .iter_mut()
            .filter(|g| g.id == group_id)
        {
            g.phase_msg = message.to_string();
            g.is_msg_to_zone = to_zone;
        }
        for t in self.data.templates.values_mut() {
            for g in t.func_groups.iter_mut().filter(|g| g.id == group_id) {
                g.phase_msg = message.to_string();
                g.is_msg_to_zone = to_zone;
            }
        }
        self
    }

    fn server(self) -> GameServer {
        let mut gd = GameData::empty();
        gd.systems.npcs.doodad = self.data;
        for (id, max_range) in self.skills {
            gd.systems
                .skills
                .add_skill(crate::systems::skills::data::SkillTemplate {
                    id,
                    max_range,
                    ..Default::default()
                });
        }
        let s = test_server_with(gd);
        s.systems.npcs.doodads.tasks.set_manual(true);
        s
    }

    /// [`Self::server`] with a caller-built world template (e.g. more than one zone key).
    fn server_with_world(self, world: WorldTemplate) -> GameServer {
        let mut gd = GameData::empty();
        gd.systems.npcs.doodad = self.data;
        let s = test_server_with_world(gd, world);
        s.systems.npcs.doodads.tasks.set_manual(true);
        s
    }

    /// [`Self::server`] with the skills bridge, a good `buff` and the arena `shape` the clout
    /// applies it with.
    fn clout_server(self, buff: u32, shape: AreaShape) -> Arc<GameServer> {
        let mut gd = GameData::empty();
        gd.systems.npcs.doodad = self.data;
        gd.systems.npcs.area_trigger.shapes.insert(shape.id, shape);
        gd.factions.add(crate::data::world_data::SystemFaction {
            id: HAZARD_OWNER_FACTION,
            ..Default::default()
        });
        gd.systems
            .skills
            .add_buff(crate::systems::skills::data::BuffTemplate {
                id: buff,
                kind: 1,
                stack_rule: 1,
                duration: 0,
                ..Default::default()
            });
        gd.skills.buffs.insert(
            buff,
            crate::data::skills::BuffTemplate::simple(buff, crate::data::skills::BuffKind::Good),
        );
        let server = Arc::new(test_server_with(gd));
        server.systems.npcs.doodads.tasks.set_manual(true);
        server.set_self();
        crate::systems::skills::bind(&server);
        server
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
    player.data().unit.max_hp = 1000;
    player.data().unit.hp = 1000;
    client.conn.active_char = Some(player.clone());
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

    /// `(doodad obj id, func group id)` of the drained `SCDoodadPhaseMsgPacket`s.
    fn phase_msgs(&mut self) -> Vec<(u32, u32)> {
        self.client
            .drain()
            .into_iter()
            .filter(|f| f.0 == sc::SCDoodadPhaseMsgPacket)
            .map(|f| {
                let doodad = u32::from_le_bytes([f.1[0], f.1[1], f.1[2], 0]);
                let group = u32::from_le_bytes(f.1[3..7].try_into().unwrap());
                (doodad, group)
            })
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
    new_spawner_with(unit_id, id, None, 0)
}

/// [`new_spawner`] with the spawn row's `RelatedIds` and `FamilyId`.
fn new_spawner_with(
    unit_id: u32,
    id: u32,
    related_ids: Option<Vec<u32>>,
    family_id: u32,
) -> Arc<DoodadSpawner> {
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
            related_ids,
            family_id,
            respawn_doodad_template_id: 0,
            initial_func_group_id: 0,
        },
        Some(0),
    ))
}

fn group(d: &DoodadRef) -> u32 {
    d.data().func_group_id
}

/// `manager::create` (an obj id from the id manager) placed at `x`, spawned, then `InitDoodad`.
fn place(f: &Fx, template: u32, x: f32) -> DoodadRef {
    let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
    d.data().transform.set_xyz(x, 100.0, 100.0);
    manager::spawn(&f.server, &f.world, &d);
    manager::init_doodad(&f.server, &f.world, &d);
    d
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

/// A source has one level-design family (its `Doodad::family_id`); its provider rows name the
/// *receiver* family, and the receiver's subscriber rows name the *source* family. Both the
/// active source and a late-spawned receiver must connect, and a source outside the subscriber
/// set must be ignored.
#[tokio::test]
async fn family_pulses_route_by_the_level_design_family_and_owner_scope() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .template(102, &[(10, 1)])
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 1,
                family_id: 8,
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
    let make = |template, family| {
        let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
        d.data().transform.set_xyz(101.0, 100.0, 100.0);
        d.data().family_id = family;
        manager::spawn(&f.server, &f.world, &d);
        manager::init_doodad(&f.server, &f.world, &d);
        d
    };
    // Template 100 is the subscribed source (family 7); 102 provides to the same receiver but
    // is not in the receiver's subscriber set, so it must not trigger anything.
    let receiver = make(101, 8);
    let other_owner = make(101, 8);
    other_owner.data().owner_id = 99;
    let source = make(100, 7);
    let foreign = make(102, 9);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), source.clone());
    let foreign_ctx = DoodadCtx::new(&f.server, f.world.clone(), foreign.clone());
    assert_eq!(
        group(&receiver),
        20,
        "registration alone must not activate a trigger"
    );
    logic::emit(&foreign_ctx, None, true);
    assert_eq!(group(&receiver), 20, "a foreign family must be refused");
    logic::emit(&ctx, None, true);
    assert_eq!(group(&receiver), 21);
    assert_eq!(group(&other_owner), 20);
    let late_receiver = make(101, 8);
    assert_eq!(
        group(&late_receiver),
        21,
        "a late receiver adopts an already active source"
    );
    logic::emit(&ctx, None, false);
    assert_eq!(group(&receiver), 20);
    assert_eq!(group(&late_receiver), 20);
}

/// A spawner's `RelatedIds` is an authored wire from that placement to the named template, like
/// a client wire: it must not go through the subscriber-family source filter. Shipped shape
/// (Howling Abyss 2 6345 -> 6346 and 18 more pairs): the source's group pulses true, the target's
/// start group holds a `DoodadFuncLogicFamilySubscriber` and a true `DoodadFuncPulseTrigger`, and
/// neither placement row carries a `FamilyId`.
#[tokio::test]
async fn spawner_related_ids_reach_a_subscribed_target_without_family_ids() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Pulse(DoodadFuncPulse { id: 1, flag: true }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                id: 2,
                family_id: 500,
            }),
        )
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 3,
                flag: true,
                next_phase: 21,
            }),
        );
    let mut f = fx(b);
    let target = f.doodad(101);
    let source = new_spawner_with(100, 600, Some(vec![101]), 0)
        .spawn(&f.server, 0)
        .expect("the source spawns");
    assert_eq!((source.data().family_id, target.data().family_id), (0, 0));
    assert_eq!(
        group(&target),
        20,
        "spawning the source pulses nothing (no caster)"
    );
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), source.clone());
    logic::emit(&ctx, Some(&f.caster()), true);
    assert_eq!(
        group(&target),
        21,
        "the RelatedIds signal reaches the target's trigger"
    );
}

/// The Sharpwind Mines water chain over a synthetic copy of the shipped rows: switches 5064
/// (family 50002) and 5068 (50003) feed the AND gate 5070 (50005), which starts the 5072
/// streams and arms gate 5071 (50006); switch 5069 (50004) then lets gate 5071 start the 5073
/// streams and the lake 5063 (50001), whose authored timers raise the water volume 22 m after
/// 5 s and swap to the raised model after further 22 s.
#[tokio::test]
async fn sharpwind_switches_raise_the_lake_through_the_family_network() {
    fn switch(
        group: u32,
        pulse: u32,
        provider: u32,
        provider_family: u32,
        trigger: u32,
        final_group: u32,
    ) -> Vec<(u32, FuncTemplate)> {
        vec![
            (
                group,
                FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                    id: trigger,
                    flag: true,
                    next_phase: final_group as i32,
                }),
            ),
            (
                group,
                FuncTemplate::Pulse(DoodadFuncPulse {
                    id: pulse,
                    flag: true,
                }),
            ),
            (
                group,
                FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                    id: provider,
                    family_id: provider_family,
                }),
            ),
        ]
    }
    fn gate(
        group: u32,
        logic: u32,
        out_a: (u32, u32),
        out_b: (u32, u32),
        in_a: (u32, u32),
        in_b: (u32, u32),
    ) -> Vec<(u32, FuncTemplate)> {
        vec![
            (
                group,
                FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                    id: in_a.0,
                    family_id: in_a.1,
                }),
            ),
            (
                group,
                FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                    id: in_b.0,
                    family_id: in_b.1,
                }),
            ),
            (
                group,
                FuncTemplate::Logic(DoodadFuncLogic {
                    id: logic,
                    operation_id: 1,
                    delay_id: 1,
                }),
            ),
            (
                group,
                FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                    id: out_a.0,
                    family_id: out_a.1,
                }),
            ),
            (
                group,
                FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                    id: out_b.0,
                    family_id: out_b.1,
                }),
            ),
        ]
    }
    let mut b = Builder::default()
        .template(5064, &[(13160, 1), (13161, 2), (13163, 2)])
        .template(5068, &[(13175, 1), (13176, 2), (13177, 2)])
        .template(5069, &[(13178, 1), (13179, 2), (13180, 2)])
        .template(5070, &[(13181, 1)])
        .template(5071, &[(13182, 1)])
        .template(5072, &[(13183, 1), (13184, 2), (13188, 2)])
        .template(5073, &[(13185, 1), (13186, 2), (13187, 2)])
        .template(5063, &[(13157, 1), (14184, 2), (13158, 2), (13159, 2)]);
    // Start groups: no use func in the fixture (the test calls `use_doodad` through a synthetic
    // row below only for the switches we use).
    for (group, family) in [(13160u32, 50_005u32), (13175, 50_005), (13178, 50_006)] {
        b = b
            .phase_func(
                group,
                FuncTemplate::Pulse(DoodadFuncPulse { id: 9, flag: false }),
            )
            .phase_func(
                group,
                FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                    id: 10,
                    family_id: family,
                }),
            );
    }
    b = b
        .fake_use(13160, 1001, 13161)
        .fake_use(13175, 1002, 13176)
        .fake_use(13178, 1003, 13179);
    for (group, t) in switch(13161, 287, 444, 50_005, 274, 13163) {
        b = b.phase_func(group, t);
    }
    for (group, t) in switch(13176, 290, 447, 50_005, 275, 13177) {
        b = b.phase_func(group, t);
    }
    for (group, t) in switch(13179, 292, 449, 50_006, 276, 13180) {
        b = b.phase_func(group, t);
    }
    for (group, t) in gate(
        13181,
        88,
        (450, 50_006),
        (451, 50_007),
        (45, 50_002),
        (46, 50_003),
    ) {
        b = b.phase_func(group, t);
    }
    for (group, t) in gate(
        13182,
        89,
        (452, 50_001),
        (453, 50_008),
        (47, 50_004),
        (48, 50_005),
    ) {
        b = b.phase_func(group, t);
    }
    b = b
        .phase_func(
            13183,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 278,
                flag: true,
                next_phase: 13184,
            }),
        )
        .phase_func(
            13184,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 280,
                flag: false,
                next_phase: 13188,
            }),
        )
        .phase_func(
            13185,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 277,
                flag: true,
                next_phase: 13186,
            }),
        )
        .phase_func(
            13186,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 279,
                flag: false,
                next_phase: 13187,
            }),
        )
        .phase_func(
            13157,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 273,
                flag: true,
                next_phase: 14184,
            }),
        )
        .phase_func(
            14184,
            FuncTemplate::Timer(DoodadFuncTimer {
                id: 5938,
                delay: 5000,
                next_phase: 13158,
                ..Default::default()
            }),
        )
        .phase_func(
            13158,
            FuncTemplate::WaterVolume(DoodadFuncWaterVolume {
                id: 15,
                level_change: 22.0,
                duration: 22.0,
            }),
        )
        .phase_func(
            13158,
            FuncTemplate::Timer(DoodadFuncTimer {
                id: 5491,
                delay: 22_000,
                next_phase: 13159,
                ..Default::default()
            }),
        );
    let f = fx(b);
    let make = |template: u32, family: u32, x: f32| {
        let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
        d.data().transform.set_xyz(x, 100.0, 100.0);
        d.data().family_id = family;
        manager::spawn(&f.server, &f.world, &d);
        manager::init_doodad(&f.server, &f.world, &d);
        d
    };
    let sw_a = make(5064, 50_002, 100.0);
    let sw_b = make(5068, 50_003, 101.0);
    let sw_c = make(5069, 50_004, 102.0);
    let gate_a = make(5070, 50_005, 103.0);
    let gate_b = make(5071, 50_006, 104.0);
    let stream_a = make(5072, 50_007, 105.0);
    let stream_b = make(5073, 50_008, 106.0);
    let lake = make(5063, 50_001, 107.0);

    // One switch is not enough for the AND gate.
    f.use_(&sw_a);
    assert_eq!(
        group(&sw_a),
        13163,
        "the used switch runs its own final phase"
    );
    f.advance(1_000);
    assert_eq!(group(&gate_a), 13181);
    assert_eq!(group(&gate_b), 13182);
    assert_eq!(group(&stream_a), 13183, "no stream before gate 5070 fires");
    assert_eq!(group(&lake), 13157);

    // The second switch completes gate 5070; its 1 s output starts the 5072 streams and arms
    // gate 5071, which still waits for switch 5069.
    f.use_(&sw_b);
    f.advance(1_000);
    assert_eq!(group(&stream_a), 13184, "gate 5070 fires the 5072 streams");
    assert_eq!(group(&stream_b), 13185);
    assert_eq!(group(&gate_b), 13182);
    assert_eq!(group(&lake), 13157);

    // Switch 5069 completes gate 5071 after its 1 s delay: the 5073 streams erupt and the lake
    // starts its authored water-volume sequence.
    f.use_(&sw_c);
    f.advance(1_000);
    assert_eq!(group(&stream_b), 13186, "gate 5071 fires the 5073 streams");
    assert_eq!(group(&lake), 14184);
    f.advance(5_000);
    assert_eq!(group(&lake), 13158, "the 5 s pre-roll ends");
    f.advance(21_000);
    assert_eq!(group(&lake), 13158, "the water volume still rises (22 s)");
    f.advance(1_000);
    assert_eq!(group(&lake), 13159, "the raised water2 phase is reached");
}

/// With 1 s delays, outputs at 0 ms (true), 200 ms (false) and 400 ms (true) must emit once, at
/// 1400 ms, with the final value; the replaced outputs cannot fire at their own (earlier) due
/// times.
#[tokio::test]
async fn replaced_logic_output_emits_once_with_the_last_value() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Logic(DoodadFuncLogic {
                id: 1,
                operation_id: 3,
                delay_id: 1,
            }),
        )
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 2,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2), (22, 2)])
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
                flag: true,
                next_phase: 22,
            }),
        )
        .phase_func(
            21,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 5,
                flag: false,
                next_phase: 20,
            }),
        )
        .template(102, &[(30, 1)]);
    let f = fx(b);
    let make = |template: u32, family: u32, x: f32| {
        let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
        d.data().transform.set_xyz(x, 100.0, 100.0);
        d.data().family_id = family;
        manager::spawn(&f.server, &f.world, &d);
        manager::init_doodad(&f.server, &f.world, &d);
        d
    };
    // A phase-entry `PulseTrigger` never fires without a pulse of its own group, so the
    // receiver's groups advance once per received signal.
    let source = make(100, 40_000, 101.0);
    let receiver = make(101, 500, 102.0);
    let driver = make(102, 0, 103.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), driver);
    logic::receive(&ctx, &source, None, true, true);
    assert_eq!(
        source.data().phase.logic_output,
        None,
        "a pending result is not emitted yet"
    );
    assert_eq!(source.data().phase.pending_output, Some(true));
    f.advance(200);
    logic::receive(&ctx, &source, None, false, true);
    f.advance(200);
    logic::receive(&ctx, &source, None, true, true);
    assert_eq!(group(&receiver), 20);

    f.advance(599);
    assert_eq!(group(&receiver), 20, "still inside the first 1 s delay");
    f.advance(1);
    assert_eq!(
        group(&receiver),
        20,
        "the replaced 0 ms output must not emit at 1000 ms"
    );
    assert_eq!(source.data().phase.logic_output, None);
    f.advance(200);
    assert_eq!(
        group(&receiver),
        20,
        "the replaced 200 ms output must not emit at 1200 ms"
    );
    assert_eq!(source.data().phase.logic_output, None);

    f.advance(200);
    assert_eq!(
        group(&receiver),
        21,
        "the last output emits once, at 1400 ms"
    );
    assert_eq!(source.data().phase.logic_output, Some(true));
    assert_eq!(source.data().phase.pending_output, None);
    f.advance(5_000);
    assert_eq!(
        group(&receiver),
        21,
        "a replaced output does not emit a second time"
    );
}

/// A wire linked while a result is still pending receives nothing until the delayed emit; on
/// the emit it receives the value once.
#[tokio::test]
async fn a_wire_linked_during_the_logic_delay_receives_nothing_until_the_emit() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Logic(DoodadFuncLogic {
                id: 1,
                operation_id: 3,
                delay_id: 1,
            }),
        )
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 2,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                id: 3,
                family_id: 500,
            }),
        )
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 4,
                flag: true,
                next_phase: 21,
            }),
        )
        .template(102, &[(30, 1)]);
    let f = fx(b);
    let make = |template: u32, x: f32| {
        let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
        // `link` requires the player to own both doodads.
        d.data().owner_id = f.player.character_id;
        d.data().transform.set_xyz(x, 100.0, 100.0);
        manager::spawn(&f.server, &f.world, &d);
        manager::init_doodad(&f.server, &f.world, &d);
        d
    };
    let source = make(100, 101.0);
    let subscriber = make(101, 102.0);
    let driver = make(102, 103.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), driver);
    logic::receive(&ctx, &source, None, true, true);

    f.advance(200);
    assert!(
        logic::link(&f.server, &f.player, source.obj_id, subscriber.obj_id),
        "the player owns and stands next to both doodads"
    );
    assert_eq!(
        group(&subscriber),
        20,
        "the wire must not receive the pending value"
    );
    assert_eq!(source.data().phase.logic_output, None);
    assert_eq!(source.data().phase.pending_output, Some(true));
    f.advance(799);
    assert_eq!(group(&subscriber), 20);
    f.advance(1);
    assert_eq!(group(&subscriber), 21, "the delayed emit reaches the wire");
}

/// `subscribe` replays only the last emitted value, never a result that is still waiting for
/// its delay.
#[tokio::test]
async fn subscribe_replays_only_the_emitted_logic_value() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Logic(DoodadFuncLogic {
                id: 1,
                operation_id: 3,
                delay_id: 1,
            }),
        )
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 2,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                id: 3,
                family_id: 500,
            }),
        )
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 4,
                flag: true,
                next_phase: 21,
            }),
        )
        .template(102, &[(30, 1)]);
    let f = fx(b);
    let make = |template: u32, family: u32, x: f32| {
        let d = manager::create(&f.server, &f.world, 0, template, None, true).unwrap();
        d.data().transform.set_xyz(x, 100.0, 100.0);
        d.data().family_id = family;
        manager::spawn(&f.server, &f.world, &d);
        manager::init_doodad(&f.server, &f.world, &d);
        d
    };
    let source = make(100, 500, 101.0);
    let driver = make(102, 0, 103.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), driver);
    logic::receive(&ctx, &source, None, true, true);

    f.advance(100);
    let receiver = make(101, 500, 102.0);
    assert_eq!(
        group(&receiver),
        20,
        "the phase-entry subscribe must not replay the pending value"
    );
    assert_eq!(source.data().phase.logic_output, None);
    assert_eq!(source.data().phase.pending_output, Some(true));
    f.advance(900);
    assert_eq!(
        group(&receiver),
        21,
        "the delayed emit reaches the subscriber"
    );
    f.advance(5_000);
    assert_eq!(group(&receiver), 21, "subscribe replays nothing again");
}

/// A gate that already emitted false, whose inputs flip to true and back to false inside the 1 s
/// delay, has nothing left to say: the pending true is cancelled and the false is not emitted a
/// second time. The receiver counts the false signals it gets (each `PulseTrigger(false)` moves
/// it one group on), so a repeated false would show as a second step.
#[tokio::test]
async fn a_pending_logic_output_that_returns_to_the_emitted_value_is_cancelled() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Logic(DoodadFuncLogic {
                id: 1,
                operation_id: 3,
                delay_id: 1,
            }),
        )
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 2,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2), (22, 2)])
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 3,
                flag: false,
                next_phase: 21,
            }),
        )
        .phase_func(
            21,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 4,
                flag: false,
                next_phase: 22,
            }),
        )
        .template(102, &[(30, 1)]);
    let f = fx(b);
    let source = place(&f, 100, 101.0);
    let receiver = place(&f, 101, 102.0);
    receiver.data().family_id = 500;
    let driver = place(&f, 102, 103.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), driver);

    logic::receive(&ctx, &source, None, false, true);
    f.advance(1_000);
    assert_eq!(source.data().phase.logic_output, Some(false));
    assert_eq!(group(&receiver), 21, "the first false is delivered");

    logic::receive(&ctx, &source, None, true, true);
    assert_eq!(source.data().phase.pending_output, Some(true));
    f.advance(200);
    let generation = source.data().phase.pending_output_generation;
    logic::receive(&ctx, &source, None, false, true);
    assert_eq!(
        source.data().phase.pending_output,
        None,
        "the inputs returned to the emitted value: nothing is pending"
    );
    assert_ne!(
        source.data().phase.pending_output_generation,
        generation,
        "the cancelled output's task no longer matches"
    );

    f.advance(2_000);
    assert_eq!(source.data().phase.logic_output, Some(false));
    assert_eq!(
        group(&receiver),
        21,
        "neither the cancelled true nor a repeated false reaches the receiver"
    );
}

/// The spawn row's `FamilyId` reaches the doodad: `DoodadSpawner::new` keeps it, `spawn` copies
/// it to `Doodad::family_id` and the event copy of the spawner keeps it; a row without one
/// spawns a doodad of family 0.
#[tokio::test]
async fn a_spawner_gives_its_family_id_to_the_doodad_it_spawns() {
    let f = fx(Builder::default().template(100, &[(10, 1)]));
    let spawner = new_spawner_with(100, 600, None, 50_002);
    assert_eq!(spawner.family_id, 50_002);
    let doodad = spawner.spawn(&f.server, 0).expect("the spawner spawns");
    assert_eq!(doodad.data().family_id, 50_002);
    assert_eq!(
        spawner.event_copy(0).family_id,
        50_002,
        "the event copy keeps the placement's family"
    );
    let plain = new_spawner(100, 601).spawn(&f.server, 0).expect("spawns");
    assert_eq!(plain.data().family_id, 0);
}

/// `DoodadFuncPulse` runs only with a caster (C# `if (caster is not null)`): a world spawn enters
/// its start group without one, so a spawn-time `Pulse(false)` reaches no receiver (this one's
/// trigger waits for false) and leaves the source's output untouched; the same group entered
/// with a character does pulse.
#[tokio::test]
async fn a_pulse_without_a_caster_emits_nothing() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Pulse(DoodadFuncPulse { id: 1, flag: false }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 2,
                flag: false,
                next_phase: 21,
            }),
        );
    let mut f = fx(b);
    let receiver = f.doodad(101);
    let source = new_spawner_with(100, 600, Some(vec![101]), 0)
        .spawn(&f.server, 0)
        .expect("the source spawns");
    assert_eq!(
        group(&receiver),
        20,
        "a spawn-time Pulse(false) pulses nothing"
    );
    assert_eq!(source.data().phase.logic_output, None);
    assert_eq!(source.data().phase.pulsed, None);

    let ctx = DoodadCtx::new(&f.server, f.world.clone(), source.clone());
    do_change_phase(&ctx, Some(&f.caster()), 10);
    assert_eq!(source.data().phase.logic_output, Some(false));
    assert_eq!(group(&receiver), 21, "with a caster the same group pulses");
}

/// A deleted doodad takes no part in the family network: `subscribe` does not replay the output
/// of a deleted provider (a late subscriber stays put) and `emit` does not reach a deleted family
/// target. `receive` refuses a deleted target as well, so the `emit` half pins the outcome rather
/// than its own filter.
#[tokio::test]
async fn deleted_doodads_take_no_part_in_family_routing() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 1,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
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
        );
    let f = fx(b);
    let source = place(&f, 100, 101.0);
    source.data().family_id = 7;
    let live = place(&f, 101, 102.0);
    let gone = place(&f, 101, 103.0);
    live.data().family_id = 500;
    gone.data().family_id = 500;
    gone.data().phase.deleted = true;

    logic::emit(
        &DoodadCtx::new(&f.server, f.world.clone(), source.clone()),
        None,
        true,
    );
    assert_eq!(group(&live), 21);
    assert_eq!(group(&gone), 20, "a deleted target receives nothing");

    let adopter = place(&f, 101, 104.0);
    assert_eq!(
        group(&adopter),
        21,
        "a late subscriber adopts the live source's output"
    );
    source.data().phase.deleted = true;
    let late = place(&f, 101, 105.0);
    assert_eq!(
        group(&late),
        20,
        "the output of a deleted provider is not replayed"
    );
}

/// `clear_links` (a deleted doodad, a cleanup) drops a pending `DoodadFuncLogic` output: it is
/// cleared and the generation moves on, so the scheduled task cannot emit afterwards.
#[tokio::test]
async fn clear_links_cancels_a_pending_logic_output() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::Logic(DoodadFuncLogic {
                id: 1,
                operation_id: 3,
                delay_id: 1,
            }),
        )
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 2,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 3,
                flag: true,
                next_phase: 21,
            }),
        )
        .template(102, &[(30, 1)]);
    let f = fx(b);
    let source = place(&f, 100, 101.0);
    let receiver = place(&f, 101, 102.0);
    receiver.data().family_id = 500;
    let driver = place(&f, 102, 103.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), driver);

    logic::receive(&ctx, &source, None, true, true);
    let generation = source.data().phase.pending_output_generation;
    assert_eq!(source.data().phase.pending_output, Some(true));
    logic::clear_links(&DoodadCtx::new(&f.server, f.world.clone(), source.clone()));
    assert_eq!(source.data().phase.pending_output, None);
    assert_ne!(
        source.data().phase.pending_output_generation,
        generation,
        "the scheduled output's task no longer matches"
    );
    f.advance(2_000);
    assert_eq!(source.data().phase.logic_output, None);
    assert_eq!(group(&receiver), 20, "the cleared output never emits");
}

/// Family routing needs the `FamilyId` of the placement rows: a provider family names the
/// receiver *family*, not a template, so a source whose group provides 500 and a target whose
/// group subscribes to 500 do not meet while both placements carry no `FamilyId` (the shipped
/// Nachashgar rooms 4 / 6 / 7 pairs such as 6643 -> 6646 are of this kind: main matched them by
/// template, the logic network does not).
#[tokio::test]
async fn no_family_id_on_the_placement_means_no_family_routing() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .phase_func(
            10,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 1,
                family_id: 500,
            }),
        )
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::LogicFamilySubscriber(DoodadFuncLogicFamilySubscriber {
                id: 2,
                family_id: 500,
            }),
        )
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 3,
                flag: true,
                next_phase: 21,
            }),
        );
    let f = fx(b);
    let source = place(&f, 100, 101.0);
    let target = place(&f, 101, 102.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), source.clone());
    logic::emit(&ctx, Some(&f.caster()), true);
    assert_eq!(
        group(&target),
        20,
        "the provider and subscriber rows alone do not route"
    );

    // An identified source does not make a template match either: the target is addressed by
    // its own placement's family.
    source.data().family_id = 500;
    logic::emit(&ctx, Some(&f.caster()), true);
    assert_eq!(group(&target), 20, "the target's placement has no family");

    // With the family ids on both placements (500 names the receiver, the subscribed 500 the
    // source) the same rows connect.
    target.data().family_id = 500;
    logic::emit(&ctx, Some(&f.caster()), true);
    assert_eq!(group(&target), 21);
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

    // Exact upper bound of `[0, 500)` misses.
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(500));
    let d_edge = manager::create(&f.server, &f.world, 0x501, TEMPLATE, None, true).unwrap();
    manager::init_doodad(&f.server, &f.world, &d_edge);
    assert_eq!(group(&d_edge), 10);

    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(499));
    let d2 = manager::create(&f.server, &f.world, 0x502, TEMPLATE, None, true).unwrap();
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
    // One roll per ratio phase: 10:RatioChange hits `[0, 500)` -> 20; 20:self-respawn hits
    // `[0, 2000)` -> new TEMPLATE, its 10 hits -> 20 again (the repeated source); second 20 rolls
    // 2500, missing `[0, 2000)` and hitting `[2000, 4000)` -> TEMPLATE + 1.
    f.server
        .systems
        .npcs
        .doodads
        .queue_phase_ratios(&[100, 100, 100, 2500]);
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
    // One roll per ratio phase: Tod phases consume none; phase 11 rolls 2500, missing `[0, 2000)`
    // and hitting `[2000, 4000)` -> TEMPLATE + 2.
    f.server.systems.npcs.doodads.queue_phase_ratios(&[2500]);
    let hit = new_spawner(TEMPLATE, 522);
    hit.spawn(&f.server, 0).expect("spawns the board");
    let variant = hit.last().expect("the variant is tracked");
    assert_eq!(variant.data().template_id, TEMPLATE + 2);
    assert_eq!(group(&variant), 30);
    assert_eq!(hit.ratio_respawn_peak_depth(), 1);
    assert_eq!(hit.ratio_respawn_chain_depth(), 0);

    // Roll misses both cumulative intervals `[0, 2000)` and `[2000, 4000)`: the board stays in its
    // respawning phase, with no replacement and no rejected chain.
    f.server.systems.npcs.doodads.queue_phase_ratios(&[9000]);
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
async fn ratio_change_partitions_single_roll_across_fish_school_species_and_rare_procs() {
    // Saltwater fish school 6448 phase 16998 shape: four RatioChange rows summing to 10000
    // (4000/1500/1000/3500) plus a 1000 ms fallback Timer -> Bluefin Tuna (17231). Every roll in
    // `0..10000` must match its authored cumulative interval and never fall through to the timer.
    const SALT: u32 = 100;
    const FRESH: u32 = 101;
    const RARE: u32 = 102;
    let b = Builder::default()
        .template(SALT, &[(10, 1), (11, 2), (12, 2), (13, 2), (14, 2)])
        .phase_func(10, ratio(1, 4_000, 11))
        .phase_func(10, ratio(2, 1_500, 12))
        .phase_func(10, ratio(3, 1_000, 13))
        .phase_func(10, ratio(4, 3_500, 14))
        .phase_func(10, timer(5, 1_000, 11))
        .template(FRESH, &[(20, 1), (21, 2), (22, 2), (23, 2)])
        .phase_func(20, ratio(6, 3_400, 21))
        .phase_func(20, ratio(7, 3_300, 22))
        .phase_func(20, ratio(8, 3_300, 23))
        .phase_func(20, timer(9, 1_000, 21))
        .template(RARE, &[(30, 1), (31, 2), (32, 2)])
        .phase_func(30, ratio(10, 9_999, 31))
        .phase_func(30, ratio(11, 1, 32));
    let f = fx(b);

    let mut next_id = 0x600u32;
    for (roll, expected) in [
        (0, 11),
        (3_999, 11),
        (4_000, 12),
        (5_499, 12),
        (5_500, 13),
        (6_499, 13),
        (6_500, 14),
        (9_999, 14),
    ] {
        f.server
            .systems
            .npcs
            .doodads
            .set_forced_phase_ratio(Some(roll));
        let d = manager::create(&f.server, &f.world, next_id, SALT, None, true).unwrap();
        next_id += 1;
        manager::init_doodad(&f.server, &f.world, &d);
        assert_eq!(group(&d), expected, "saltwater roll {roll}");
    }
    assert_eq!(
        f.advance(1_000),
        0,
        "fallback timer must never be armed when cumulative ratios cover 0..10000"
    );

    for (roll, expected) in [
        (0, 21),
        (3_399, 21),
        (3_400, 22),
        (6_699, 22),
        (6_700, 23),
        (9_999, 23),
    ] {
        f.server
            .systems
            .npcs
            .doodads
            .set_forced_phase_ratio(Some(roll));
        let d = manager::create(&f.server, &f.world, next_id, FRESH, None, true).unwrap();
        next_id += 1;
        manager::init_doodad(&f.server, &f.world, &d);
        assert_eq!(group(&d), expected, "freshwater roll {roll}");
    }
    assert_eq!(f.advance(1_000), 0);

    // 9999 + 1 rare proc: `ratio = 9999` covers `[0, 9999)` so roll `9999` reaches the 0.01% row.
    for (roll, expected) in [(9_998, 31), (9_999, 32)] {
        f.server
            .systems
            .npcs
            .doodads
            .set_forced_phase_ratio(Some(roll));
        let d = manager::create(&f.server, &f.world, next_id, RARE, None, true).unwrap();
        next_id += 1;
        manager::init_doodad(&f.server, &f.world, &d);
        assert_eq!(group(&d), expected, "rare-proc roll {roll}");
    }
}

#[tokio::test]
async fn ratio_respawn_partitions_single_roll_and_does_not_leak_rejected_or_prior_phase_totals() {
    const REJECT_SRC: u32 = TEMPLATE + 10;
    const CHAIN_SRC: u32 = TEMPLATE + 20;
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .template(TEMPLATE + 1, &[(11, 1)])
        .template(TEMPLATE + 2, &[(12, 1)])
        .template(TEMPLATE + 3, &[(13, 1)])
        .phase_func(10, ratio_respawn(1, 2_000, TEMPLATE + 1))
        .phase_func(10, ratio_respawn(2, 3_000, TEMPLATE + 2))
        .phase_func(10, ratio_respawn(3, 5_000, TEMPLATE + 3))
        .template(REJECT_SRC, &[(20, 1)])
        .phase_func(20, ratio_respawn(4, 2_000, 999_999))
        .phase_func(20, ratio_respawn(5, 8_000, TEMPLATE + 1))
        .template(CHAIN_SRC, &[(30, 1), (31, 2), (32, 2), (33, 2), (34, 2)])
        .phase_func(30, ratio(6, 4_000, 31))
        .phase_func(30, ratio(7, 6_000, 32))
        .phase_func(32, ratio(8, 3_000, 33))
        .phase_func(32, ratio(9, 7_000, 34));
    let f = fx(b);

    // Cumulative buckets `[0, 2000)`, `[2000, 5000)`, `[5000, 10000)`.
    let mut spawner_id = 600u32;
    for (roll, expected_template) in [
        (0, TEMPLATE + 1),
        (1_999, TEMPLATE + 1),
        (2_000, TEMPLATE + 2),
        (4_999, TEMPLATE + 2),
        (5_000, TEMPLATE + 3),
        (9_999, TEMPLATE + 3),
    ] {
        f.server
            .systems
            .npcs
            .doodads
            .set_forced_phase_ratio(Some(roll));
        let spawner = new_spawner(TEMPLATE, spawner_id);
        spawner_id += 1;
        spawner.spawn(&f.server, 0).expect("spawns");
        let live = spawner.last().expect("replacement tracked");
        assert_eq!(live.data().template_id, expected_template, "roll {roll}");
    }

    // A rejected RatioRespawn (missing template) must not spill its roll into the next sibling.
    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(500));
    let spawner_miss = new_spawner(REJECT_SRC, spawner_id);
    spawner_id += 1;
    let kept = spawner_miss.spawn(&f.server, 0).expect("spawns");
    assert_eq!(kept.data().template_id, REJECT_SRC);
    assert!(!kept.data().phase.deleted);

    f.server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(2_000));
    let spawner_hit = new_spawner(REJECT_SRC, spawner_id);
    spawner_hit.spawn(&f.server, 0).expect("spawns");
    assert_eq!(
        spawner_hit.last().expect("replacement").data().template_id,
        TEMPLATE + 1
    );

    // `cumulative_phase_ratio` must reset to 0 on every phase entry: 30 accumulates 10000 to
    // select 32 (`5000 in [4000, 10000)`), and 32 must still match `1000 in [0, 3000)` -> 33.
    f.server.systems.npcs.doodads.set_forced_phase_ratio(None);
    f.server
        .systems
        .npcs
        .doodads
        .queue_phase_ratios(&[5_000, 1_000]);
    let chained = manager::create(&f.server, &f.world, 0x700, CHAIN_SRC, None, true).unwrap();
    manager::init_doodad(&f.server, &f.world, &chained);
    assert_eq!(group(&chained), 33);
}

#[tokio::test]
async fn clout_with_origin_source_changes_only_its_own_phase() {
    // Doodad 5007 (NPC 10119's summon) next to the Amber Boulder 4252 (quest 3447): the clout
    // area stays at the summon (see the tests below) and its NextPhase / Final belong to it.
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

/// `aoe_shapes` 719: a 2 m half-length x 15.5 m half-width cuboid, 4 m high.
fn comet_shape() -> AreaShape {
    AreaShape {
        id: 1,
        kind: AREA_SHAPE_CUBOID,
        value1: 2.0,
        value2: 15.5,
        value3: 4.0,
    }
}

/// The clout of Comet's Boon (doodad 5017, `UseOriginSource`, buff 3771).
fn comet_clout(buff: u32) -> FuncTemplate {
    clout(buff, 1, 1000)
}

/// A `UseOriginSource` clout (`duration` ms) as its template row `id`.
fn clout(buff: u32, id: u32, duration: i32) -> FuncTemplate {
    clout_to(buff, id, duration, -1)
}

/// [`clout`] that moves its doodad to func group `next_phase` when it ends (`-1`: deletes it).
fn clout_to(buff: u32, id: u32, duration: i32, next_phase: i32) -> FuncTemplate {
    FuncTemplate::Clout(DoodadFuncClout {
        id,
        duration,
        buff_id: buff,
        next_phase,
        aoe_shape_id: 1,
        use_origin_source: true,
        ..DoodadFuncClout::default()
    })
}

#[tokio::test]
async fn chummed_schools_keep_their_aura_at_the_school_when_the_boat_leaves_and_returns() {
    const BUFF: u32 = 5767;
    for school_template in [SALTWATER_FISH_SCHOOL, FRESHWATER_FISH_SCHOOL] {
        let server = Builder::default()
            .template(school_template, &[(10, 1), (11, 2), (12, 2)])
            .template(TEMPLATE, &[(20, 1)])
            .phase_func(11, clout(BUFF, 1, 0))
            .clout_server(
                BUFF,
                AreaShape {
                    id: 1,
                    kind: crate::systems::npcs::area_trigger::AREA_SHAPE_SPHERE,
                    value1: 50.0,
                    value2: 0.0,
                    value3: 0.0,
                },
            );
        let world = server.world.world(0).unwrap();
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Angler", 100.0, 100.0),
            0x400,
        );
        let me = WorldObject::from(&player);
        let mut hull = crate::models::slave::Slave::new(1, 1);
        hull.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let boat = crate::world::Entity::new(0x401, hull);
        world.spawn(&boat, &server.view_ctx());
        let boat_obj = WorldObject::from(&boat);
        world.attach_child(&boat_obj, &me);
        me.with_transform(|t| t.set_xyz(0.0, 0.0, 1.0));
        let fitting = manager::create(&server, &world, 0x402, TEMPLATE, None, true).unwrap();
        world.attach_child(&boat_obj, &WorldObject::from(&fitting));
        manager::spawn(&server, &world, &fitting);
        let school = manager::create(&server, &world, 0x403, school_template, None, true).unwrap();
        school.data().transform.set_xyz(120.0, 100.0, 100.0);
        manager::spawn(&server, &world, &school);
        manager::do_change_phase(&server, &world, &school, Some(&me), 11);
        let tick = || server.systems.npcs.area_triggers.tick(&server);
        let has_buff = || player.data().unit.buffs.check_buff(BUFF);
        tick();
        assert!(has_buff());

        for (x, expected) in [(300.0, false), (100.0, true)] {
            boat.data().unit.transform.x = x;
            crate::systems::ai::npc::check_moved_position(&server, &boat_obj);
            tick();
            assert_eq!(has_buff(), expected, "hull position {x}");
        }
        manager::do_change_phase(&server, &world, &school, None, 12);
        tick();
        assert!(!has_buff(), "school expiry removes the aura");
    }
}

/// Summons `template` 15.49 m in front of `player` (`SummonDoodad` order: create, place,
/// `InitDoodad`, spawn).
fn summon_in_front(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    player: &PlayerRef,
    obj_id: u32,
    template: u32,
) -> DoodadRef {
    let summon = manager::create(
        server,
        world,
        obj_id,
        template,
        Some(&WorldObject::from(player)),
        true,
    )
    .unwrap();
    summon.data().transform.set_xyz(100.0, 100.0 + 15.49, 100.0);
    manager::init_doodad(server, world, &summon);
    manager::spawn(server, world, &summon);
    summon
}

#[tokio::test]
async fn summoned_clout_keeps_the_summon_as_area_origin() {
    // A planted doodad 3.5 m from the caster must not move Comet's Boon's clout area: the
    // caster stands 15.49 m behind the summon (inside its 15.5 m half-width) and 2.5 m off the
    // nearby doodad's 2.0 m half-length. The C# anchor put the caster outside the area, so
    // buff 3771 was never applied next to any doodad.
    const BYSTANDER: u32 = 200;
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(BYSTANDER, &[(20, 1)])
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, comet_clout(BUFF))
        .clout_server(BUFF, comet_shape());
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    client.drain();
    let world = server.world.world(0).unwrap();

    let bystander = manager::create(&server, &world, 0x600, BYSTANDER, None, true).unwrap();
    bystander.data().transform.set_xyz(102.5, 102.5, 100.0);
    manager::spawn(&server, &world, &bystander);

    let summon = summon_in_front(&server, &world, &player, 0x500, TEMPLATE);
    assert_eq!(
        server
            .systems
            .npcs
            .area_triggers
            .attached_to(summon.obj_id)
            .len(),
        1,
        "the clout must stay attached to the summon, not to the nearby doodad"
    );
    server.systems.npcs.area_triggers.tick(&server);
    assert!(player.data().unit.buffs.check_buff(BUFF));

    // The clout task ends the area: the buff leaves with the trigger.
    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 1000), 1);
    assert!(!player.data().unit.buffs.check_buff(BUFF));
    assert!(summon.data().phase.deleted);
    assert!(server
        .systems
        .npcs
        .area_triggers
        .attached_to(summon.obj_id)
        .is_empty());
}

#[tokio::test]
async fn summoned_clout_applies_again_after_its_trigger_ended() {
    // Cast, buff expires with the clout, cast again: the trigger count and buff state must not
    // leak across summons (the second buff is created, not suppressed by a stale count).
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, comet_clout(BUFF))
        .clout_server(BUFF, comet_shape());
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    client.drain();
    let world = server.world.world(0).unwrap();

    for obj_id in [0x500, 0x501] {
        let summon = summon_in_front(&server, &world, &player, obj_id, TEMPLATE);
        server.systems.npcs.area_triggers.tick(&server);
        assert!(player.data().unit.buffs.check_buff(BUFF));
        assert_eq!(player.data().unit.trigger_count(BUFF), 1);
        server.systems.npcs.doodads.tasks.advance(&server, 1000);
        assert!(!player.data().unit.buffs.check_buff(BUFF));
        assert_eq!(player.data().unit.trigger_count(BUFF), 0);
        assert!(summon.data().phase.deleted);
    }
}

#[tokio::test]
async fn clout_ending_in_a_phase_change_is_not_entered_again_by_later_ticks() {
    // `NextPhase != -1`: the clout task removes the trigger but its doodad stays visible, and the
    // manager drops a removed trigger only at the end of the next tick. That tick must not run
    // the removed trigger (its `OnDelete` already released the unit): entering it again would
    // count the unit once more and re-apply the buff, which nothing ever removes.
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_func(10, clout_to(BUFF, 1, 1000, 11))
        .clout_server(BUFF, comet_shape());
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    client.drain();
    let world = server.world.world(0).unwrap();

    let summon = summon_in_front(&server, &world, &player, 0x500, TEMPLATE);
    server.systems.npcs.area_triggers.tick(&server);
    assert!(player.data().unit.buffs.check_buff(BUFF));
    assert_eq!(player.data().unit.trigger_count(BUFF), 1);

    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 1000), 1);
    assert_eq!(group(&summon), 11);
    assert!(summon.is_visible(), "the doodad outlives its clout");
    assert!(!player.data().unit.buffs.check_buff(BUFF));
    assert_eq!(player.data().unit.trigger_count(BUFF), 0);
    for _ in 0..3 {
        server.systems.npcs.area_triggers.tick(&server);
        assert!(!player.data().unit.buffs.check_buff(BUFF));
        assert_eq!(player.data().unit.trigger_count(BUFF), 0);
    }
    assert!(server.systems.npcs.area_triggers.triggers().is_empty());
}

#[tokio::test]
async fn clout_of_a_doodad_initialised_before_its_spawn_waits_for_the_spawn() {
    // `SummonDoodad` / `SpawnDoodad`: `InitDoodad()` runs on the invisible doodad, then `Spawn()`.
    // An area tick between the two (the tick runs on its own thread) must neither drop the new
    // trigger (its owner is not visible yet) nor enter units: the tick needs the owner's region,
    // which `Spawn` assigns after it showed the owner.
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, comet_clout(BUFF))
        .clout_server(BUFF, comet_shape());
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    client.drain();
    let world = server.world.world(0).unwrap();

    let summon = manager::create(
        &server,
        &world,
        0x500,
        TEMPLATE,
        Some(&WorldObject::from(&player)),
        true,
    )
    .unwrap();
    summon.data().transform.set_xyz(100.0, 100.0 + 15.49, 100.0);
    manager::init_doodad(&server, &world, &summon);
    assert!(!summon.is_visible());
    for _ in 0..3 {
        server.systems.npcs.area_triggers.tick(&server);
        assert_eq!(
            server
                .systems
                .npcs
                .area_triggers
                .attached_to(summon.obj_id)
                .len(),
            1,
            "the trigger of the unspawned doodad stays"
        );
        assert!(!player.data().unit.buffs.check_buff(BUFF));
    }

    manager::spawn(&server, &world, &summon);
    server.systems.npcs.area_triggers.tick(&server);
    assert!(player.data().unit.buffs.check_buff(BUFF));
    assert_eq!(player.data().unit.trigger_count(BUFF), 1);
}

#[tokio::test]
async fn spawned_doodad_clout_with_origin_source_borrows_a_doodad_near_the_caster() {
    // An already spawned (visible) doodad keeps the C# anchor `WorldManager.GetAround<Doodad>(
    // caster, 4f)`, unlike a summon initialised before its Spawn (Comet's Boon, tests above).
    const BYSTANDER: u32 = 200;
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(BYSTANDER, &[(20, 1)])
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, comet_clout(BUFF))
        .clout_server(BUFF, comet_shape());
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    client.drain();
    let world = server.world.world(0).unwrap();

    let bystander = manager::create(&server, &world, 0x600, BYSTANDER, None, true).unwrap();
    bystander.data().transform.set_xyz(101.0, 101.0, 100.0);
    manager::spawn(&server, &world, &bystander);

    // Created, spawned and only then initialised, 14 m from the caster (a planted doodad).
    let owner = manager::create(
        &server,
        &world,
        0x500,
        TEMPLATE,
        Some(&WorldObject::from(&player)),
        true,
    )
    .unwrap();
    owner.data().transform.set_xyz(114.0, 100.0, 100.0);
    manager::spawn(&server, &world, &owner);
    manager::init_doodad(&server, &world, &owner);

    assert_eq!(
        server
            .systems
            .npcs
            .area_triggers
            .attached_to(bystander.obj_id)
            .len(),
        1,
        "a visible doodad's clout must borrow the doodad within 4 m of the caster"
    );
    assert!(server
        .systems
        .npcs
        .area_triggers
        .attached_to(owner.obj_id)
        .is_empty());
    // The caster stands inside the borrowed doodad's area.
    server.systems.npcs.area_triggers.tick(&server);
    assert!(player.data().unit.buffs.check_buff(BUFF));
    assert_eq!(player.data().unit.trigger_count(BUFF), 1);

    // The clout task still acts on the doodad that owns it (deviation of 2026-10-30).
    server.systems.npcs.doodads.tasks.advance(&server, 1000);
    assert!(owner.data().phase.deleted);
    assert!(!bystander.data().phase.deleted);
    assert!(world.doodad(0x600).is_some());

    // The summon is gone but the trigger's owner (the borrowed doodad) stays visible: the buff
    // left with the clout and later ticks must not bring it back.
    assert!(!player.data().unit.buffs.check_buff(BUFF));
    assert_eq!(player.data().unit.trigger_count(BUFF), 0);
    assert!(server
        .systems
        .npcs
        .area_triggers
        .attached_to(bystander.obj_id)
        .is_empty());
    for _ in 0..3 {
        server.systems.npcs.area_triggers.tick(&server);
        assert!(!player.data().unit.buffs.check_buff(BUFF));
        assert_eq!(player.data().unit.trigger_count(BUFF), 0);
    }
    assert!(server.systems.npcs.area_triggers.triggers().is_empty());
}

/// An NPC at (100, 100) that owns the doodads its skills summon.
fn spawn_owner_npc(server: &GameServer, world: &Arc<WorldInstance>) -> WorldObject {
    // Putrid Demon (11357), the caster of the real Poison Belch, has faction 115.
    spawn_owner_npc_with_faction(server, world, HAZARD_OWNER_FACTION)
}

fn spawn_owner_npc_with_faction(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    faction_id: u32,
) -> WorldObject {
    let npc = crate::world::object::Entity::new(
        server.ids.object.get_next_id().unwrap(),
        crate::models::npc::Npc::new(1),
    );
    {
        let mut n = npc.data();
        n.unit.faction_id = faction_id;
        n.unit
            .transform
            .set_position(100.0, 100.0, 100.0, 0.0, 0.0, 0.0);
        n.unit.transform.instance_id = world.id;
    }
    let _ = world.spawn(&npc, &server.view_ctx());
    WorldObject::from(&npc)
}

/// The clout of an NPC-summoned hazard (Kroloal Cradle's Poison Belch 5151: no inside buff,
/// one damage effect, Hostile, `UseOriginSource`).
fn npc_hazard_clout(relation: u8, effect_id: u32) -> FuncTemplate {
    FuncTemplate::Clout(DoodadFuncClout {
        id: 1,
        duration: 1000,
        tick: 100,
        target_relation: relation,
        next_phase: -1,
        aoe_shape_id: 1,
        use_origin_source: true,
        effects: vec![effect_id],
        ..DoodadFuncClout::default()
    })
}

#[tokio::test]
async fn npc_summoned_origin_clout_casts_as_its_npc_at_its_own_doodad() {
    // Poison Belch's doodad reaches its clout through a timer after the spawn, with the
    // summoning Putrid Demon as the caster. A visible player-cast doodad borrows a prop within
    // 4 m of the caster (test above); the NPC's hazard stays where it was placed, under the
    // victim, and runs as the NPC (its relation and damage source), not the faction-less doodad.
    use crate::systems::npcs::area_trigger::skill_target_relation::{ANY, HOSTILE};
    const BYSTANDER: u32 = 200;
    const EFFECT: u32 = 77;
    for (relation, effects) in [(HOSTILE, vec![EFFECT]), (ANY, Vec::new())] {
        let server = Builder::default()
            .template(BYSTANDER, &[(20, 1)])
            .template(TEMPLATE, &[(10, 1)])
            .phase_func(10, npc_hazard_clout(relation, EFFECT))
            .clout_server(631, comet_shape());
        let world = server.world.world(0).unwrap();
        let npc = spawn_owner_npc(&server, &world);
        let bystander = manager::create(&server, &world, 0x600, BYSTANDER, None, true).unwrap();
        bystander.data().transform.set_xyz(101.0, 101.0, 100.0);
        manager::spawn(&server, &world, &bystander);
        let owner = manager::create(&server, &world, 0x500, TEMPLATE, Some(&npc), true).unwrap();
        owner.data().transform.set_xyz(104.5, 100.0, 100.0);
        manager::spawn(&server, &world, &owner);
        manager::init_doodad(&server, &world, &owner);

        let triggers = &server.systems.npcs.area_triggers;
        assert!(triggers.attached_to(bystander.obj_id).is_empty());
        let attached = triggers.attached_to(owner.obj_id);
        assert_eq!(attached.len(), 1, "the hazard stays at its own doodad");
        let trigger = &attached[0];
        assert_eq!(
            trigger.caster.as_ref().map(WorldObject::obj_id),
            Some(npc.obj_id()),
            "the summoning NPC is the origin source"
        );
        // A Hostile hazard carries its damage. An Any area would also take the NPC and its
        // allies, so only its beneficial buffs apply (none here).
        assert_eq!(trigger.effects_per_buff.get(&0), Some(&effects));
    }
}

#[tokio::test]
async fn hostile_clout_of_an_owner_without_a_known_faction_keeps_only_good_buffs() {
    // A Hostile relation from a caster whose faction is not in the data cannot pick a side
    // (`can_attack`'s missing-faction branch admits everyone, the summoner included), so the
    // payload falls back to beneficial buffs.
    use crate::systems::npcs::area_trigger::skill_target_relation::HOSTILE;
    const UNKNOWN_FACTION: u32 = 9_999;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, npc_hazard_clout(HOSTILE, 77))
        .clout_server(631, comet_shape());
    let world = server.world.world(0).unwrap();
    let npc = spawn_owner_npc_with_faction(&server, &world, UNKNOWN_FACTION);
    let owner = manager::create(&server, &world, 0x500, TEMPLATE, Some(&npc), true).unwrap();
    owner.data().transform.set_xyz(104.5, 100.0, 100.0);
    manager::spawn(&server, &world, &owner);
    manager::init_doodad(&server, &world, &owner);

    let attached = server.systems.npcs.area_triggers.attached_to(owner.obj_id);
    assert_eq!(attached.len(), 1);
    assert_eq!(
        attached[0].caster.as_ref().map(WorldObject::obj_id),
        Some(npc.obj_id()),
        "the summoning NPC is still the origin source"
    );
    assert_eq!(attached[0].effects_per_buff.get(&0), Some(&Vec::new()));
}

#[tokio::test]
async fn ownerless_placed_hazard_without_an_inside_buff_applies_its_full_payload() {
    // A placed environmental hazard (no owner unit, no supplied caster) carries its linked
    // effects. Before, every clout without an inside buff kept only its beneficial buffs.
    use crate::systems::npcs::area_trigger::skill_target_relation::ANY;
    const EFFECT: u32 = 77;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, npc_hazard_clout(ANY, EFFECT))
        .clout_server(631, comet_shape());
    let world = server.world.world(0).unwrap();
    let placed = manager::create(&server, &world, 0x500, TEMPLATE, None, true).unwrap();
    placed.data().transform.set_xyz(104.5, 100.0, 100.0);
    manager::spawn(&server, &world, &placed);
    let ctx = DoodadCtx::new(&server, world.clone(), placed.clone());
    do_change_phase(&ctx, None, 10);

    let attached = server.systems.npcs.area_triggers.attached_to(placed.obj_id);
    assert_eq!(attached.len(), 1);
    assert_eq!(
        attached[0].caster.as_ref().map(WorldObject::obj_id),
        Some(placed.obj_id)
    );
    assert_eq!(attached[0].effects_per_buff.get(&0), Some(&vec![EFFECT]));
}

#[tokio::test]
async fn origin_clout_of_a_doodad_the_npc_did_not_summon_keeps_the_doodad_as_caster() {
    // An NPC that changes a placed doodad's phase is not that doodad's origin source.
    use crate::systems::npcs::area_trigger::skill_target_relation::HOSTILE;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, npc_hazard_clout(HOSTILE, 77))
        .clout_server(631, comet_shape());
    let world = server.world.world(0).unwrap();
    let npc = spawn_owner_npc(&server, &world);
    let placed = manager::create(&server, &world, 0x500, TEMPLATE, None, true).unwrap();
    placed.data().transform.set_xyz(104.5, 100.0, 100.0);
    manager::spawn(&server, &world, &placed);
    let ctx = DoodadCtx::new(&server, world.clone(), placed.clone());
    do_change_phase(&ctx, Some(&npc), 10);

    let attached = server.systems.npcs.area_triggers.attached_to(placed.obj_id);
    assert_eq!(attached.len(), 1);
    assert_eq!(
        attached[0].caster.as_ref().map(WorldObject::obj_id),
        Some(placed.obj_id)
    );
    assert_eq!(attached[0].effects_per_buff.get(&0), Some(&Vec::new()));
}

#[tokio::test]
async fn overlapping_clouts_keep_the_buff_until_the_last_one_ends() {
    // Two casters' comets cover one ally. The first clout's delete removes its trigger twice
    // (`Doodad.Delete` then `DoodadFuncCloutTask`); `AreaTrigger.OnDelete` must take its units
    // or the second decrement drops the ally's buff while the other clout still runs.
    const OTHER: u32 = 101;
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, clout(BUFF, 1, 1000))
        .template(OTHER, &[(20, 1)])
        .phase_func(20, clout(BUFF, 2, 3000))
        .clout_server(BUFF, comet_shape());
    let mut ca = TestClient::new(&server, 1, 1001);
    let a = spawn_player(
        &server,
        &ca,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    let mut cb = TestClient::new(&server, 2, 1002);
    let b = spawn_player(&server, &cb, test_character(2, "Beta", 101.0, 100.0), 0x811);
    let mut cc = TestClient::new(&server, 3, 1003);
    let ally = spawn_player(
        &server,
        &cc,
        test_character(3, "Gamma", 100.5, 103.0),
        0x820,
    );
    ca.drain();
    cb.drain();
    cc.drain();
    let world = server.world.world(0).unwrap();

    for (obj_id, template, caster, x) in [(0x500, TEMPLATE, &a, 100.0), (0x501, OTHER, &b, 101.0)] {
        let d = manager::create(
            &server,
            &world,
            obj_id,
            template,
            Some(&WorldObject::from(caster)),
            true,
        )
        .unwrap();
        d.data().transform.set_xyz(x, 101.0, 100.0);
        manager::init_doodad(&server, &world, &d);
        manager::spawn(&server, &world, &d);
    }
    let casters = server.world.world(0).unwrap().all_doodads();
    assert_eq!(casters.len(), 2);

    server.systems.npcs.area_triggers.tick(&server);
    assert!(ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 2);

    // The 1000 ms clout ends: the ally is still inside the 3000 ms one.
    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 1000), 1);
    assert!(ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 1);
    for _ in 0..2 {
        server.systems.npcs.area_triggers.tick(&server);
        assert!(ally.data().unit.buffs.check_buff(BUFF));
        assert_eq!(ally.data().unit.trigger_count(BUFF), 1);
    }

    // The last clout ends: now the buff goes.
    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 2000), 1);
    assert!(!ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 0);
}

#[tokio::test]
async fn overlapping_clouts_ending_in_a_phase_change_keep_the_buff_until_the_last_one_ends() {
    // As above, but the doodads stay visible after their clouts (`NextPhase != -1`): the area
    // ticks between the two ends must neither enter the ally in the finished clout again nor
    // let its count outlive the second clout.
    const OTHER: u32 = 101;
    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_func(10, clout_to(BUFF, 1, 1000, 11))
        .template(OTHER, &[(20, 1), (21, 2)])
        .phase_func(20, clout_to(BUFF, 2, 3000, 21))
        .clout_server(BUFF, comet_shape());
    let mut ca = TestClient::new(&server, 1, 1001);
    let a = spawn_player(
        &server,
        &ca,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    let mut cb = TestClient::new(&server, 2, 1002);
    let b = spawn_player(&server, &cb, test_character(2, "Beta", 101.0, 100.0), 0x811);
    let mut cc = TestClient::new(&server, 3, 1003);
    let ally = spawn_player(
        &server,
        &cc,
        test_character(3, "Gamma", 100.5, 103.0),
        0x820,
    );
    ca.drain();
    cb.drain();
    cc.drain();
    let world = server.world.world(0).unwrap();

    let mut doodads = Vec::new();
    for (obj_id, template, caster, x) in [(0x500, TEMPLATE, &a, 100.0), (0x501, OTHER, &b, 101.0)] {
        let d = manager::create(
            &server,
            &world,
            obj_id,
            template,
            Some(&WorldObject::from(caster)),
            true,
        )
        .unwrap();
        d.data().transform.set_xyz(x, 101.0, 100.0);
        manager::init_doodad(&server, &world, &d);
        manager::spawn(&server, &world, &d);
        doodads.push(d);
    }

    server.systems.npcs.area_triggers.tick(&server);
    assert!(ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 2);

    // The 1000 ms clout ends: the ally is still inside the 3000 ms one.
    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 1000), 1);
    assert_eq!(group(&doodads[0]), 11);
    assert_eq!(ally.data().unit.trigger_count(BUFF), 1);
    for _ in 0..2 {
        server.systems.npcs.area_triggers.tick(&server);
        assert!(ally.data().unit.buffs.check_buff(BUFF));
        assert_eq!(ally.data().unit.trigger_count(BUFF), 1);
    }

    // The last clout ends: the buff goes and stays away.
    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 2000), 1);
    assert_eq!(group(&doodads[1]), 21);
    for _ in 0..2 {
        assert!(!ally.data().unit.buffs.check_buff(BUFF));
        assert_eq!(ally.data().unit.trigger_count(BUFF), 0);
        server.systems.npcs.area_triggers.tick(&server);
    }
    assert!(server.systems.npcs.area_triggers.triggers().is_empty());
}

#[tokio::test(start_paused = true)]
async fn delayed_spawn_doodad_keeps_its_clout_until_the_spawn() {
    // `SpawnDoodad` (`special_effects` type 15, `value2` = 1000) spawns the doodad after the
    // delay (`SpawnDoodad.cs`: `InitDoodad(); Thread.Sleep(delay); Spawn()`). Here the init and
    // the spawn run together after the delay; the clout must exist then and apply its buff, and
    // area ticks during the wait neither create nor drop it. The clout is shorter (500 ms) than
    // the delay, so starting its timer at the cast would end it before the doodad exists.
    use std::time::Duration;

    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};

    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, clout(BUFF, 1, 500))
        .clout_server(BUFF, comet_shape());
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let caster = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    let mut ally_client = TestClient::new(&server, 2, 1002);
    // The synthetic world has no geodata: `SpawnDoodad` puts the doodad on the fallback ground
    // height 0, so the ally stands on it (the caster stays at 100 and out of the area).
    let mut ally_character = test_character(2, "Beta", 100.0, 103.0);
    ally_character.unit.transform.set_xyz(100.0, 103.0, 0.0);
    let ally = spawn_player(&server, &ally_client, ally_character, 0x820);
    client.drain();
    ally_client.drain();
    let world = server.world.world(0).unwrap();

    let cast = || EffectArgs {
        caster: Some(WorldObject::from(&caster)),
        caster_caster: SkillCaster::Unit(caster.obj_id),
        target: Target::Obj(WorldObject::from(&caster)),
        target_caster: SkillCastTarget::Unit(caster.obj_id),
        cast: CastAction::Skill {
            skill_id: 18_222,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    };
    crate::systems::skills::effects::special::execute(
        &server,
        15,
        &cast(),
        [TEMPLATE as i32, 1000, 0, 0],
    );

    assert!(
        world.all_doodads().is_empty(),
        "the summon is not spawned before the delay"
    );
    assert!(
        server.systems.npcs.area_triggers.triggers().is_empty(),
        "the start phase runs at the spawn, not at the cast"
    );
    // Area ticks during the wait must not create or remove anything.
    for _ in 0..5 {
        server.systems.npcs.area_triggers.tick(&server);
    }
    assert!(server.systems.npcs.area_triggers.triggers().is_empty());
    assert!(!ally.data().unit.buffs.check_buff(BUFF));

    // Drive the delay (doodad task queue in the port, tokio timer of the old path too).
    tokio::task::yield_now().await;
    server.systems.npcs.doodads.tasks.advance(&server, 1000);
    tokio::time::advance(Duration::from_millis(1000)).await;
    tokio::task::yield_now().await;
    let summon = world.all_doodads().pop().expect("the spawned summon");
    assert!(summon.is_visible());
    assert_eq!(
        server
            .systems
            .npcs
            .area_triggers
            .attached_to(summon.obj_id)
            .len(),
        1,
        "the clout exists once the summon spawned"
    );
    server.systems.npcs.area_triggers.tick(&server);
    assert!(ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 1);

    // The 500 ms clout ends: the trigger and its buff go with it.
    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 500), 1);
    assert!(!ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 0);
    assert!(summon.data().phase.deleted);
    assert!(world.all_doodads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn delayed_spawn_doodads_cast_back_to_back_get_their_own_clouts() {
    // The first cast's clout timer starts at its spawn (see above), so a second cast one second
    // later does not inherit a stale trigger or an expired one.
    use std::time::Duration;

    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};

    const BUFF: u32 = 631;
    let server = Builder::default()
        .template(TEMPLATE, &[(10, 1)])
        .phase_func(10, clout(BUFF, 1, 1500))
        .clout_server(BUFF, comet_shape());
    crate::systems::npcs::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let caster = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    let mut ally_client = TestClient::new(&server, 2, 1002);
    let mut ally_character = test_character(2, "Beta", 100.0, 103.0);
    ally_character.unit.transform.set_xyz(100.0, 103.0, 0.0);
    let ally = spawn_player(&server, &ally_client, ally_character, 0x820);
    client.drain();
    ally_client.drain();
    let world = server.world.world(0).unwrap();

    let cast = || EffectArgs {
        caster: Some(WorldObject::from(&caster)),
        caster_caster: SkillCaster::Unit(caster.obj_id),
        target: Target::Obj(WorldObject::from(&caster)),
        target_caster: SkillCastTarget::Unit(caster.obj_id),
        cast: CastAction::Skill {
            skill_id: 18_222,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    };
    let spawn_after_delay = async || {
        tokio::task::yield_now().await;
        server.systems.npcs.doodads.tasks.advance(&server, 1000);
        tokio::time::advance(Duration::from_millis(1000)).await;
        tokio::task::yield_now().await;
    };
    crate::systems::skills::effects::special::execute(
        &server,
        15,
        &cast(),
        [TEMPLATE as i32, 1000, 0, 0],
    );
    spawn_after_delay().await;
    server.systems.npcs.area_triggers.tick(&server);
    assert!(ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 1);

    crate::systems::skills::effects::special::execute(
        &server,
        15,
        &cast(),
        [TEMPLATE as i32, 1000, 0, 0],
    );
    assert_eq!(world.all_doodads().len(), 1, "the second summon waits");
    spawn_after_delay().await;
    assert_eq!(world.all_doodads().len(), 2);
    server.systems.npcs.area_triggers.tick(&server);
    assert_eq!(server.systems.npcs.area_triggers.triggers().len(), 2);
    assert!(ally.data().unit.buffs.check_buff(BUFF));
    assert_eq!(ally.data().unit.trigger_count(BUFF), 2);
}

/// A caster and an ally for a delayed `SpawnDoodad` summon of `TEMPLATE` whose start phase is a
/// `clout_ms` clout. The synthetic world has no geodata: `SpawnDoodad` puts the doodad on the
/// fallback ground height 0, so the ally stands on it (the caster stays at 100, out of the area).
struct DelayedSummon {
    server: Arc<GameServer>,
    world: Arc<WorldInstance>,
    caster: PlayerRef,
    ally: PlayerRef,
    ally_client: TestClient,
    _caster_client: TestClient,
}

impl DelayedSummon {
    fn new(buff: u32, clout_ms: i32) -> Self {
        let server = Builder::default()
            .template(TEMPLATE, &[(10, 1)])
            .phase_func(10, clout(buff, 1, clout_ms))
            .clout_server(buff, comet_shape());
        crate::systems::npcs::bridges::install(&server);
        let mut caster_client = TestClient::new(&server, 1, 1001);
        let caster = spawn_player(
            &server,
            &caster_client,
            test_character(1, "Alpha", 100.0, 100.0),
            0x810,
        );
        let mut ally_client = TestClient::new(&server, 2, 1002);
        let mut ally_character = test_character(2, "Beta", 100.0, 103.0);
        ally_character.unit.transform.set_xyz(100.0, 103.0, 0.0);
        let ally = spawn_player(&server, &ally_client, ally_character, 0x820);
        caster_client.drain();
        ally_client.drain();
        let world = server.world.world(0).unwrap();
        Self {
            server,
            world,
            caster,
            ally,
            ally_client,
            _caster_client: caster_client,
        }
    }

    /// The `SpawnDoodad` special effect (type 15) of Comet's Boon: `TEMPLATE` after 1000 ms.
    fn cast(&self) {
        use crate::systems::skills::effects::{EffectArgs, EffectSource};
        use crate::systems::skills::geometry::Target;
        use crate::systems::skills::types::{
            CastAction, SkillCastTarget, SkillCaster, SkillObject,
        };

        let args = EffectArgs {
            caster: Some(WorldObject::from(&self.caster)),
            caster_caster: SkillCaster::Unit(self.caster.obj_id),
            target: Target::Obj(WorldObject::from(&self.caster)),
            target_caster: SkillCastTarget::Unit(self.caster.obj_id),
            cast: CastAction::Skill {
                skill_id: 18_222,
                tl_id: 1,
            },
            source: EffectSource::default(),
            skill_object: &SkillObject::None,
        };
        crate::systems::skills::effects::special::execute(
            &self.server,
            15,
            &args,
            [TEMPLATE as i32, 1000, 0, 0],
        );
    }
}

#[tokio::test]
async fn delayed_spawn_doodad_clout_stays_the_casters_when_the_caster_leaves_during_the_delay() {
    // The C# ran `InitDoodad()` at the cast, with the caster in the world: `DoodadFuncClout.Use`
    // took `areaTrigger.Caster = caster is Character ? caster : owner`. The port initialises
    // after the delay; a caster who logged out meanwhile must not turn the clout into the
    // doodad's own (its relation would be judged against a doodad, not the caster).
    const BUFF: u32 = 631;
    let f = DelayedSummon::new(BUFF, 500);
    f.cast();
    f.world.hide(&f.caster);
    f.world.remove_object(f.caster.obj_id);
    f.server.world.try_remove_character(f.caster.obj_id);
    assert!(f.world.unit(f.caster.obj_id).is_none());

    assert_eq!(
        f.server.systems.npcs.doodads.tasks.advance(&f.server, 1000),
        1
    );
    let summon = f.world.all_doodads().pop().expect("the spawned summon");
    let triggers = f
        .server
        .systems
        .npcs
        .area_triggers
        .attached_to(summon.obj_id);
    assert_eq!(triggers.len(), 1);
    assert_eq!(
        triggers[0].caster.as_ref().map(WorldObject::obj_id),
        Some(f.caster.obj_id),
        "the clout belongs to the unit that cast the summon, not to the summon"
    );
    f.server.systems.npcs.area_triggers.tick(&f.server);
    assert!(f.ally.data().unit.buffs.check_buff(BUFF));
}

#[tokio::test]
async fn delayed_spawn_doodad_is_initialised_before_it_is_visible() {
    // `SpawnDoodad.cs`: `InitDoodad()` runs on the invisible doodad, then `Spawn()`: a player who
    // sees the summon appear gets its `SCDoodadCreated`, not the phase change of its init.
    const BUFF: u32 = 631;
    let mut f = DelayedSummon::new(BUFF, 500);
    f.cast();
    assert_eq!(
        f.server.systems.npcs.doodads.tasks.advance(&f.server, 1000),
        1
    );
    let ops = f.ally_client.drain_ops();
    assert!(
        ops.contains(&sc::SCDoodadCreatedPacket),
        "the ally sees the summon appear"
    );
    assert!(
        !ops.contains(&sc::SCDoodadPhaseChangedPacket),
        "the start phase ran while the doodad was still invisible"
    );
    // The clout started with the visible summon all the same.
    f.server.systems.npcs.area_triggers.tick(&f.server);
    assert!(f.ally.data().unit.buffs.check_buff(BUFF));
}

#[tokio::test]
async fn delayed_spawn_that_can_no_longer_run_releases_the_doodad_object_id() {
    // `SpawnDoodad` allocates the summon's object id when it is cast. When its instance is gone
    // by the time the delay is over the spawn is dropped, and nothing else owns the id.
    use std::time::Duration;

    let server = Builder::default().template(TEMPLATE, &[(10, 1)]).server();
    let template = server.world.world(0).unwrap().template.clone();
    let world = server.world.create_world_instance(template, 77);
    let doodad = manager::create(&server, &world, 0, TEMPLATE, None, true).unwrap();
    let obj_id = doodad.obj_id;
    tasks::schedule_spawn(&server, &doodad, None, Duration::from_millis(1000));
    drop(world);
    assert!(server.world.remove_world(77));

    assert_eq!(server.systems.npcs.doodads.tasks.advance(&server, 1000), 1);
    assert_eq!(
        server.ids.object.get_next_id().unwrap(),
        obj_id,
        "the dropped spawn gave its object id back"
    );
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
async fn fish_finder_shows_only_rendered_phases_through_school_respawns() {
    const SETUP_MS: i32 = 1000;
    const ACTIVE_MS: i32 = 1_800_000;
    const RESPAWN_MS: i32 = 1_800_000;
    for (template, setup, shown, expired, prefab) in [
        (
            FRESHWATER_FISH_SCHOOL,
            16995,
            17243,
            17140,
            "water_inactive",
        ),
        (FRESHWATER_FISH_SCHOOL, 16995, 17245, 17140, "water_active"),
        (SALTWATER_FISH_SCHOOL, 16997, 17231, 17079, "ocean_inactive"),
        (SALTWATER_FISH_SCHOOL, 16997, 17232, 17079, "ocean_active"),
    ] {
        let mut b = Builder::default()
            .template(template, &[(setup, 1), (shown, 2), (expired, 2)])
            .phase_func(setup, timer(3, SETUP_MS, shown as i32))
            .phase_func(shown, timer(1, ACTIVE_MS, expired as i32))
            .phase_func(
                expired,
                FuncTemplate::Final(DoodadFuncFinal {
                    id: 2,
                    respawn: false,
                    min_time: RESPAWN_MS,
                    max_time: RESPAWN_MS,
                    ..Default::default()
                }),
            );
        let visible_model = format!("prefab://prefabs/fish.xml/fish_school.{prefab}");
        let hidden_model = "prefab://prefabs/interaction.xml/env.invisible";
        for (group_id, model) in [(setup, hidden_model), (shown, &visible_model)] {
            let index = b.data.all_func_groups_index[&group_id];
            b.data.all_func_groups[index].model = Some(model.to_owned());
        }
        let mut f = fx(b);
        let radar = &f.server.systems.services.radar;
        radar.register_for_fish_school(&f.player, 1000.0);
        let spawner = new_spawner(template, 123);
        let first = spawner.spawn(&f.server, 0).unwrap();

        assert_eq!(group(&first), setup);
        assert!(first.core().is_visible());
        radar.radar_tick(&f.server).await;
        assert_school_snapshot(&mut f.client, None);
        assert_eq!(f.advance(SETUP_MS as u64), 1);
        assert_eq!(group(&first), shown);
        radar.radar_tick(&f.server).await;
        assert_school_snapshot(&mut f.client, Some(&first));
        assert_eq!(f.advance(ACTIVE_MS as u64), 1);
        assert!(first.data().phase.deleted);
        assert!(f.world.doodad(first.obj_id).is_none());
        assert!(f
            .server
            .systems
            .npcs
            .fish_schools
            .fish_schools_of(0)
            .is_empty());
        radar.radar_tick(&f.server).await;
        assert_school_snapshot(&mut f.client, None);

        assert_eq!(f.advance(RESPAWN_MS as u64 - 1), 0);
        assert!(spawner.last().is_none());
        assert_eq!(f.advance(1), 1);
        let replacement = spawner.last().unwrap();
        assert!(!Arc::ptr_eq(&first, &replacement));
        assert_eq!(group(&replacement), setup);
        assert_eq!(first.data().transform.x, replacement.data().transform.x);
        radar.radar_tick(&f.server).await;
        assert_school_snapshot(&mut f.client, None);
        assert_eq!(f.advance(SETUP_MS as u64), 1);
        radar.radar_tick(&f.server).await;
        assert_school_snapshot(&mut f.client, Some(&replacement));

        assert_eq!(f.advance(ACTIVE_MS as u64), 1);
        assert!(replacement.data().phase.deleted);
    }
}

fn assert_school_snapshot(client: &mut TestClient, school: Option<&DoodadRef>) {
    let packets: Vec<_> = client
        .drain()
        .into_iter()
        .filter(|packet| packet.0 == sc::SCSchoolOfFishDoodadsPacket)
        .map(|packet| packet.1)
        .collect();
    assert_eq!(packets.len(), 1);
    let packet = &packets[0];
    if let Some(school) = school {
        assert_eq!(&packet[..2], [1, 1]);
        assert_eq!(&packet[2..5], &school.obj_id.to_le_bytes()[..3]);
        assert_eq!(&packet[5..9], &school.data().template_id.to_le_bytes());
    } else {
        assert_eq!(packet, &[1, 0]);
    }
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
    do_change_phase(&ctx, None, 10);
    assert_eq!(group(&d), 10);
    // Entering the phase fires nothing with a character caster either: only a received signal
    // or the pulse of the trigger's own group is a pulse (the previous rule fired on every
    // character-caused entry and bounced toggles, see the `toggle` / Sal Temple tests).
    do_change_phase(&ctx, Some(&caster), 10);
    assert_eq!(
        group(&d),
        10,
        "a character-caused phase entry is not a pulse"
    );
    super::super::logic::receive(&ctx, &d, Some(&caster), true, false);
    assert_eq!(group(&d), 11);
    f.use_(&d); // 11 -> 10: the entry does not fire the row again, stays in 10
    assert_eq!(group(&d), 10);
    let other = manager::create(&f.server, &f.world, 0x501, TEMPLATE, None, true).unwrap();
    let other_ctx = DoodadCtx::new(&f.server, f.world.clone(), other.clone());
    do_change_phase(&other_ctx, Some(&caster), 10);
    assert_eq!(group(&other), 10);
    // A received signal only fires on the authored flag value.
    super::super::logic::receive(&ctx, &other, Some(&caster), false, false);
    assert_eq!(group(&other), 10);
    super::super::logic::receive(&ctx, &other, Some(&caster), true, false);
    assert_eq!(group(&other), 11);
}

/// A `DoodadFuncPulseTrigger` fires on a received signal or on the pulse its own group sent in
/// this phase entry, never because a player caster travelled along a signal chain. Toggle shape
/// (63 template pairs, e.g. Sal Temple's Ice Trap 4751): `A: PulseTrigger(true -> B)`,
/// `B: PulseTrigger(false -> A)`. The signals true, false, true (all carrying the player caster,
/// as every signal of a player-started chain does) must leave it in B, A, B.
#[tokio::test]
async fn a_toggle_pair_follows_its_signals_when_the_chain_carries_a_player_caster() {
    let b = Builder::default()
        .template(100, &[(10, 1)])
        .template(101, &[(20, 1), (21, 2)])
        .phase_func(
            20,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 1,
                flag: true,
                next_phase: 21,
            }),
        )
        .phase_func(
            21,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 2,
                flag: false,
                next_phase: 20,
            }),
        );
    let f = fx(b);
    let source = place(&f, 100, 101.0);
    let toggle = place(&f, 101, 102.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), source);
    let caster = f.caster();
    assert_eq!(group(&toggle), 20);
    logic::receive(&ctx, &toggle, Some(&caster), true, true);
    assert_eq!(group(&toggle), 21, "the true signal switches it on");
    logic::receive(&ctx, &toggle, Some(&caster), false, true);
    assert_eq!(
        group(&toggle),
        20,
        "the false signal switches it off, for good"
    );
    logic::receive(&ctx, &toggle, Some(&caster), true, true);
    assert_eq!(group(&toggle), 21);
}

/// Jolly Roger Tower 3382 group 7885 shape: `Pulse(false)` + `PulseTrigger(true -> X)`. The
/// group's own pulse is false and the trigger waits for true, so a character-caused entry must
/// not move the doodad on.
#[tokio::test]
async fn a_false_pulse_does_not_fire_the_true_trigger_of_its_own_group() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2), (12, 2)])
        .fake_use(10, 1, 11)
        .phase_func(
            11,
            FuncTemplate::Pulse(DoodadFuncPulse { id: 5, flag: false }),
        )
        .phase_func(
            11,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 6,
                flag: true,
                next_phase: 12,
            }),
        );
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.use_(&d);
    assert_eq!(
        group(&d),
        11,
        "a false pulse is not the true trigger's signal"
    );
}

/// Sharpwind switch 13161 shape, rows in the shipped `actual_func_id` order (trigger 274 before
/// pulse 287): a character-caused entry pulses true and the group's own true trigger then takes
/// the doodad to its final phase. Entered without a caster (a world spawn) nothing pulses, so
/// the trigger of the same group stays armed.
#[tokio::test]
async fn a_true_pulse_fires_the_true_trigger_of_its_own_group() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2), (12, 2)])
        .fake_use(10, 1, 11)
        .phase_func(
            11,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 6,
                flag: true,
                next_phase: 12,
            }),
        )
        .phase_func(
            11,
            FuncTemplate::Pulse(DoodadFuncPulse { id: 5, flag: true }),
        );
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.use_(&d);
    assert_eq!(group(&d), 12, "the use pulses true and its trigger fires");

    let spawned = place(&f, TEMPLATE, 130.0);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), spawned.clone());
    do_change_phase(&ctx, None, 11);
    assert_eq!(group(&spawned), 11, "no caster, no pulse, no fire");
}

/// Sal Temple Palace Cellar with the shipped group shapes: using the Ice Machine 4752
/// (`RelatedIds [4751]`) starts timers of 2.7 s / 4.3 s / 2.7 s that all carry the player caster,
/// `Pulse(true)` in 11800 and `Pulse(false)` back in the start group 11798. The Ice Trap 4751
/// (`11795: PulseTrigger(true -> 11796)`, `11796: PulseTrigger(false -> 11795)`) goes on at the
/// true pulse and has to stay off after the false one: its entry into 11795 by that signal is not
/// a pulse (the previous rule fired it again and switched the ice field on for good).
#[tokio::test]
async fn sal_temple_ice_trap_stays_off_after_the_ice_machines_false_pulse() {
    let b = Builder::default()
        .template(4752, &[(11798, 1), (11799, 2), (11800, 2), (11801, 2)])
        .fake_use(11798, 2335, 11799)
        .phase_func(
            11798,
            FuncTemplate::Pulse(DoodadFuncPulse {
                id: 259,
                flag: false,
            }),
        )
        .phase_func(
            11798,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 419,
                family_id: 30_014,
            }),
        )
        .phase_func(11799, timer(4565, 2_700, 11800))
        .phase_func(
            11800,
            FuncTemplate::Pulse(DoodadFuncPulse {
                id: 260,
                flag: true,
            }),
        )
        .phase_func(
            11800,
            FuncTemplate::LogicFamilyProvider(DoodadFuncLogicFamilyProvider {
                id: 420,
                family_id: 30_014,
            }),
        )
        .phase_func(11800, timer(4566, 4_300, 11801))
        .phase_func(11801, timer(4567, 2_700, 11798))
        .template(4751, &[(11795, 1), (11796, 2)])
        .phase_func(
            11795,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 252,
                flag: true,
                next_phase: 11796,
            }),
        )
        .phase_func(
            11796,
            FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                id: 253,
                flag: false,
                next_phase: 11795,
            }),
        );
    let mut f = fx(b);
    let trap = f.doodad(4751);
    let machine = new_spawner_with(4752, 700, Some(vec![4751]), 0)
        .spawn(&f.server, 0)
        .expect("the Ice Machine spawns");
    assert_eq!((group(&machine), group(&trap)), (11798, 11795));

    f.use_(&machine);
    assert_eq!(group(&machine), 11799);
    f.advance(2_700);
    assert_eq!(group(&machine), 11800);
    assert_eq!(group(&trap), 11796, "the true pulse switches the trap on");
    f.advance(4_300);
    assert_eq!(group(&machine), 11801);
    assert_eq!(group(&trap), 11796);
    f.advance(2_700);
    assert_eq!(group(&machine), 11798);
    assert_eq!(
        group(&trap),
        11795,
        "the false pulse switches the trap off and it stays off"
    );
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
    let mut f = fx(Builder {
        data,
        ..Default::default()
    });
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

#[tokio::test]
async fn npc_encounter_skill_reaches_every_matching_doodad_in_its_area() {
    use crate::models::npc::Npc;
    use crate::systems::skills::data::SkillTemplate;
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::skill::{apply_effects, CastCtx};
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillTargetSelection};
    use crate::world::Entity;

    let mut data = GameData::empty();
    data.systems.npcs.doodad = Builder::default()
        .template(TEMPLATE, &[(1, 1), (2, 0)])
        .template(TEMPLATE + 1, &[(3, 1), (4, 0)])
        .func(1, "DoodadFuncSkillHit", 2, 0)
        .func(3, "DoodadFuncSkillHit", 4, 0)
        .data;
    data.systems.npcs.doodad.func_templates.insert(
        "DoodadFuncSkillHit",
        [FAKE_SKILL, FAKE_SKILL + 1]
            .into_iter()
            .enumerate()
            .map(|(index, skill_id)| {
                let id = index as u32 + 1;
                (
                    id,
                    FuncTemplate::SkillHit(DoodadFuncSkillHit { id, skill_id }),
                )
            })
            .collect(),
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::npcs::install_hooks(&server);
    let world = server.world.world(0).unwrap();
    let mut npc = Npc::new(123);
    npc.unit.transform.set_xyz(100.0, 100.0, 40.0);
    npc.unit.hp = 100;
    let npc = Entity::new(0x500, npc);
    world.spawn(&npc, &server.view_ctx());

    let place = |template, x| {
        let doodad = manager::create(&server, &world, 0, template, None, true).unwrap();
        doodad.data().transform.set_xyz(x, 100.0, 40.0);
        manager::spawn(&server, &world, &doodad);
        manager::init_doodad(&server, &world, &doodad);
        doodad
    };
    let unrelated = place(TEMPLATE + 1, 101.0);
    let near = place(TEMPLATE, 110.0);
    let far = place(TEMPLATE, 120.0);
    let outside = place(TEMPLATE, 150.0);
    let caster = WorldObject::from(&npc);
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: FAKE_SKILL,
            target_selection: SkillTargetSelection::Source as i32,
            target_area_radius: 30,
            target_area_count: 1,
            ..Default::default()
        }),
        1,
    );
    let cast = CastCtx {
        target: Target::Obj(caster.clone()),
        caster,
        caster_caster: SkillCaster::Unit(npc.obj_id),
        target_caster: SkillCastTarget::Unit(npc.obj_id),
        skill_object: Default::default(),
    };
    // The area count (1) does not cut the props that answer the signal (PORTING.md, Howling
    // Abyss follow-up): both doodads inside the radius change phase on the first cast.
    apply_effects(&server, &skill, &cast);
    assert_eq!((group(&near), group(&far), group(&outside)), (2, 2, 1));
    assert_eq!(group(&unrelated), 3);
}

/// Howling Abyss 2 "Strange Device" 6345 -> "Iron Gate" 6346 with the real compact data: the
/// spawn rows carry `RelatedIds [6346]` and no `FamilyId`, the gate's start group 16787 holds a
/// `DoodadFuncLogicFamilySubscriber` (30200) and a `DoodadFuncPulseTrigger` (true -> 16788), and a
/// character using the device (skill 21394 -> group 16786, `DoodadFuncPulse` true) must open it.
#[tokio::test]
#[ignore]
async fn real_compact_db_related_ids_signal_opens_the_howling_abyss_2_gate() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    let f = fx(Builder {
        data,
        ..Default::default()
    });
    let gate = new_spawner(6346, 700)
        .spawn(&f.server, 0)
        .expect("spawns the gate");
    let device = new_spawner_with(6345, 701, Some(vec![6346]), 0)
        .spawn(&f.server, 0)
        .expect("spawns the device");
    assert_eq!((group(&device), group(&gate)), (16785, 16787));
    manager::use_doodad(&f.server, &f.world, &device, Some(&f.caster()), 21394, 0);
    assert_eq!(group(&device), 16786);
    assert_eq!(group(&gate), 16788, "the used device opens the gate");
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
    let f = fx(Builder {
        data,
        ..Default::default()
    });
    // One roll per ratio phase, partitioned across sibling rows in `actual_func_id` order. The
    // first 2768: 7026 rolls 9600 (misses 186 `[0, 7000)` and 187 `[7000, 9500)`, hits 188
    // `[9500, 10000)`) -> 7055, where 100 hits 383 `[0, 2000)` (self-respawn -> 2768). The
    // replacement 2768 does the same in 7026, then its 7055 rolls 6500 (misses 383/384/385
    // `[0, 6000)` and hits 387 `[6000, 8000)` -> 3225), which the guard used to reject as a
    // repeated (2768, 7055) source.
    f.server.systems.npcs.doodads.queue_phase_ratios(&[
        9600, 100, // first 2768: 7026 -> 7055, 383 -> 2768
        9600, 6500, // replacement: 7026 -> 7055, 387 -> 3225
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
    let f = fx(Builder {
        data,
        ..Default::default()
    });
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

// ---- `CSChangeDoodadPhasePacket` (client-acknowledged Bubble / OpenPaper phase) ----

fn phase_packet_body(obj_id: u32, skill: u32, next_phase: u32, func_id: u32) -> Vec<u8> {
    let mut s = aaemu_commons::PacketStream::new();
    s.write_bc(obj_id)
        .write_u32(skill)
        .write_u32(next_phase)
        .write_u32(func_id);
    s.into_bytes()
}

fn run_phase_packet(f: &mut Fx, body: &[u8]) {
    use crate::net::registry::ClientPacket;
    let mut s = aaemu_commons::PacketStream::from_bytes(body.to_vec());
    let p = crate::systems::npcs::doodad::packets::CSChangeDoodadPhasePacket::read(&mut s)
        .expect("read");
    futures::executor::block_on(p.handle(&f.server, &mut f.client.conn)).expect("handle");
}

#[tokio::test]
async fn change_doodad_phase_moves_a_bubble_of_the_current_group() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 1)])
        .func(10, "DoodadFuncBubble", 20, 0);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    d.data().transform.set_xyz(100.0, 100.0, 100.0);
    f.client.drain();
    assert_eq!(group(&d), 10);

    // Func key 1 is the Bubble row.
    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 20, 1));
    assert_eq!(group(&d), 20);
    assert_eq!(f.phase_packets(), vec![20]);

    // A replay of the same func no longer belongs to the (new) current group.
    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 20, 1));
    assert_eq!(group(&d), 20);
    assert!(f.phase_packets().is_empty());
}

#[tokio::test]
async fn change_doodad_phase_refuses_another_group_type_or_target_phase() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 1), (30, 2)])
        .func(10, "DoodadFuncBubble", 20, 0)
        .func(10, "DoodadFuncUse", 30, 0)
        .func(30, "DoodadFuncOpenPaper", 20, 0);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    d.data().transform.set_xyz(100.0, 100.0, 100.0);
    f.client.drain();

    // Func 2 is a Use, not a client-side Bubble / OpenPaper.
    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 30, 2));
    // Func 3 is not in the current group.
    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 20, 3));
    // Wrong target phase for func 1.
    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 30, 1));
    assert_eq!(group(&d), 10);
    assert!(f.phase_packets().is_empty());
}

#[tokio::test]
async fn change_doodad_phase_refuses_out_of_reach() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 1)])
        .func(10, "DoodadFuncBubble", 20, 7)
        .skill(7, 4);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    // Player is at (100, 100); 30 m is beyond the 4 m skill + the 15 m doodad allowance.
    d.data().transform.set_xyz(130.0, 100.0, 100.0);
    f.client.drain();

    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 7, 20, 1));
    assert_eq!(group(&d), 10);
    assert!(f.phase_packets().is_empty());
}

#[tokio::test]
async fn change_doodad_phase_never_uses_the_client_skill_for_range() {
    // The func has no skill (structurally the same as Bubble funcs 6279, 9407, 9965); the
    // packet's forged skill claims a 1000 m range.
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 1)])
        .func(10, "DoodadFuncBubble", 20, 0)
        .skill(FAKE_SKILL, 1000);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    // 30 m: beyond the 5 m doodad interaction distance + the 15 m allowance.
    d.data().transform.set_xyz(130.0, 100.0, 100.0);
    f.client.drain();

    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, FAKE_SKILL, 20, 1));
    assert_eq!(group(&d), 10);
    assert!(f.phase_packets().is_empty());

    // 18 m is inside the fixed bound; the forged skill does not widen it.
    d.data().transform.set_xyz(118.0, 100.0, 100.0);
    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, FAKE_SKILL, 20, 1));
    assert_eq!(group(&d), 20);
}

#[tokio::test]
async fn change_doodad_phase_refuses_a_doodad_scheduled_for_despawn() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 1)])
        .func(10, "DoodadFuncBubble", 20, 0);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    d.data().transform.set_xyz(100.0, 100.0, 100.0);
    f.client.drain();
    f.server
        .systems
        .npcs
        .spawns
        .set_despawn_field(d.obj_id, tokio::time::Instant::now());

    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 20, 1));
    assert_eq!(group(&d), 10);
    assert!(f.phase_packets().is_empty());
}

#[tokio::test]
async fn change_doodad_phase_refuses_a_dead_caster() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (20, 1)])
        .func(10, "DoodadFuncBubble", 20, 0);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    d.data().transform.set_xyz(100.0, 100.0, 100.0);
    f.client.drain();
    f.player.data().unit.hp = 0;

    run_phase_packet(&mut f, &phase_packet_body(d.obj_id, 0, 20, 1));
    assert_eq!(group(&d), 10);
    assert!(f.phase_packets().is_empty());
}

// ---- `SCDoodadPhaseMsg` (WP-UI1) ----

#[tokio::test]
async fn phase_msg_is_sent_to_viewers_only() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, 11)
        .phase_msg(11, "The furnace is overheated!", false);
    let mut f = fx(b);
    // Same zone, out of the 5x5 sector neighbourhood of the doodad.
    let mut far_client = TestClient::new(&f.server, 2, 1002);
    let _far = spawn_player(
        &f.server,
        &far_client,
        test_character(2, "Beta", 900.0, 900.0),
        0x811,
    );
    let d = f.doodad(TEMPLATE);
    f.client.drain();
    far_client.drain();

    f.use_(&d);
    assert_eq!(f.phase_msgs(), vec![(d.obj_id, 11)]);
    assert!(far_client
        .drain()
        .iter()
        .all(|p| p.0 != sc::SCDoodadPhaseMsgPacket));
}

#[tokio::test]
async fn phase_group_without_a_message_sends_nothing() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .fake_use(10, 1, 11);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.client.drain();

    f.use_(&d);
    assert!(f.phase_msgs().is_empty());
}

#[tokio::test]
async fn an_unchanged_func_group_sends_no_phase_msg() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_msg(10, "start", false)
        .phase_msg(11, "normal", false);
    let mut f = fx(b);
    let d = f.doodad(TEMPLATE);
    f.client.drain();

    manager::set_func_group_id(&f.server, &d, 11);
    assert_eq!(f.phase_msgs(), vec![(d.obj_id, 11)]);
    manager::set_func_group_id(&f.server, &d, 11);
    assert!(f.phase_msgs().is_empty());
}

#[tokio::test]
async fn spawn_time_initial_group_sends_no_phase_msg() {
    // The saved-state / spawner phase is assigned before the doodad is spawned; even a
    // zone-wide message of that group must not announce a transition nobody saw.
    const OTHER: u32 = 12;
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2), (OTHER, 2)])
        .phase_msg(11, "saved phase", true)
        .phase_msg(OTHER, "next phase", true);
    let mut f = fx(b);
    let d = manager::create(&f.server, &f.world, 0x500, TEMPLATE, None, true).unwrap();
    d.data().transform.set_xyz(120.0, 130.0, 100.0);
    f.client.drain();

    manager::set_func_group_id(&f.server, &d, 11);
    assert!(f.phase_msgs().is_empty(), "not spawned yet");

    manager::spawn(&f.server, &f.world, &d);
    manager::init_doodad(&f.server, &f.world, &d);
    f.client.drain();
    manager::set_func_group_id(&f.server, &d, OTHER);
    assert_eq!(f.phase_msgs(), vec![(d.obj_id, OTHER)]);
}

fn two_zone_world() -> WorldTemplate {
    const OTHER_ZONE: u32 = 200;
    let mut t = WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE);
    t.xml_world.zones.insert(
        OTHER_ZONE,
        XmlWorldZone {
            zone_key: OTHER_ZONE,
            name: "other".into(),
            origin_x: 0,
            origin_y: 0,
            cells: Default::default(),
            spawn_position: Default::default(),
        },
    );
    t.zone_keys.push(OTHER_ZONE);
    for x in 16..32 {
        for y in 0..32 {
            assert!(t.set_zone_key_at(x, y, OTHER_ZONE));
        }
    }
    t
}

#[tokio::test]
async fn zone_wide_phase_msg_reaches_the_whole_zone_only() {
    let b = Builder::default()
        .template(TEMPLATE, &[(10, 1), (11, 2)])
        .phase_msg(11, "The school of fish appears.", true);
    let server = b.server_with_world(two_zone_world());
    let mut near = TestClient::new(&server, 1, 1001);
    let _near = spawn_player(
        &server,
        &near,
        test_character(1, "Alpha", 100.0, 100.0),
        0x810,
    );
    let mut same_zone_far = TestClient::new(&server, 2, 1002);
    let same_far = spawn_player(
        &server,
        &same_zone_far,
        test_character(2, "Beta", 900.0, 900.0),
        0x811,
    );
    let mut other_zone = TestClient::new(&server, 3, 1003);
    let other = spawn_player(
        &server,
        &other_zone,
        test_character(3, "Gamma", 1200.0, 900.0),
        0x812,
    );
    assert_eq!(same_far.data().unit.transform.zone_id, TEST_ZONE);
    assert_eq!(other.data().unit.transform.zone_id, 200);
    let world = server.world.world(0).unwrap();
    let d = manager::create(&server, &world, 0x500, TEMPLATE, None, true).unwrap();
    d.data().transform.set_xyz(120.0, 130.0, 100.0);
    manager::spawn(&server, &world, &d);
    manager::init_doodad(&server, &world, &d);
    near.drain();
    same_zone_far.drain();
    other_zone.drain();

    manager::set_func_group_id(&server, &d, 11);
    let msgs = |c: &mut TestClient| -> Vec<(u32, u32)> {
        c.drain()
            .into_iter()
            .filter(|p| p.0 == sc::SCDoodadPhaseMsgPacket)
            .map(|p| {
                (
                    u32::from_le_bytes([p.1[0], p.1[1], p.1[2], 0]),
                    u32::from_le_bytes(p.1[3..7].try_into().unwrap()),
                )
            })
            .collect()
    };
    assert_eq!(msgs(&mut near), vec![(d.obj_id, 11)]);
    assert_eq!(msgs(&mut same_zone_far), vec![(d.obj_id, 11)]);
    assert!(msgs(&mut other_zone).is_empty());
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

/// The shipped `compact.sqlite3` rows the Sharpwind chain depends on: the switch's rows in the
/// loader's `actual_func_id` order (PulseTrigger 274, Pulse 287, provider 444; the executor runs
/// the trigger last, see `phase::do_phase_funcs`), the gate family wiring, the lake's 5 s + 22 s
/// water-volume sequence and the water volume's 22 m / 22 s.
#[tokio::test]
#[ignore]
async fn real_compact_db_sharpwind_water_network_rows() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    for id in [5063u32, 5064, 5068, 5069, 5070, 5071, 5072, 5073] {
        assert!(data.template(id).is_some(), "missing template {id}");
    }

    // Switch 5064: start group 13160 (false pulse + provider), used group 13161 in the loader's
    // `actual_func_id` order (PulseTrigger 274 -> final 13163, Pulse 287, provider 444 -> 50005).
    let start: Vec<(u32, &str)> = data
        .phase_funcs(13160)
        .iter()
        .map(|f| (f.func_id, f.func_type.as_str()))
        .collect();
    assert_eq!(
        start,
        [
            (288, "DoodadFuncPulse"),
            (445, "DoodadFuncLogicFamilyProvider")
        ]
    );
    let used: Vec<(u32, &str)> = data
        .phase_funcs(13161)
        .iter()
        .map(|f| (f.func_id, f.func_type.as_str()))
        .collect();
    assert_eq!(
        used,
        [
            (274, "DoodadFuncPulseTrigger"),
            (287, "DoodadFuncPulse"),
            (444, "DoodadFuncLogicFamilyProvider"),
        ],
        "actual_func_id order: the executor defers the trigger until the pulse has run"
    );
    match data.phase_func_template(274, "DoodadFuncPulseTrigger") {
        Some(FuncTemplate::PulseTrigger(t)) => {
            assert!(t.flag);
            assert_eq!(t.next_phase, 13163);
        }
        other => panic!("{other:?}"),
    }

    // Gate 5070: AND (op 1, 1 s) with outputs 50006/50007 and inputs 50002/50003.
    let gate: Vec<(u32, &str)> = data
        .phase_funcs(13181)
        .iter()
        .map(|f| (f.func_id, f.func_type.as_str()))
        .collect();
    assert_eq!(
        gate,
        [
            (45, "DoodadFuncLogicFamilySubscriber"),
            (46, "DoodadFuncLogicFamilySubscriber"),
            (88, "DoodadFuncLogic"),
            (450, "DoodadFuncLogicFamilyProvider"),
            (451, "DoodadFuncLogicFamilyProvider"),
        ]
    );
    match data.phase_func_template(88, "DoodadFuncLogic") {
        Some(FuncTemplate::Logic(l)) => {
            assert_eq!((l.operation_id, l.delay_id), (1, 1));
        }
        other => panic!("{other:?}"),
    }
    let families = |ids: &[u32], ty: &str| -> Vec<u32> {
        ids.iter()
            .filter_map(|&id| match data.phase_func_template(id, ty) {
                Some(FuncTemplate::LogicFamilyProvider(p))
                    if ty == "DoodadFuncLogicFamilyProvider" =>
                {
                    Some(p.family_id)
                }
                Some(FuncTemplate::LogicFamilySubscriber(s))
                    if ty == "DoodadFuncLogicFamilySubscriber" =>
                {
                    Some(s.family_id)
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        families(&[450, 451], "DoodadFuncLogicFamilyProvider"),
        [50_006, 50_007]
    );
    assert_eq!(
        families(&[45, 46], "DoodadFuncLogicFamilySubscriber"),
        [50_002, 50_003]
    );
    assert_eq!(
        families(&[452, 453], "DoodadFuncLogicFamilyProvider"),
        [50_001, 50_008]
    );
    assert_eq!(
        families(&[47, 48], "DoodadFuncLogicFamilySubscriber"),
        [50_004, 50_005]
    );

    // Lake 5063: trigger -> 5 s timer -> water volume + 22 s timer -> raised water2.
    match data.phase_func_template(273, "DoodadFuncPulseTrigger") {
        Some(FuncTemplate::PulseTrigger(t)) => {
            assert!(t.flag);
            assert_eq!(t.next_phase, 14184);
        }
        other => panic!("{other:?}"),
    }
    match data.phase_func_template(5938, "DoodadFuncTimer") {
        Some(FuncTemplate::Timer(t)) => {
            assert_eq!((t.delay, t.next_phase), (5000, 13158));
        }
        other => panic!("{other:?}"),
    }
    match data.phase_func_template(15, "DoodadFuncWaterVolume") {
        Some(FuncTemplate::WaterVolume(w)) => {
            assert_eq!((w.level_change, w.duration), (22.0, 22.0));
        }
        other => panic!("{other:?}"),
    }
    match data.phase_func_template(5491, "DoodadFuncTimer") {
        Some(FuncTemplate::Timer(t)) => {
            assert_eq!((t.delay, t.next_phase), (22_000, 13159));
        }
        other => panic!("{other:?}"),
    }

    // Streams 5072/5073: level 1 -> level 2/3 on a true pulse.
    for (trigger, next) in [(278u32, 13184i32), (277, 13186)] {
        match data.phase_func_template(trigger, "DoodadFuncPulseTrigger") {
            Some(FuncTemplate::PulseTrigger(t)) => {
                assert!(t.flag);
                assert_eq!(t.next_phase, next);
            }
            other => panic!("{other:?}"),
        }
    }
}

/// The shipped Sharpwind water chain driven from the shipped data: the real compact DB and the
/// shipped `doodad_spawns.json` (22 rows, 11 of them the water network with their `FamilyId`s).
/// A character uses the three "Blocked Geyser" switches (skill 18321); the gates, the five
/// streams and the lake follow through the family network on the virtual clock, and the lake
/// reaches its raised phase 13159 (1 s gate delay, 5 s pre-roll, 22 s rise). Each switch's own
/// `Final` removes it 1 s after its use and must not undo its gate input.
#[tokio::test]
#[ignore]
async fn real_data_sharpwind_shipped_rows_raise_the_lake() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let spawn_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds/instance_cuttingwind_deadmine/doodad_spawns.json");
    let Ok(text) = std::fs::read_to_string(spawn_file) else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    let f = fx(Builder {
        data,
        ..Default::default()
    });
    let defs = parse_list(&text, DoodadSpawnerDef::from_json).expect("doodad_spawns.json");
    for (i, def) in defs.into_iter().enumerate() {
        let template = def.base.unit_id;
        assert!(
            Arc::new(DoodadSpawner::new(def, Some(0)))
                .spawn(&f.server, 0)
                .is_some(),
            "row {i} (template {template}) spawns"
        );
    }
    let of = |template: u32| -> Vec<DoodadRef> {
        let mut found: Vec<_> = f
            .world
            .all_doodads()
            .into_iter()
            .filter(|d| d.data().template_id == template)
            .collect();
        found.sort_by_key(|d| d.obj_id);
        found
    };
    let (switches, lake) = (
        [5064u32, 5068, 5069].map(|t| of(t).pop().expect("a switch is placed")),
        of(5063).pop().expect("the lake is placed"),
    );
    let (streams_a, streams_b) = (of(5072), of(5073));
    assert_eq!((streams_a.len(), streams_b.len()), (3, 2));
    assert_eq!(
        switches.each_ref().map(group),
        [13160, 13175, 13178],
        "the switches start in their start groups"
    );
    assert_eq!(group(&lake), 13157);

    let use_switch = |switch: &DoodadRef| {
        manager::use_doodad(&f.server, &f.world, switch, Some(&f.caster()), 18321, 0);
    };
    // Switches 5064 and 5068 feed gate 5070 (AND, 1 s): after the second one, the 5072 streams
    // run; gate 5071 still waits for switch 5069.
    use_switch(&switches[0]);
    assert_eq!(group(&switches[0]), 13163, "the used switch's final phase");
    f.advance(1_000);
    assert!(
        switches[0].data().phase.deleted,
        "the switch's Final removed it"
    );
    assert!(streams_a.iter().all(|s| group(s) == 13183));
    use_switch(&switches[1]);
    f.advance(1_000);
    assert!(
        streams_a.iter().all(|s| group(s) == 13184),
        "gate 5070 opens the 5072 streams although switch 5064 is gone: {:?}",
        streams_a.iter().map(group).collect::<Vec<_>>()
    );
    assert!(streams_b.iter().all(|s| group(s) == 13185));
    assert_eq!(group(&lake), 13157);

    // Switch 5069 completes gate 5071: the 5073 streams erupt and the lake starts its sequence.
    use_switch(&switches[2]);
    f.advance(1_000);
    assert!(streams_b.iter().all(|s| group(s) == 13186));
    assert_eq!(group(&lake), 14184);
    f.advance(5_000);
    assert_eq!(group(&lake), 13158, "the 5 s pre-roll ends");
    f.advance(22_000);
    assert_eq!(group(&lake), 13159, "the lake is raised");
}

/// Real compact.sqlite3 fish schools (saltwater 6448 and freshwater 6447): their species
/// selection phases (`16998` and `16996`) partition `0..10000` across every authored species
/// without falling through to the Bluefin Tuna / Carp fallback timers, and each species
/// activates `5767: School of Fish Area` and its species-specific `DoodadFuncFishSchool` spawner
/// when hit by its matching chum skill (and ignores mismatched chum).
#[tokio::test]
#[ignore = "requires AAEMU_COMPACT_DB"]
async fn real_compact_db_fish_schools_partition_all_species_and_activate_buff_with_matching_chum() {
    const SCHOOL_BUFF: u32 = 5767;
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let files = crate::data::client_files::ClientFileManager::default();
    let load_ctx = crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: std::path::Path::new("."),
        files: &files,
        language: "en_us",
    };
    let mut gd = GameData::empty();
    gd.systems.npcs.doodad = DoodadData::load_from(&conn).unwrap();
    gd.systems.npcs.area_trigger =
        crate::systems::npcs::area_trigger::AreaTriggerData::load(&load_ctx).unwrap();
    gd.systems
        .skills
        .add_buff(crate::systems::skills::data::BuffTemplate {
            id: SCHOOL_BUFF,
            kind: 1,
            stack_rule: 1,
            duration: 0,
            ..Default::default()
        });
    gd.skills.buffs.insert(
        SCHOOL_BUFF,
        crate::data::skills::BuffTemplate::simple(SCHOOL_BUFF, crate::data::skills::BuffKind::Good),
    );
    let server = Arc::new(test_server_with(gd));
    server.systems.npcs.doodads.tasks.set_manual(true);
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::bridges::install(&server);

    let world = server.world.world(0).unwrap();
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Angler", 100.0, 100.0),
        0x400,
    );
    let caster = WorldObject::from(&player);

    let mut next_obj_id = 0x500u32;
    let spawn_school = |template_id: u32, roll: i32, obj_id: u32| {
        server
            .systems
            .npcs
            .doodads
            .set_forced_phase_ratio(Some(roll));
        let school = manager::create(&server, &world, obj_id, template_id, None, true).unwrap();
        school.data().transform.set_xyz(110.0, 100.0, 100.0);
        manager::spawn(&server, &world, &school);
        manager::init_doodad(&server, &world, &school);
        // Start phase (16997 / 16995) arms a 1000 ms timer into the species selection phase.
        server.systems.npcs.doodads.tasks.advance(&server, 1_000);
        school
    };

    // Saltwater 6448: 4000 Tuna (17231), 1500 Sailfish (17233), 1000 Marlin (17235), 3500 Sturgeon (17237).
    for (roll, expected_group) in [
        (0, 17231),
        (3_999, 17231),
        (4_000, 17233),
        (5_499, 17233),
        (5_500, 17235),
        (6_499, 17235),
        (6_500, 17237),
        (9_999, 17237),
    ] {
        let school = spawn_school(SALTWATER_FISH_SCHOOL, roll, next_obj_id);
        next_obj_id += 1;
        assert_eq!(group(&school), expected_group, "saltwater roll {roll}");
        // Fallback Timer 7472 (1000 ms -> 17231) must never have been armed.
        assert_eq!(
            server.systems.npcs.doodads.tasks.advance(&server, 1_000),
            0,
            "saltwater roll {roll} should not fall through to fallback timer"
        );
        assert_eq!(group(&school), expected_group);
        manager::delete(&server, &world, &school);
    }

    // Freshwater 6447: 3400 Carp (17243), 3300 Arowana (17246), 3300 Electric Eel (17248).
    for (roll, expected_group) in [
        (0, 17243),
        (3_399, 17243),
        (3_400, 17246),
        (6_699, 17246),
        (6_700, 17248),
        (9_999, 17248),
    ] {
        let school = spawn_school(FRESHWATER_FISH_SCHOOL, roll, next_obj_id);
        next_obj_id += 1;
        assert_eq!(group(&school), expected_group, "freshwater roll {roll}");
        assert_eq!(
            server.systems.npcs.doodads.tasks.advance(&server, 1_000),
            0,
            "freshwater roll {roll} should not fall through to fallback timer"
        );
        assert_eq!(group(&school), expected_group);
        manager::delete(&server, &world, &school);
    }

    // Verify wrong chum vs matching chum -> Feeding Frenzy + buff 5767 + species spawner.
    // (school, roll, inactive phase, wrong chum skill, right chum skill, frenzy phase, spawner):
    // Bluefin Tuna (Sardines), Sailfish (Pike), Blue Marlin (Mackerel), Sturgeon (Roosterfish),
    // then Carp, Arowana and Electric Eel.
    let species_cases = [
        (SALTWATER_FISH_SCHOOL, 0, 17231, 21743, 21702, 17232, 14385),
        (
            SALTWATER_FISH_SCHOOL,
            4_000,
            17233,
            21702,
            21742,
            17234,
            14473,
        ),
        (
            SALTWATER_FISH_SCHOOL,
            5_500,
            17235,
            21702,
            21743,
            17236,
            14474,
        ),
        (
            SALTWATER_FISH_SCHOOL,
            6_500,
            17237,
            21702,
            21741,
            17238,
            14476,
        ),
        (FRESHWATER_FISH_SCHOOL, 0, 17243, 21744, 21693, 17245, 14475),
        (
            FRESHWATER_FISH_SCHOOL,
            3_400,
            17246,
            21693,
            21744,
            17247,
            14477,
        ),
        (
            FRESHWATER_FISH_SCHOOL,
            6_700,
            17248,
            21744,
            21693,
            17250,
            14594,
        ),
    ];
    for (template_id, roll, inactive, wrong_skill, right_skill, frenzy, expected_spawner) in
        species_cases
    {
        let school = spawn_school(template_id, roll, next_obj_id);
        next_obj_id += 1;
        assert_eq!(group(&school), inactive);

        let ctx = DoodadCtx::new(&server, world.clone(), school.clone());
        on_skill_hit(&ctx, Some(&caster), wrong_skill);
        server.systems.npcs.area_triggers.tick(&server);
        assert_eq!(group(&school), inactive, "wrong chum {wrong_skill}");
        assert!(
            !player.data().unit.buffs.check_buff(SCHOOL_BUFF),
            "wrong chum {wrong_skill} must not grant buff {SCHOOL_BUFF}"
        );

        on_skill_hit(&ctx, Some(&caster), right_skill);
        assert_eq!(group(&school), frenzy, "right chum {right_skill}");
        server.systems.npcs.area_triggers.tick(&server);
        assert!(
            player.data().unit.buffs.check_buff(SCHOOL_BUFF),
            "right chum {right_skill} must activate buff {SCHOOL_BUFF}"
        );

        let spawner_id = server
            .game_data
            .systems
            .npcs
            .doodad
            .phase_funcs(frenzy)
            .iter()
            .filter_map(|pf| {
                server
                    .game_data
                    .systems
                    .npcs
                    .doodad
                    .phase_func_template(pf.func_id, &pf.func_type)
            })
            .find_map(|t| match t {
                FuncTemplate::FishSchool(f) => Some(f.npc_spawner_id),
                _ => None,
            });
        assert_eq!(
            spawner_id,
            Some(expected_spawner),
            "frenzy phase {frenzy} fish spawner"
        );

        manager::delete(&server, &world, &school);
        server.systems.npcs.area_triggers.tick(&server);
        assert!(
            !player.data().unit.buffs.check_buff(SCHOOL_BUFF),
            "deleting school must clear buff {SCHOOL_BUFF}"
        );
    }
}

fn harvest_audit_data() -> DoodadData {
    let path = std::env::var("AAEMU_COMPACT_DB").expect("AAEMU_COMPACT_DB");
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    DoodadData::load_from(&conn).unwrap()
}

#[tokio::test]
#[ignore = "requires AAEMU_COMPACT_DB"]
async fn real_data_growth_carries_offline_time_through_two_deadlines() {
    use crate::systems::npcs::doodad::natural::{self, StateRow};

    let f = fx(Builder {
        data: harvest_audit_data(),
        ..Default::default()
    });
    let phase_time_ms = now_ms() - 6_000_000;
    let key = natural::spawn_key(1017, 120.0, 130.0, 100.0);
    f.server
        .systems
        .npcs
        .doodads
        .persistence
        .natural
        .enable_for_test(vec![StateRow {
            world_id: 0,
            spawn_key: key,
            template_id: 1017,
            func_group_id: 1489,
            plant_time: phase_time_ms / 1000,
            growth_time_ms: 0,
            phase_time_ms,
            data: 0,
            interaction_counts: "{}".into(),
            updated: 0,
        }]);

    let d = new_spawner(1017, 501)
        .spawn(&f.server, 0)
        .expect("restored");
    assert_eq!(group(&d), 1489);
    f.advance(1);
    assert_eq!(group(&d), 1490);
    f.advance(1);
    assert_eq!(group(&d), 1491);
    let d = d.data();
    assert_eq!(d.phase_time_ms, phase_time_ms + 1_028_572 + 4_114_286);
    assert_eq!(d.growth_time_ms, d.phase_time_ms + 5_142_858);
}

#[tokio::test]
#[ignore = "requires AAEMU_COMPACT_DB"]
async fn real_data_growth_scale_uses_client_endpoints_and_remaining_delay() {
    let mut f = fx(Builder {
        data: harvest_audit_data(),
        ..Default::default()
    });
    let ctx_data = &f.server.game_data.systems.npcs.doodad;
    let Some(FuncTemplate::Growth(growth)) = ctx_data.phase_func_template(1047, "DoodadFuncGrowth")
    else {
        panic!("missing growth descriptor 1047");
    };
    assert_eq!(
        (growth.start_scale, growth.end_scale, growth.delay),
        (300, 1000, 30_000)
    );
    let d = f.doodad(5059);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), d.clone());
    let d = d.data();
    let packet = SCDoodadPhaseChangedPacket::new(ctx.doodad.obj_id, &d, d.phase_time_ms);
    assert_eq!((packet.func_group_id, packet.time_left), (13149, 30_000));
    let packet = SCDoodadPhaseChangedPacket::new(ctx.doodad.obj_id, &d, d.phase_time_ms + 15_000);
    assert_eq!((packet.func_group_id, packet.time_left), (13149, 15_000));
    assert_eq!(d.scale, 1.0);
    drop(d);

    // The persistent loader supplies both saved timestamps before running phase entry.
    let phase_time_ms = now_ms() - 15_000;
    {
        let mut d = ctx.doodad.data();
        d.phase_time_ms = phase_time_ms;
        d.phase.override_phase_time_ms = phase_time_ms;
    }
    init_doodad(&ctx);
    let d = ctx.doodad.data();
    assert_eq!(d.func_group_id, 13149);
    assert_eq!(d.growth_time_ms, phase_time_ms + 30_000);
    assert_eq!(d.scale, 1.0);
    drop(d);

    // Compact group 9439 runs both Growth 1083 (1000 -> 1500) and Timer 3021.
    // At 2x growth speed the timer still runs for 1000 ms, while the animation needs 500 ms.
    f.server.systems.npcs.doodads.set_growth_rate(2.0);
    let d = manager::create(&f.server, &f.world, 0x501, 3768, None, true).unwrap();
    d.data().transform.set_xyz(120.0, 130.0, 100.0);
    set_func_group_id(&f.server, &d, 9439);
    manager::spawn(&f.server, &f.world, &d);
    let ctx = DoodadCtx::new(&f.server, f.world.clone(), d);
    init_doodad(&ctx);
    let d = ctx.doodad.data();
    let packet = SCDoodadPhaseChangedPacket::new(ctx.doodad.obj_id, &d, d.phase_time_ms);
    assert_eq!((packet.func_group_id, packet.time_left), (9439, 500));
    assert!(
        d.time_left(d.phase_time_ms) >= 1000,
        "timer remains independent"
    );
    let mut snapshot = aaemu_commons::PacketStream::new();
    d.write(
        ctx.doodad.obj_id,
        &d.transform.local(),
        d.phase_time_ms + 250,
        &mut snapshot,
    );
    let snapshot = snapshot.into_bytes();
    assert_eq!(
        u32::from_le_bytes(snapshot[58..62].try_into().unwrap()),
        250
    );
}

#[tokio::test]
#[ignore = "requires AAEMU_COMPACT_DB"]
async fn real_data_doodad_insert_counter_debits_and_restores_progress() {
    use crate::systems::npcs::doodad::natural;
    use crate::systems::npcs::hooks::EconomyBridge;
    use std::sync::atomic::{AtomicI32, Ordering};

    #[derive(Default)]
    struct Contributions(AtomicI32);

    impl EconomyBridge for Contributions {
        fn try_bag_consume_item(
            &self,
            _server: &GameServer,
            _player: &PlayerRef,
            _task: u8,
            template_id: u32,
            count: i32,
        ) -> bool {
            assert_eq!((template_id, count), (27301, 1));
            self.0
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |balance| {
                    (balance >= count).then(|| balance - count)
                })
                .is_ok()
        }
    }

    let f = fx(Builder {
        data: harvest_audit_data(),
        ..Default::default()
    });
    let contributions = Arc::new(Contributions::default());
    f.server
        .systems
        .npcs
        .hooks
        .economy
        .install(contributions.clone());
    let store = &f.server.systems.npcs.doodads.persistence.natural;
    store.enable_for_test(Vec::new());
    let spawner = new_spawner(7229, 501);
    let d = spawner.spawn(&f.server, 0).expect("counter doodad");
    set_func_group_id(&f.server, &d, 19752);

    let caster = f.caster();
    let use_counter = |d: &DoodadRef| {
        manager::use_doodad(&f.server, &f.world, d, Some(&caster), 23780, 0);
    };
    use_counter(&d);
    assert!(d.data().phase.interaction_counts.is_empty());
    contributions.0.store(4, Ordering::Relaxed);
    for _ in 0..4 {
        use_counter(&d);
    }
    assert_eq!(group(&d), 19752);
    assert_eq!(contributions.0.load(Ordering::Relaxed), 0);
    assert_eq!(d.data().phase.interaction_counts.get(&15994), Some(&4));

    let key = natural::spawn_key(7229, 120.0, 130.0, 100.0);
    let (_, saved) = store
        .pending_for_test()
        .into_iter()
        .find(|((_, name), _)| name == &key)
        .expect("counter has a pending save");
    spawner.despawn(&f.server, &d);
    store.enable_for_test(vec![saved.expect("counter was saved")]);
    let d = spawner.spawn(&f.server, 0).expect("restored counter");
    assert_eq!(group(&d), 19752);
    assert_eq!(d.data().phase.interaction_counts.get(&15994), Some(&4));
    contributions.0.store(6, Ordering::Relaxed);
    for _ in 0..5 {
        use_counter(&d);
        assert_eq!(group(&d), 19752);
    }
    use_counter(&d);
    assert_eq!(group(&d), 19755);
    assert_eq!(contributions.0.load(Ordering::Relaxed), 0);
    assert!(d.data().phase.interaction_counts.is_empty());
}

#[tokio::test]
#[ignore = "requires AAEMU_COMPACT_DB"]
async fn real_data_placement_mall_workbench_initial_group() {
    let mut f = fx(Builder {
        data: harvest_audit_data(),
        ..Default::default()
    });
    let text = include_str!("../../../../app/Data/Worlds/arche_mall_world/doodad_spawns.json");
    let mut def = parse_list(text, DoodadSpawnerDef::from_json)
        .unwrap()
        .into_iter()
        .find(|def| def.base.id == 1399)
        .expect("Fellowship Workbench placement");
    assert_eq!(def.base.unit_id, 7701);
    def.base.position = WorldSpawnPosition {
        x: 120.0,
        y: 130.0,
        z: 100.0,
        ..Default::default()
    };
    let spawner = Arc::new(DoodadSpawner::new(def, Some(0)));
    let d = spawner.spawn(&f.server, 0).expect("spawns");
    assert_eq!(group(&d), 22066);
    let (_, body) = f
        .client
        .drain()
        .into_iter()
        .find(|(opcode, _)| *opcode == sc::SCDoodadCreatedPacket)
        .expect("first creation packet");
    assert_eq!(u32::from_le_bytes(body[34..38].try_into().unwrap()), 22066);
    spawner.despawn(&f.server, &d);
    let d = spawner.spawn(&f.server, 0).expect("respawns");
    assert_eq!(group(&d), 22066);

    spawner.despawn(&f.server, &d);
    let d = spawner
        .event_copy(0)
        .spawn(&f.server, 0)
        .expect("fresh event placement");
    assert_eq!(group(&d), 22066);

    let foreign = r#"[{"UnitId":7701,"funcGroupId":1490,"Position":{"X":120,"Y":130,"Z":100}}]"#;
    let def = parse_list(foreign, DoodadSpawnerDef::from_json)
        .unwrap()
        .remove(0);
    let d = Arc::new(DoodadSpawner::new(def, Some(0)))
        .spawn(&f.server, 0)
        .expect("spawns with fallback group");
    assert_eq!(group(&d), 21218);
}
