//! Plot-driven cast bars: the 1.2 client learns a plot cast's duration from the
//! `SCPlotEventPacket` `castingTime` field (reader `FUN_397b2160`: u16 on the wire, x10 in ms).
//! Akmit's "Terrifying Roar" (skill 17563) has `casting_time` 0 and plot 338; the plot's cast
//! event carries a 5000 ms next-event delay.
//!
//! The real-data test is gated by `AAEMU_COMPACT_DB`.

use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::PacketStream;
use rusqlite::{Connection, OpenFlags};

use crate::data::GameData;
use crate::models::npc::Npc;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, NpcRef, PlayerRef, WorldObject};

use super::super::data::plots::{
    Plot, PlotData, PlotEventTemplate, PlotNextEvent, PlotNode, PlotTree,
};
use super::super::data::SkillTemplate;
use super::super::skill::use_skill;
use super::super::state::{SkillRef, SkillRun};
use super::super::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};

const SKILL: u32 = 17563;
const PLOT: u32 = 338;
const CAST_MS: u16 = 5000;

fn fixture(data: GameData) -> (Arc<crate::GameServer>, TestClient, PlayerRef, NpcRef) {
    let server = Arc::new(test_server_with(data));
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "Target", 100.0, 100.0);
    character.unit.hp = 1000;
    character.unit.mp = 1000;
    character.unit.max_mp = 1000;
    let player = spawn_player(&server, &client, character, 0x100);
    let mut n = Npc::new(1);
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.mp = 1000;
    n.unit.max_mp = 1000;
    n.unit.level = 21;
    n.unit.faction_id = 101;
    n.unit.transform.set_xyz(100.0, 105.0, 100.0);
    let npc = Entity::new(0x200, n);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    (server, client, player, npc)
}

/// The `castingTime` field of a `SCPlotEventPacket` body.
fn packet_cast_time(body: &[u8]) -> u16 {
    let mut s = PacketStream::from_bytes(body.to_vec());
    s.read_u16().unwrap(); // tl
    s.read_u32().unwrap(); // eventId
    s.read_u32().unwrap(); // skillId
    for _ in 0..2 {
        match s.read_u8().unwrap() {
            1 => {
                s.read_bc().unwrap();
            }
            2 => {
                s.read_position().unwrap();
                s.read_i8().unwrap();
                s.read_i8().unwrap();
                s.read_i8().unwrap();
            }
            other => panic!("unknown plot object kind {other}"),
        }
    }
    s.read_u64().unwrap(); // itemId
    s.read_bc().unwrap(); // objId
    s.read_u16().unwrap() // castingTime (wire deciseconds)
}

fn next_event(id: u32, event_id: u32, casting: bool, delay: i32) -> PlotNextEvent {
    PlotNextEvent {
        id,
        event_id,
        position: 1,
        per_target: false,
        casting,
        delay,
        speed: 0,
        channeling: false,
        casting_inc: 0,
        add_anim_cs_time: false,
        casting_delayable: false,
        casting_cancelable: false,
        cancel_on_big_hit: false,
        use_exe_time: false,
        fail: false,
    }
}

