//! Port of AAEmu.UnitTests/Game/Core/Managers/MusicManagerTests.cs
//!
//! `Constructor_DoesNotCallDeps` verifies (Moq) that the constructor touches neither the
//! `IMusicIdManager` nor the `IItemManager`. The Rust `MusicManager` has no injected
//! dependencies (both are passed per call); construction has no side effects: no songs, no
//! queued uploads, no MIDI cache.

use super::{
    CSRequestMusicNotesPacket, InstrumentSounds, MusicManager, SongData, BUFF_SCORE_MEMORIZED,
};
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

const INSTRUMENT: u32 = 20480; // a real `instrument_sounds` item id
const OTHER_ITEM: u32 = 1000;

/// A player with a plain item and an owned instrument, and `instrument_sounds` containing only
/// the instrument template.
fn upload_fixture() -> (GameServer, TestClient, PlayerRef, u64, u64) {
    use crate::data::items::ItemTemplate;
    use crate::systems::economy::inventory::Inv;

    let mut data = GameData::empty();
    data.systems.services.instrument_sounds = InstrumentSounds::from_ids([INSTRUMENT]);
    for id in [INSTRUMENT, OTHER_ITEM] {
        data.items
            .templates
            .insert(id, ItemTemplate::plain(id, 1000));
    }
    let server = test_server_with(data);
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Composer", 10.0, 10.0),
        1,
    );
    client.conn.active_char = Some(player.clone());
    let give = |template: u32, slot: i32| {
        let item = server
            .items
            .create(
                template,
                1,
                0,
                1,
                &server.game_data.items,
                &server.ids.item,
                0,
            )
            .expect("item");
        let id = item.id;
        assert!(Inv::of(&player)
            .bag()
            .lock()
            .unwrap()
            .add_new_item(item, slot, &server.game_data.items)
            .is_ok());
        id
    };
    let instrument = give(INSTRUMENT, 0);
    let other = give(OTHER_ITEM, 1);
    client.drain();
    (server, client, player, instrument, other)
}

async fn upload(
    server: &GameServer,
    client: &mut TestClient,
    song_size: u32,
    item_id: u64,
    song: &str,
) {
    super::CSSaveUserMusicNotesPacket {
        song_size,
        item_id,
        value2: 0,
        title: "Song".into(),
        song: song.into(),
    }
    .handle(server, &mut client.conn)
    .await
    .unwrap();
}

#[tokio::test]
async fn upload_accepts_an_owned_instrument_and_matching_size() {
    let (server, mut client, _player, instrument, _) = upload_fixture();
    upload(&server, &mut client, 3, instrument, "abc").await;
    let queued = server
        .systems
        .services
        .music
        .queued_upload(1)
        .expect("accepted upload");
    assert_eq!(
        (queued.title.as_str(), queued.song.as_str()),
        ("Song", "abc")
    );
    assert_eq!(queued.source_item_id, instrument);
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn upload_accepts_exactly_max_song_chars() {
    let (server, mut client, _player, instrument, _) = upload_fixture();
    let song = "a".repeat(server.config.music.max_song_chars);
    upload(&server, &mut client, song.len() as u32, instrument, &song).await;
    assert!(server.systems.services.music.queued_upload(1).is_some());
}

#[tokio::test]
async fn upload_accepts_a_mismatched_song_size() {
    // The client writer's size unit is not evidenced: warn, do not drop.
    let (server, mut client, _player, instrument, _) = upload_fixture();
    upload(&server, &mut client, 4, instrument, "abc").await;
    assert!(server.systems.services.music.queued_upload(1).is_some());
}

#[tokio::test]
async fn upload_rejects_an_unowned_item() {
    let (server, mut client, _player, instrument, _) = upload_fixture();
    upload(&server, &mut client, 3, instrument + 100, "abc").await;
    assert!(server.systems.services.music.queued_upload(1).is_none());
}

#[tokio::test]
async fn upload_accepts_an_owned_non_instrument_such_as_the_music_paper() {
    // The client sends the bag item it found by id (the music paper), not an instrument.
    let (server, mut client, _player, _, other) = upload_fixture();
    upload(&server, &mut client, 3, other, "abc").await;
    assert!(server.systems.services.music.queued_upload(1).is_some());
}

#[tokio::test]
async fn upload_rejects_a_song_over_max_song_chars() {
    let (server, mut client, _player, instrument, _) = upload_fixture();
    let song = "a".repeat(server.config.music.max_song_chars + 1);
    upload(&server, &mut client, song.len() as u32, instrument, &song).await;
    assert!(server.systems.services.music.queued_upload(1).is_none());
}

#[test]
fn max_song_chars_binds_from_config_and_defaults_to_3000() {
    let default = crate::config::AppConfig::default();
    assert_eq!(default.music.max_song_chars, 3000);
    let config = crate::config::AppConfig::from_value(serde_json::json!({
        "Music": { "MaxSongChars": "4000" }
    }))
    .unwrap();
    assert_eq!(config.music.max_song_chars, 4000);
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn instrument_sounds_real_compact_db() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let sounds = InstrumentSounds::load(&conn).unwrap();
    assert_eq!(sounds.len(), 199);
    assert!(sounds.contains(20480));
    assert!(
        !sounds.contains(4045),
        "the teleport scroll is not an instrument"
    );
}
