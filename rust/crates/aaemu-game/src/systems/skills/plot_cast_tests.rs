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

/// A `plot_only` skill commits its GCD and cooldown when accepted; cancelling the plot while it
/// is still casting (no effect has landed) hands both back and tells the client.
#[tokio::test]
async fn plot_only_cancelled_during_casting_restores_cooldown_and_gcd() {
    let mut data = GameData::empty();
    synthetic_plot(&mut data);
    let (server, mut client, player, npc) = fixture(data);
    let skill: SkillRef = SkillRun::new(
        Arc::new(SkillTemplate {
            id: SKILL,
            plot_id: Some(1),
            plot_only: true,
            custom_gcd: 500,
            cooldown_time: 30_000,
            cooldown_tag_id: 9,
            ..Default::default()
        }),
        1,
    );
    let caster = WorldObject::from(&player);
    let (result, _) = use_skill(
        &server,
        &skill,
        &caster,
        SkillCaster::Unit(player.obj_id),
        &mut SkillCastTarget::Unit(npc.obj_id),
        Some(SkillObject::None),
        false,
    );
    assert_eq!(result, skill_result::SUCCESS);
    {
        let p = player.data();
        assert!(p.unit.cooldowns.cooldowns.contains_key(&SKILL));
        assert!(p.unit.cooldowns.tag_cooldowns.contains_key(&9));
        assert!(p.unit.global_cooldown_ms > 0);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.drain_frames();
    skill
        .state()
        .active_plot
        .clone()
        .unwrap()
        .request_cancellation();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    {
        let p = player.data();
        assert!(!p.unit.cooldowns.cooldowns.contains_key(&SKILL));
        assert!(!p.unit.cooldowns.tag_cooldowns.contains_key(&9));
        assert_eq!(p.unit.global_cooldown_ms, 0);
    }
    let resets = client
        .drain_frames()
        .iter()
        .filter(|f| {
            f.bytes.get(1) == Some(&1)
                && u16::from_le_bytes([f.bytes[4], f.bytes[5]])
                    == crate::opcodes::sc::SCSkillCooldownResetPacket
        })
        .count();
    assert_eq!(resets, 2, "skill reset and GCD reset");
}

const REEL_A: u32 = 21196;
const REEL_B: u32 = 21195;
/// The initial cast (21578) is also in the real tag 1024 but is not a reel response.
const TAGGED_CAST: u32 = 21578;
/// The landing skill, also tagged 1024 and not a reel response.
const TAGGED_LANDING: u32 = 21322;
const UNRELATED: u32 = 17564;

fn reel_data() -> GameData {
    let mut data = GameData::empty();
    synthetic_plot(&mut data);
    for id in [REEL_A, REEL_B, TAGGED_CAST, TAGGED_LANDING] {
        data.systems.skills.skill_tags.insert(id, vec![1024]);
    }
    data
}

fn plot_only(id: u32, tag: i32) -> SkillRef {
    SkillRun::new(
        Arc::new(SkillTemplate {
            id,
            plot_id: Some(1),
            plot_only: true,
            cooldown_time: 30_000,
            cooldown_tag_id: tag,
            ..Default::default()
        }),
        1,
    )
}

fn use_plot_only(server: &crate::GameServer, skill: &SkillRef, caster: &WorldObject, target: u32) {
    let (result, _) = use_skill(
        server,
        skill,
        caster,
        SkillCaster::Unit(caster.obj_id()),
        &mut SkillCastTarget::Unit(target),
        Some(SkillObject::None),
        false,
    );
    assert_eq!(result, skill_result::SUCCESS);
}

/// An accepted sport reel response cancels the player's previous one (the old plot keeps
/// repeating its conditions and cannot be stopped once it is no longer the newest). The old end
/// releases only its own timeline: it cannot clear the replacement, nor give back the cooldowns
/// the replacement just committed.
#[tokio::test]
async fn replacement_sport_reel_cancels_previous_plot_without_clearing_the_replacement() {
    let (server, _client, player, npc) = fixture(reel_data());
    let caster = WorldObject::from(&player);
    let old = plot_only(REEL_A, 9);
    use_plot_only(&server, &old, &caster, npc.obj_id);
    let (old_skill_cooldown, old_tag_cooldown) = {
        let p = player.data();
        (
            p.unit.cooldowns.cooldowns[&REEL_A],
            p.unit.cooldowns.tag_cooldowns[&9],
        )
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let old_plot = old.state().active_plot.clone().unwrap();
    assert!(!old_plot.cancellation_requested());
    assert!(old_plot.inner().is_casting);

    let new = plot_only(REEL_B, 10);
    use_plot_only(&server, &new, &caster, npc.obj_id);
    let new_plot = new.state().active_plot.clone().unwrap();
    assert!(old_plot.cancellation_requested(), "previous reel cancelled");
    assert!(
        !new_plot.cancellation_requested(),
        "replacement keeps running"
    );

    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(old.state().active_plot.is_none(), "old plot ended");
    assert_eq!(old.tl_id(), 0, "old timeline released");
    let active = player.data().unit.skill_state.active_plot.clone();
    assert!(
        active.is_some_and(|p| Arc::ptr_eq(&p.0, &new_plot)),
        "the old end must not clear the replacement"
    );
    assert!(new.state().active_plot.is_some());
    assert_ne!(new.tl_id(), 0);
    assert_eq!(server.systems.skills.tl_ids.in_use(), 1);
    let p = player.data();
    assert_eq!(
        p.unit.cooldowns.cooldowns.get(&REEL_A),
        Some(&old_skill_cooldown),
        "replacing the reel keeps the old skill cooldown"
    );
    assert_eq!(
        p.unit.cooldowns.tag_cooldowns.get(&9),
        Some(&old_tag_cooldown),
        "replacing the reel keeps the old tag cooldown"
    );
    assert!(p.unit.cooldowns.cooldowns.contains_key(&REEL_B));
    assert!(p.unit.cooldowns.tag_cooldowns.contains_key(&10));
}

/// Only reel plots supersede each other: an unrelated plot of the player survives a reel, a reel
/// survives an unrelated plot, and NPC casts of tagged skills are never cancelled.
#[tokio::test]
async fn sport_reel_replacement_leaves_unrelated_and_npc_plots_alone() {
    let (server, _client, player, npc) = fixture(reel_data());
    let caster = WorldObject::from(&player);
    let other = plot_only(UNRELATED, 11);
    use_plot_only(&server, &other, &caster, npc.obj_id);
    let other_plot = other.state().active_plot.clone().unwrap();
    let reel = plot_only(REEL_A, 9);
    use_plot_only(&server, &reel, &caster, npc.obj_id);
    assert!(!other_plot.cancellation_requested(), "unrelated plot kept");

    let reel_plot = reel.state().active_plot.clone().unwrap();
    let after = plot_only(UNRELATED + 1, 12);
    use_plot_only(&server, &after, &caster, npc.obj_id);
    assert!(
        !reel_plot.cancellation_requested(),
        "unrelated replacement keeps reel"
    );

    // A tagged non-response skill (the initial cast) neither cancels a reel nor is cancelled
    // by one, although it shares tag 1024.
    let cast = plot_only(TAGGED_CAST, 13);
    use_plot_only(&server, &cast, &caster, npc.obj_id);
    let cast_plot = cast.state().active_plot.clone().unwrap();
    assert!(
        !after
            .state()
            .active_plot
            .as_ref()
            .unwrap()
            .cancellation_requested(),
        "tagged cast replacing an unrelated plot cancels nothing"
    );
    let reel_after_cast = plot_only(REEL_B, 14);
    use_plot_only(&server, &reel_after_cast, &caster, npc.obj_id);
    assert!(
        !cast_plot.cancellation_requested(),
        "a reel does not cancel the tagged initial cast"
    );
    let cast_again = plot_only(TAGGED_LANDING, 15);
    use_plot_only(&server, &cast_again, &caster, npc.obj_id);
    assert!(
        !reel_after_cast
            .state()
            .active_plot
            .as_ref()
            .unwrap()
            .cancellation_requested(),
        "a tagged non-reel skill replacing a reel does not cancel it"
    );

    let npc_caster = WorldObject::from(&npc);
    let first = plot_only(REEL_A, 0);
    use_plot_only(&server, &first, &npc_caster, player.obj_id);
    let first_plot = first.state().active_plot.clone().unwrap();
    let second = plot_only(REEL_B, 0);
    use_plot_only(&server, &second, &npc_caster, player.obj_id);
    assert!(
        !first_plot.cancellation_requested(),
        "NPC cast not cancelled"
    );
}
