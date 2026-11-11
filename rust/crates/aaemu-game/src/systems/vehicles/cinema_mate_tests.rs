//! A cut-scene cinema clears the 1.2 client's own-mate records, so the re-send of the visible
//! objects after it has to bring the owner's `SCMateSpawnedPacket` first: the mate's unit state
//! and the rider's `SCUnitAttachedPacket` look the record up (attaching to a mate without a
//! record crashes the client). A cinema that is not a cut scene removes nothing.

use super::*;
use crate::net::registry::ClientPacket;
use crate::systems::quests::cinema::CinemaInfo;
use crate::systems::quests::data::QuestData;
use crate::systems::quests::model::{detail, kind};
use crate::systems::quests::packets::{
    CSCompletedCinemaPacket, CSRequestPermissionToPlayCinemaForDirectingMode, CSStartedCinemaPacket,
};
use crate::systems::quests::templates::{QuestComponentTemplate, QuestTemplate};

/// Quest 50's components play cinema 80 (not a cut scene) and cinema 16 (a cut scene).
const COMP_PLAIN: u32 = 5001;
const COMP_CUT: u32 = 5002;

fn cinema_server() -> Arc<GameServer> {
    let mut quests = QuestData::default();
    quests.add_template(QuestTemplate {
        id: 50,
        detail_id: detail::NORMAL,
        ..QuestTemplate::default()
    });
    for (id, cinema_id) in [(COMP_PLAIN, 80), (COMP_CUT, 16)] {
        quests.add_component(QuestComponentTemplate {
            id,
            quest_id: 50,
            kind_id: kind::START,
            npc_ai_id: 1,
            cinema_id,
            ..QuestComponentTemplate::default()
        });
    }
    for (id, name, cut_scene) in [(80, "sl_plain", false), (16, "fly_cut", true)] {
        quests.cinemas.insert(
            id,
            CinemaInfo {
                id,
                name: name.to_string(),
                replay: false,
                cut_scene: Some(cut_scene),
            },
        );
    }
    quests.finish();
    let mut d = data();
    d.systems.quests = quests;
    server_with(d)
}

struct Rider {
    server: Arc<GameServer>,
    client: TestClient,
    owner: PlayerRef,
    mate: crate::world::MateRef,
    /// The `SCMateSpawnedPacket` body the summon sent.
    summoned: Vec<u8>,
}

/// An owner (connected, active) with a spawned mate; `mounted` seats the owner on it.
fn rider(mounted: bool) -> Rider {
    let server = cinema_server();
    let mut client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = player_with_item(&server, &client, 1, PA, (100.0, 100.0), MATE_ITEM);
    client.conn.active_char = Some(owner.clone());
    mates::spawn_mount(&server, &owner, item_id);
    let summoned = body(&client.drain(), sc::SCMateSpawnedPacket);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    if mounted {
        let tl = u32::from(mate.data().unit.tl_id);
        mates::mount_mate(&server, &owner, tl, attach_point::DRIVER, 1);
        assert!(owner.data().is_riding);
        client.drain();
    }
    Rider {
        server,
        client,
        owner,
        mate,
        summoned,
    }
}

impl Rider {
    /// Plays a cinema the way the client does (optionally announced by its component) and
    /// returns the frames the owner got for the completion.
    async fn play(&mut self, component: Option<u32>) -> Vec<(u16, Vec<u8>)> {
        if let Some(component_id) = component {
            CSRequestPermissionToPlayCinemaForDirectingMode {
                component_id,
                npc_obj_id: 0,
                doodad_obj_id: 0,
            }
            .handle(&self.server, &mut self.client.conn)
            .await
            .unwrap();
        }
        CSStartedCinemaPacket
            .handle(&self.server, &mut self.client.conn)
            .await
            .unwrap();
        self.client.drain();
        CSCompletedCinemaPacket
            .handle(&self.server, &mut self.client.conn)
            .await
            .unwrap();
        self.client.drain()
    }

    /// Index of the first frame `op` whose body starts with the bc object id `obj_id`.
    fn index_of(&self, frames: &[(u16, Vec<u8>)], op: u16, obj_id: u32) -> usize {
        frames
            .iter()
            .position(|(o, b)| *o == op && b.starts_with(&bc(obj_id)))
            .unwrap_or_else(|| panic!("no 0x{op:03x} for {obj_id:x} in {:x?}", ops(frames)))
    }
}

