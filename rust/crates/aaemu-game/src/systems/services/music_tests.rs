//! Port of AAEmu.UnitTests/Game/Core/Managers/MusicManagerTests.cs
//!
//! `Constructor_DoesNotCallDeps` verifies (Moq) that the constructor touches neither the
//! `IMusicIdManager` nor the `IItemManager`. The Rust `MusicManager` has no injected
//! dependencies (both are passed per call); construction has no side effects: no songs, no
//! queued uploads, no MIDI cache.

use super::{CSRequestMusicNotesPacket, MusicManager, SongData, BUFF_SCORE_MEMORIZED};
use crate::data::GameData;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::systems::skills::data::BuffTemplate;
use crate::systems::skills::types::SkillCaster;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;
use crate::GameServer;
use aaemu_commons::PacketStream;

#[test]
fn constructor_does_not_call_deps() {
    let manager = MusicManager::default();
    assert!(manager.get_song_by_id(1).is_none());
    assert!(manager.queued_upload(1).is_none());
    assert!(manager.get_midi_cache(1).is_empty());
}

fn request_fixture() -> (GameServer, TestClient, PlayerRef) {
    let mut data = GameData::empty();
    data.systems.skills.add_buff(BuffTemplate {
        id: BUFF_SCORE_MEMORIZED,
        kind: 1,
        ..BuffTemplate::default()
    });
    let server = test_server_with(data);
    crate::systems::skills::install_hooks(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Listener", 10.0, 10.0),
        1,
    );
    client.conn.active_char = Some(player.clone());
    client.drain();
    (server, client, player)
}

fn stored_song(id: u32) -> SongData {
    // Reading does not require authorship or holding a sheet in the 1.2 server.
    SongData {
        id,
        author_id: 2,
        title: "T".into(),
        song: "abc".into(),
        source_item_id: 0,
    }
}

fn native_request(id: u64, is_tooltip: bool, show_buff_only: bool) -> CSRequestMusicNotesPacket {
    let mut body = id.to_le_bytes().to_vec();
    body.extend([u8::from(is_tooltip), u8::from(show_buff_only)]);
    let mut stream = PacketStream::from_bytes(body);
    let request = CSRequestMusicNotesPacket::read(&mut stream).unwrap();
    assert_eq!(stream.remaining(), 0);
    request
}

fn expected_notes(id: u32, is_tooltip: bool) -> Vec<u8> {
    let mut body = id.to_le_bytes().to_vec();
    body.extend([
        u8::from(is_tooltip),
        3,
        0,
        0,
        0,
        1,
        0,
        b'T',
        4,
        0,
        b'a',
        b'b',
        b'c',
        0,
    ]);
    body
}

#[tokio::test]
async fn native_uncached_play_returns_notes() {
    let (server, mut client, player) = request_fixture();
    server
        .systems
        .services
        .music
        .st()
        .all_songs
        .insert(7, stored_song(7));

    // Native 39082550 cache miss: u64 noteId, isTooltip=false, showBuffOnly=false.
    let request = CSRequestMusicNotesPacket::read(&mut PacketStream::from_bytes(vec![
        7, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]))
    .unwrap();
    request.handle(&server, &mut client.conn).await.unwrap();
    assert_eq!(
        client.drain(),
        vec![(
            sc::SCUserNoteLoadedPacket,
            vec![7, 0, 0, 0, 0, 3, 0, 0, 0, 1, 0, b'T', 4, 0, b'a', b'b', b'c', 0],
        )],
        "native uncached playback must receive SC1e0 with isTooltip=false"
    );
    assert!(!player.data().unit.buffs.check_buff(BUFF_SCORE_MEMORIZED));
}