fn synthetic_plot(data: &mut GameData) {
    let root = PlotEventTemplate {
        id: 1,
        plot_id: 1,
        position: 1,
        source_update_method_id: 1,
        target_update_method_id: 2,
        next_events: vec![next_event(10, 2, false, 0)],
        ..Default::default()
    };
    let cast = PlotEventTemplate {
        id: 2,
        plot_id: 1,
        position: 2,
        source_update_method_id: 1,
        target_update_method_id: 2,
        next_events: vec![next_event(20, 3, true, i32::from(CAST_MS))],
        ..Default::default()
    };
    let target = PlotEventTemplate {
        id: 3,
        plot_id: 1,
        position: 3,
        source_update_method_id: 1,
        target_update_method_id: 2,
        ..Default::default()
    };
    let plots = &mut data.systems.skills.plots;
    plots.plots.insert(
        1,
        Plot {
            id: 1,
            target_type_id: 0,
            event_id: Some(1),
        },
    );
    for ev in [root, cast, target] {
        plots.events.insert(ev.id, ev);
    }
    plots.trees.insert(
        1,
        PlotTree {
            plot_id: 1,
            nodes: vec![
                PlotNode {
                    event_id: 1,
                    parent: None,
                    parent_next_event: None,
                    children: vec![1],
                },
                PlotNode {
                    event_id: 2,
                    parent: Some(0),
                    parent_next_event: Some((1, 0)),
                    children: vec![2],
                },
                PlotNode {
                    event_id: 3,
                    parent: Some(1),
                    parent_next_event: Some((2, 0)),
                    children: Vec::new(),
                },
            ],
        },
    );
}

fn cast(server: &crate::GameServer, player: &PlayerRef, npc: &NpcRef, plot: u32) -> SkillRef {
    let skill: SkillRef = SkillRun::new(
        Arc::new(SkillTemplate {
            id: SKILL,
            plot_id: Some(plot),
            casting_time: 0,
            ..Default::default()
        }),
        1,
    );
    let (result, _) = use_skill(
        server,
        &skill,
        &WorldObject::from(npc),
        SkillCaster::Unit(npc.obj_id),
        &mut SkillCastTarget::Unit(player.obj_id),
        Some(SkillObject::None),
        false,
    );
    assert_eq!(result, skill_result::SUCCESS);
    skill
}

/// Every `SCPlotEventPacket` cast time in the frames received since the last call,
/// including the ones inside a level-4 `CompressedGamePackets` batch.
fn drain_cast_times(client: &mut TestClient) -> Vec<u16> {
    let mut times = Vec::new();
    for frame in client.drain_frames() {
        let bytes = &frame.bytes[..];
        match bytes.get(1) {
            Some(1) => {
                let (op, body) = (u16::from_le_bytes([bytes[4], bytes[5]]), &bytes[6..]);
                if op == crate::opcodes::sc::SCPlotEventPacket {
                    times.push(packet_cast_time(body));
                }
            }
            Some(4) => {
                let Some(payload) = super::super::packets::inflate_stored(&bytes[4..]) else {
                    continue;
                };
                let mut s = PacketStream::from_bytes(payload);
                if s.remaining() > 4 {
                    s.read_u16().unwrap();
                    let op = s.read_u16().unwrap();
                    let n = s.remaining();
                    let body = s.read_bytes(n).unwrap();
                    if op == crate::opcodes::sc::SCPlotEventPacket {
                        times.push(packet_cast_time(&body));
                    }
                }
            }
            _ => {}
        }
    }
    times
}

/// A plot cast node with a `casting` next-event sends its delay in the `SCPlotEventPacket`
/// `castingTime` field, like C# `PlotNode.Execute` (`NextEvent.Delay / 10`).
#[tokio::test]
async fn plot_cast_event_carries_the_next_casting_delay() {
    let mut data = GameData::empty();
    synthetic_plot(&mut data);
    let (server, mut client, player, npc) = fixture(data);
    let _skill = cast(&server, &player, &npc, 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(drain_cast_times(&mut client), vec![CAST_MS / 10]);
}

/// Akmit's real plot 338: skill 17563 has `casting_time` 0; the plot's "cast" event 2599
/// carries the 5000 ms delay that the video shows as a 5.5-6 s target cast bar.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_akmit_terrifying_roar_plot_carries_its_cast_time() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut data = GameData::empty();
    data.systems.skills.plots = PlotData::load(&c).unwrap();
    let (server, mut client, player, npc) = fixture(data);
    cast(&server, &player, &npc, PLOT);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let times = drain_cast_times(&mut client);
    assert!(
        times.contains(&(CAST_MS / 10)),
        "plot 338 must send a 5000 ms cast event; got {times:?}"
    );
}