#[tokio::test]
async fn a_cut_scene_restores_the_mate_record_before_the_mate() {
    let mut r = rider(false);
    let frames = r.play(Some(COMP_CUT)).await;
    let spawned = frames
        .iter()
        .position(|f| f.0 == sc::SCMateSpawnedPacket)
        .expect("SCMateSpawnedPacket");
    // The same bytes as the summon sent.
    assert_eq!(frames[spawned].1, r.summoned);
    assert_eq!(
        ops(&frames)
            .iter()
            .filter(|op| **op == sc::SCMateSpawnedPacket)
            .count(),
        1
    );
    let state = r.index_of(&frames, sc::SCUnitStatePacket, r.mate.obj_id);
    assert!(spawned < state, "{:x?}", ops(&frames));
}

/// A rider leaves a cut scene on foot: the server dismounts it (`SCUnitDetached` for the rider)
/// before the repaint, which then restores the mate record and the mate without re-attaching
/// the rider (tester report: unable to move or dismount after Pederson's cinema while mounted).
#[tokio::test]
async fn a_cut_scene_dismounts_the_rider_before_the_repaint() {
    let mut r = rider(true);
    let frames = r.play(Some(COMP_CUT)).await;
    let detached = r.index_of(&frames, sc::SCUnitDetachedPacket, PA);
    let spawned = frames
        .iter()
        .position(|f| f.0 == sc::SCMateSpawnedPacket)
        .expect("SCMateSpawnedPacket");
    let state = r.index_of(&frames, sc::SCUnitStatePacket, r.mate.obj_id);
    assert!(detached < spawned && spawned < state, "{:x?}", ops(&frames));
    assert!(
        !ops(&frames).contains(&sc::SCUnitAttachedPacket),
        "{:x?}",
        ops(&frames)
    );
    assert!(!r.owner.data().is_riding);
    assert!(r
        .mate
        .data()
        .passenger(attach_point::DRIVER)
        .is_none_or(|seat| seat.obj_id == 0));
}

#[tokio::test]
async fn an_unidentified_cinema_restores_the_mate_record_too() {
    let mut r = rider(true);
    let frames = r.play(None).await;
    assert!(frames.iter().any(|f| f.0 == sc::SCMateSpawnedPacket));
    assert!(!ops(&frames).contains(&sc::SCUnitAttachedPacket));
    assert!(!r.owner.data().is_riding);
}

/// A cinema that is not a cut scene keeps the rider mounted (nothing was wiped).
#[tokio::test]
async fn a_cinema_that_is_not_a_cut_scene_keeps_the_rider_mounted() {
    let mut r = rider(true);
    r.play(Some(COMP_PLAIN)).await;
    assert!(r.owner.data().is_riding);
}

#[tokio::test]
async fn a_cinema_that_is_not_a_cut_scene_sends_no_mate_record() {
    for mounted in [false, true] {
        let mut r = rider(mounted);
        let frames = r.play(Some(COMP_PLAIN)).await;
        let o = ops(&frames);
        assert!(!o.contains(&sc::SCMateSpawnedPacket), "{o:x?}");
        assert!(!o.contains(&sc::SCUnitStatePacket), "{o:x?}");
        assert!(!o.contains(&sc::SCUnitAttachedPacket), "{o:x?}");
    }
}

#[tokio::test]
async fn only_the_owner_gets_its_mates_and_not_a_deleted_one() {
    let mut r = rider(false);
    let mut b = TestClient::new(&r.server, 2, 2);
    let pb = spawn_player(&r.server, &b, test_character(2, "B", 110.0, 110.0), PB);
    b.drain();
    r.client.drain();
    mates::resend_owner_mates_spawned(&r.server, &pb);
    assert!(b.drain().is_empty(), "a player without mates gets none");
    mates::resend_owner_mates_spawned(&r.server, &r.owner);
    assert_eq!(body(&r.client.drain(), sc::SCMateSpawnedPacket), r.summoned);
    assert!(b.drain().is_empty(), "other players do not get it");
    r.mate.data().deleted = true;
    mates::resend_owner_mates_spawned(&r.server, &r.owner);
    assert!(r.client.drain().is_empty());
}