#[tokio::test]
async fn native_request_flag_combinations_found_and_missing() {
    for found in [true, false] {
        for is_tooltip in [false, true] {
            for show_buff_only in [false, true] {
                let (server, mut client, player) = request_fixture();
                let observer = TestClient::new(&server, 2, 2);
                let other = spawn_player(
                    &server,
                    &observer,
                    test_character(2, "Author", 12.0, 10.0),
                    2,
                );
                client.drain();
                if found {
                    server
                        .systems
                        .services
                        .music
                        .st()
                        .all_songs
                        .insert(7, stored_song(7));
                }
                native_request(7, is_tooltip, show_buff_only)
                    .handle(&server, &mut client.conn)
                    .await
                    .unwrap();
                let frames = client.drain();
                let notes: Vec<_> = frames
                    .iter()
                    .filter(|(op, _)| *op == sc::SCUserNoteLoadedPacket)
                    .cloned()
                    .collect();
                let expected = if show_buff_only {
                    Vec::new()
                } else {
                    vec![(
                        sc::SCUserNoteLoadedPacket,
                        if found {
                            expected_notes(7, is_tooltip)
                        } else {
                            Vec::new()
                        },
                    )]
                };
                assert_eq!(
                    notes, expected,
                    "found={found}, tooltip={is_tooltip}, buff={show_buff_only}"
                );
                // The existing cached path adds the buff even for an unknown song ID.
                assert_eq!(
                    player.data().unit.buffs.check_buff(BUFF_SCORE_MEMORIZED),
                    show_buff_only
                );
                // SCUnitState also queues patron buffs when another player sees
                // us; identify ScoreMemorized and verify its actual wire target.
                let memorized: Vec<_> = frames
                    .iter()
                    .filter(|(op, _)| *op == sc::SCBuffCreatedPacket)
                    .filter_map(|(_, body)| {
                        let mut s = PacketStream::from_bytes(body.clone());
                        let kind = s.read_u8().unwrap();
                        let caster = SkillCaster::read(kind, &mut s).unwrap();
                        let caster_id = s.read_u32().unwrap();
                        let owner_obj_id = s.read_bc().unwrap();
                        let _index = s.read_u32().unwrap();
                        (s.read_u32().unwrap() == BUFF_SCORE_MEMORIZED).then_some((
                            caster,
                            caster_id,
                            owner_obj_id,
                        ))
                    })
                    .collect();
                assert_eq!(
                    memorized,
                    if show_buff_only {
                        vec![(
                            SkillCaster::Unit(player.obj_id),
                            player.character_id,
                            player.obj_id,
                        )]
                    } else {
                        Vec::new()
                    }
                );
                assert!(!other.data().unit.buffs.check_buff(BUFF_SCORE_MEMORIZED));
            }
        }
    }
}

#[tokio::test]
async fn native_request_requires_active_character() {
    for is_tooltip in [false, true] {
        for show_buff_only in [false, true] {
            let (server, mut client, player) = request_fixture();
            server
                .systems
                .services
                .music
                .st()
                .all_songs
                .insert(7, stored_song(7));
            client.conn.active_char = None;
            native_request(7, is_tooltip, show_buff_only)
                .handle(&server, &mut client.conn)
                .await
                .unwrap();
            assert!(client.drain().is_empty());
            assert!(!player.data().unit.buffs.check_buff(BUFF_SCORE_MEMORIZED));
        }
    }
}

#[tokio::test]
async fn request_preserves_u32_lookup_and_response_id() {
    // The legacy server casts the request's u64 to u32. Native SC1e0 consumes
    // signed32 and sign-extends it (391e1691 cdq); changing either width is unsafe.
    for id in [
        0,
        7,
        0x1_0000_0007,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff_8000_0000,
        u64::MAX,
    ] {
        for is_tooltip in [false, true] {
            let (server, mut client, _) = request_fixture();
            let key = id as u32;
            server
                .systems
                .services
                .music
                .st()
                .all_songs
                .insert(key, stored_song(key));
            native_request(id, is_tooltip, false)
                .handle(&server, &mut client.conn)
                .await
                .unwrap();
            assert_eq!(
                client.drain(),
                vec![(sc::SCUserNoteLoadedPacket, expected_notes(key, is_tooltip))]
            );
        }
    }
}

#[test]
fn native_request_flag_bytes_and_truncation() {
    // Original constructor/serializer 397acd70/397b1480; retained native.tsv request_00..11.
    for is_tooltip in [0, 1] {
        for show_buff_only in [0, 1] {
            let body = [4, 3, 2, 1, 8, 7, 6, 5, is_tooltip, show_buff_only];
            for end in 0..body.len() {
                assert!(
                    CSRequestMusicNotesPacket::read(&mut PacketStream::from_bytes(
                        body[..end].to_vec()
                    ))
                    .is_err()
                );
            }
            let request =
                CSRequestMusicNotesPacket::read(&mut PacketStream::from_bytes(body.to_vec()))
                    .unwrap();
            assert_eq!(request.id64, 0x0506_0708_0102_0304);
            assert_eq!(request.is_tooltip, is_tooltip != 0);
            assert_eq!(request.show_buff_only, show_buff_only != 0);
            assert_eq!(
                request,
                native_request(request.id64, is_tooltip != 0, show_buff_only != 0)
            );
        }
    }
}
