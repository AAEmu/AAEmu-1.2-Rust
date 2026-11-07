//! `Core/Managers/MusicManager` (+ `Models/Game/Music/SongData`) and the music packets
//! (`CSSaveUserMusicNotesPacket`, `CSSendUserMusicPacket`, `CSRequestMusicNotesPacket`,
//! `CSEndMusicPacket`, `SCUserNoteLoadedPacket`).
//!
//! The sheet-music item work of `CreateSheetMusic` (ownership, bag space, the new
//! `MusicSheetItem`, consuming the music paper) goes through [`SheetMusicInventory`], which
//! the economy subsystem implements (it also implements the skills `ItemSkillHooks`
//! `midi_cache` / `create_sheet_music` by calling this manager).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use aaemu_commons::{Marshal, PacketStream};
use futures::future::BoxFuture;
use serde::Deserialize;
use sqlx::Row;

use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::economy::error_message::USER_NOTE_CANNOT_SAVE;
use crate::world::{PlayerRef, WorldObject};

/// `BuffConstants.ScoreMemorized`.
pub const BUFF_SCORE_MEMORIZED: u32 = 6010;
/// `TagsEnum.PlaySong`.
pub const TAG_PLAY_SONG: u32 = 1155;
/// `ErrorMessageType.BagFull`.
const ERROR_BAG_FULL: i16 = 25;

/// `Music` (Rust only): the upload sanity the server applies before storing a user song.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MusicConfig {
    /// Outer sanity bound for a `CSSaveUserMusicNotes` song, in UTF-16 code units. The real
    /// limit is the client's `music_note_limits` row for the player's Artistry grade (at most
    /// 1000, also the UI's hard input cap); this cap defaults above it so it never binds.
    /// `Music.MaxSongChars`.
    #[serde(rename = "maxsongchars", deserialize_with = "crate::config::lenient")]
    pub max_song_chars: usize,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            max_song_chars: 3000,
        }
    }
}

/// `actability_groups` id 33 (`예술`, Artistry). The composition UI asks
/// `X2Ability.GetMyActabilityInfo(33)` and matches that grade against
/// `X2UserMusic.GetCompositionLimitInfos()` (`user_music.alb`).
pub const ARTISTRY_ACTABILITY_ID: u32 = 33;

/// `music_note_limits`: the maximum song length per Artistry grade. The 1.2 client loads it
/// (`dll.c:1207337`) and `X2UserMusic.GetCompositionLimitInfos` returns one
/// `{ grade, compositionLimit }` per row; `SaveNotes` refuses a song longer than the player's
/// limit (`FUN_39079640`, `FUN_39082130`).
#[derive(Debug, Default)]
pub struct MusicNoteLimits {
    /// `(step, note_length)` in step order.
    limits: Vec<(u8, u32)>,
}

impl MusicNoteLimits {
    /// `SELECT step, note_length FROM music_note_limits ORDER BY step ASC` (11 rows in the
    /// shipped data: 0 -> 200, 1 -> 400, 2 -> 600, 3 -> 800, 4..10 -> 1000).
    pub fn load(conn: &rusqlite::Connection) -> Result<Self, crate::data::GameDataError> {
        let mut limits = Vec::new();
        crate::data::sqlite::for_each_row(
            conn,
            "SELECT step, note_length FROM music_note_limits ORDER BY step ASC",
            [],
            |r| {
                limits.push((r.u8("step")?, r.u32("note_length")?));
                Ok(())
            },
        )?;
        Ok(Self { limits })
    }

    #[cfg(test)]
    pub(crate) fn from_limits(limits: impl IntoIterator<Item = (u8, u32)>) -> Self {
        Self {
            limits: limits.into_iter().collect(),
        }
    }

    /// The composition limit for an Artistry grade. The table ends at step 10 while an
    /// actability can regrade past it, so a higher grade uses the highest authored limit (the
    /// table saturates at 1000, which is also the UI's hard input cap, `SetMaxTextLength(scoreEdit, 1000)`).
    pub fn limit(&self, step: u8) -> Option<u32> {
        self.limits
            .iter()
            .take_while(|(s, _)| *s <= step)
            .last()
            .map(|(_, limit)| *limit)
    }

    pub fn len(&self) -> usize {
        self.limits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.limits.is_empty()
    }
}

/// `instrument_sounds` item ids (the 1.2 client loads `item_id, midi` for its composition UI,
/// `dll.c:1559064`). The server only needs the set of playable instrument templates: an upload
/// from anything else is refused.
#[derive(Debug, Default)]
pub struct InstrumentSounds {
    ids: HashSet<u32>,
}

impl InstrumentSounds {
    /// `SELECT item_id FROM instrument_sounds` (199 rows in the shipped data).
    pub fn load(conn: &rusqlite::Connection) -> Result<Self, crate::data::GameDataError> {
        let mut ids = HashSet::new();
        crate::data::sqlite::for_each_row(
            conn,
            "SELECT item_id FROM instrument_sounds",
            [],
            |r| {
                ids.insert(r.u32("item_id")?);
                Ok(())
            },
        )?;
        Ok(Self { ids })
    }

    #[cfg(test)]
    pub(crate) fn from_ids(ids: impl IntoIterator<Item = u32>) -> Self {
        Self {
            ids: ids.into_iter().collect(),
        }
    }

    pub fn contains(&self, item_id: u32) -> bool {
        self.ids.contains(&item_id)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// `SongData`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SongData {
    pub id: u32,
    pub author_id: u32,
    pub title: String,
    pub song: String,
    /// Item id of the source item (upload validation only).
    pub source_item_id: u64,
}

/// The inventory side of `CreateSheetMusic` (economy subsystem).
pub trait SheetMusicInventory {
    /// `sourceItem._holdingContainer.OwnerId` and `TemplateId` (`None` = no such item).
    fn source_item(&self, player: &PlayerRef, item_id: u64) -> Option<(u32, u32)>;
    /// `player.Inventory.Bag.FreeSlotCount`.
    fn bag_free_slots(&self, player: &PlayerRef) -> i32;
    /// `ItemManager.Create(Item.SheetMusic, 1, 0, true)` with `OwnerId` / `MadeUnitId` = the
    /// player and `SongId`, then `Bag.AddOrMoveExistingItem(SaveMusicNotes, sheet)`.
    fn add_sheet_music(&self, player: &PlayerRef, song_id: u32) -> bool;
    /// `Bag.ConsumeItem(SaveMusicNotes, templateId, 1, sourceItem)` (consumed count).
    fn consume_source(&self, player: &PlayerRef, item_id: u64, template_id: u32) -> i32;
}

#[derive(Debug, Default)]
struct State {
    /// `_uploadQueue` (player id -> song).
    upload_queue: HashMap<u32, SongData>,
    /// `_allSongs`.
    all_songs: HashMap<u32, SongData>,
    /// `_midiCache` (player id -> MIDI data).
    midi_cache: HashMap<u32, Vec<u8>>,
}

/// `MusicManager`.
#[derive(Debug, Default)]
pub struct MusicManager {
    state: Mutex<State>,
}

impl MusicManager {
    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("music poisoned")
    }

    /// `Load()`: every song of the `music` table.
    pub async fn load(&self, server: &GameServer) -> Result<()> {
        let rows = sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, CAST(author AS SIGNED) AS author, title, song FROM music",
        )
        .fetch_all(&server.db)
        .await?;
        let mut st = self.st();
        st.upload_queue.clear();
        st.all_songs.clear();
        st.midi_cache.clear();
        for r in rows {
            let song = SongData {
                id: r.try_get::<i64, _>("id")? as u32,
                author_id: r.try_get::<i64, _>("author")? as u32,
                title: r.try_get("title")?,
                song: r.try_get("song")?,
                source_item_id: 0,
            };
            // `Dictionary.Add`: ids are the primary key, never duplicated.
            st.all_songs.insert(song.id, song);
        }
        Ok(())
    }

    /// `Save(songData)`: assigns a new id and writes the row.
    pub async fn save(&self, server: &GameServer, song: &mut SongData) -> bool {
        song.id = match server.ids.music.get_next_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("{e}");
                return false;
            }
        };
        let res =
            sqlx::query("REPLACE INTO music (`id`,`author`,`title`,`song` ) VALUES ( ?, ?, ?, ? )")
                .bind(song.id)
                .bind(song.author_id)
                .bind(&song.title)
                .bind(&song.song)
                .execute(&server.db)
                .await;
        if !matches!(res, Ok(ref r) if r.rows_affected() == 1) {
            tracing::warn!("Error saving song to DB for {} ({})", song.title, song.id);
            return false;
        }
        self.st().all_songs.insert(song.id, song.clone());
        true
    }

    /// `UploadSong(charId, title, song, itemId)`.
    pub fn upload_song(&self, char_id: u32, title: &str, song: &str, item_id: u64) {
        let mut st = self.st();
        let q = st.upload_queue.entry(char_id).or_default();
        q.author_id = char_id;
        q.title = title.to_string();
        q.song = song.to_string();
        q.source_item_id = item_id;
    }

    /// The queued upload of a player (tests / economy).
    pub fn queued_upload(&self, char_id: u32) -> Option<SongData> {
        self.st().upload_queue.get(&char_id).cloned()
    }

    /// `CreateSheetMusic(player, sourceItem)`.
    pub async fn create_sheet_music(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        source_item_id: u64,
        inventory: &(dyn SheetMusicInventory + Sync),
    ) -> bool {
        let (pid, name) = {
            let c = player.data();
            (c.id, c.unit.name.clone())
        };
        let source = inventory.source_item(player, source_item_id);
        let Some((_, source_template)) = source.filter(|(owner, _)| *owner == pid) else {
            tracing::warn!("Player {name} ({pid}) does not own the used source item");
            return false;
        };
        let Some(mut sud) = self.queued_upload(pid) else {
            tracing::warn!("Player {name} ({pid}) did not upload any music yet.");
            return false;
        };
        if inventory.bag_free_slots(player) < 1 {
            player.send(SCErrorMsgPacket::new(ERROR_BAG_FULL, 0, true));
            tracing::warn!(
                "Player {name} ({pid}) did not have enough space to aquire new music sheet {} ({})",
                sud.title,
                sud.id
            );
            return false;
        }
        let saved = self.save(server, &mut sud).await;
        // `Save` assigned the id to the queued object itself.
        if let Some(q) = self.st().upload_queue.get_mut(&pid) {
            q.id = sud.id;
        }
        if saved {
            if !inventory.add_sheet_music(player, sud.id) {
                tracing::warn!(
                    "Player {name} ({pid}) had a unknown error when adding Sheet Music to inventory {} ({})",
                    sud.title,
                    sud.id
                );
                return false;
            }
            if inventory.consume_source(player, source_item_id, source_template) <= 0 {
                tracing::warn!(
                    "Failed to consume source item while creating music for Player {name} ({pid}) item {source_item_id} ({source_template})"
                );
            }
        }
        true
    }

    /// `GetSongById(songId)`.
    pub fn get_song_by_id(&self, song_id: u32) -> Option<SongData> {
        self.st().all_songs.get(&song_id).cloned()
    }

    /// `CacheMidi(playerId, midiData)`.
    pub fn cache_midi(&self, player_id: u32, midi: Vec<u8>) {
        self.st().midi_cache.insert(player_id, midi);
    }

    /// `GetMidiCache(playerId)` (empty when nothing is cached).
    pub fn get_midi_cache(&self, player_id: u32) -> Vec<u8> {
        self.st()
            .midi_cache
            .get(&player_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// `SCUserNotesLoadedPacket(songId)` (`SCUserNoteLoadedPacket`): empty body for an unknown song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCUserNoteLoadedPacket {
    pub song: Option<SongData>,
    pub is_tooltip: bool,
}
packet_def!(
    SCUserNoteLoadedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCUserNoteLoadedPacket
);
impl Marshal for SCUserNoteLoadedPacket {
    fn write(&self, s: &mut PacketStream) {
        if let Some(song) = &self.song {
            s.write_u32(song.id)
                .write_bool(self.is_tooltip)
                .write_i32(song.song.encode_utf16().count() as i32)
                .write_str(&song.title)
                .write_str(&format!("{}\0", song.song));
        }
    }
}

/// `CSSaveUserMusicNotesPacket` (file `CSSaveUserMusicNotes.cs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSaveUserMusicNotesPacket {
    pub song_size: u32,
    pub item_id: u64,
    pub value2: u16,
    pub title: String,
    pub song: String,
}

impl ClientPacket for CSSaveUserMusicNotesPacket {
    const NAME: &'static str = "CSSaveUserMusicNotesPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            song_size: s.read_u32()?,
            item_id: s.read_u64()?,
            value2: s.read_u16()?,
            title: s.read_string()?,
            song: s.read_string()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(p) = conn.active_char.clone() else {
                return Ok(());
            };
            // The C# stored anything the client sent. Refuse what the 1.2 client cannot produce:
            // a song longer than the player's `music_note_limits` row for the Artistry grade (the
            // limit the client's SaveNotes enforces before sending, `FUN_39079640` /
            // `FUN_39082130`), a song above the outer `Music.MaxSongChars` sanity bound, or a
            // source item the player does not own. The song grammar stays unparsed (no evidenced
            // server-side format), so a grammar failure can only be refused by the client itself.
            let song_len = self.song.encode_utf16().count();
            let size_mismatch = u32::try_from(song_len) != Ok(self.song_size);
            if size_mismatch {
                // The client writer's size unit is not evidenced (no decompile hit), so a
                // mismatch is logged and accepted instead of dropping the upload.
                tracing::warn!(
                    "CSSaveUserMusicNotesPacket: song_size {} does not match the {song_len} UTF-16 characters (accepted)",
                    self.song_size
                );
            }
            let (artistry_step, composition_limit) = {
                let c = p.data();
                let step = c
                    .actability
                    .actabilities
                    .iter()
                    .find(|a| a.id == ARTISTRY_ACTABILITY_ID)
                    .map_or(0, |a| a.step);
                let limit = server
                    .game_data
                    .systems
                    .services
                    .music_note_limits
                    .limit(step);
                (step, limit)
            };
            let refusal = if let Some(limit) =
                composition_limit.filter(|&limit| song_len as u32 > limit)
            {
                Some(format!(
                    "song has {song_len} UTF-16 characters (Artistry grade {artistry_step} limit {limit})"
                ))
            } else if song_len > server.config.music.max_song_chars {
                Some(format!(
                    "song has {song_len} UTF-16 characters (max {})",
                    server.config.music.max_song_chars
                ))
            } else {
                let inv = crate::systems::economy::inventory::Inv::of(&p);
                // The client sends the id of a bag item it found by its 64-bit id
                // (`sub_3961E9A0` in `sub_39079640` / `sub_390799E0`), which is the music paper
                // the later item cast consumes, not an instrument: only ownership is checked.
                match inv.item_by_id(self.item_id) {
                    None => Some(format!("item {} is not owned", self.item_id)),
                    Some(_) => None,
                }
            };
            if let Some(reason) = refusal {
                tracing::warn!(
                    "CSSaveUserMusicNotesPacket from {} refused: {reason}",
                    p.data().unit.name
                );
                // The 1.2 client's own save refusal is `ErrorMessageType.UserNoteCannotSave`
                // (0x2aa, `user_note_cannot_save`, the only error `SaveNotes` shows).
                p.send(SCErrorMsgPacket::new(USER_NOTE_CANNOT_SAVE, 0, true));
                return Ok(());
            }
            server.systems.services.music.upload_song(
                p.character_id,
                &self.title,
                &self.song,
                self.item_id,
            );
            tracing::debug!(
                "Uploading song, title: {}, songsize: {}, value2:{}",
                self.title,
                self.song_size,
                self.value2
            );
            tracing::trace!("Song data: {}", self.song);
            Ok(())
        })
    }
}

/// `CSSendUserMusicPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSendUserMusicPacket {
    pub song_size: u32,
    pub data: Vec<u8>,
}

impl ClientPacket for CSSendUserMusicPacket {
    const NAME: &'static str = "CSSendUserMusicPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let song_size = s.read_u32()?;
        let block = s.read_u16()?;
        Ok(Self {
            song_size,
            data: s.read_bytes(usize::from(block))?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!(
                "Caching MIDI data size: {}/{}",
                self.data.len(),
                self.song_size
            );
            if let Some(p) = &conn.active_char {
                server
                    .systems
                    .services
                    .music
                    .cache_midi(p.character_id, self.data);
            }
            Ok(())
        })
    }
}

/// `CSRequestMusicNotesPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSRequestMusicNotesPacket {
    pub id64: u64,
    pub is_tooltip: bool,
    pub show_buff_only: bool,
}

impl ClientPacket for CSRequestMusicNotesPacket {
    const NAME: &'static str = "CSRequestMusicNotesPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            id64: s.read_u64()?,
            // Native 1.2 constructor 397acd70 / serializer 397b1480: two bool bytes.
            is_tooltip: s.read_bool()?,
            show_buff_only: s.read_bool()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!(
                "Request Song, id: {}, isTooltip:{}, showBuffOnly:{}",
                self.id64,
                self.is_tooltip,
                self.show_buff_only
            );
            let Some(p) = conn.active_char.clone() else {
                return Ok(());
            };
            // Native 39082550 sends (isTooltip, false) on a cache miss and
            // (false, true) for cached playing. The latter only needs the buff.
            if self.show_buff_only {
                let me = WorldObject::from(&p);
                crate::systems::skills::buffs::add_buff_by_id(
                    server,
                    &me,
                    BUFF_SCORE_MEMORIZED,
                    &me,
                    0,
                );
            } else {
                // Keep the C# low-32-bit lookup: native SC1e0 reads signed32,
                // sign-extended into the cache key at 391e1691/391e169b.
                let song = server
                    .systems
                    .services
                    .music
                    .get_song_by_id(self.id64 as u32);
                p.send(SCUserNoteLoadedPacket {
                    song,
                    is_tooltip: self.is_tooltip,
                });
            }
            Ok(())
        })
    }
}

/// `CSEndMusicPacket`: ends the play-song buffs once the score memorization is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSEndMusicPacket;

impl ClientPacket for CSEndMusicPacket {
    const NAME: &'static str = "CSEndMusicPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!("CSEndMusicPacket");
            let Some(p) = conn.active_char.clone() else {
                return Ok(());
            };
            let me = WorldObject::from(&p);
            if me.with_base_unit(|b| b.buffs().check_buff(BUFF_SCORE_MEMORIZED)) {
                return Ok(());
            }
            let ids = server
                .game_data
                .systems
                .skills
                .buffs_by_tag(TAG_PLAY_SONG)
                .map(<[u32]>::to_vec)
                .unwrap_or_default();
            for id in ids {
                if me.with_base_unit(|b| b.buffs().check_buff(id)) {
                    crate::systems::skills::buffs::remove_buff(server, &me, id);
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "music_tests.rs"]
mod music_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::packet::encode;

    #[test]
    fn note_loaded_packet() {
        let song = SongData {
            id: 7,
            author_id: 1,
            title: "T".into(),
            song: "abc".into(),
            source_item_id: 0,
        };
        let mut e = PacketStream::new();
        e.write_u32(7)
            .write_u8(1)
            .write_i32(3)
            .write_str("T")
            .write_str("abc\0");
        assert_eq!(
            encode(&SCUserNoteLoadedPacket {
                song: Some(song),
                is_tooltip: true,
            })[6..]
                .to_vec(),
            e.into_bytes()
        );
        assert!(encode(&SCUserNoteLoadedPacket {
            song: None,
            is_tooltip: true,
        })[6..]
            .is_empty());
    }

    #[test]
    fn queue_and_cache() {
        let m = MusicManager::default();
        assert!(m.get_midi_cache(1).is_empty());
        m.cache_midi(1, vec![1, 2]);
        m.cache_midi(1, vec![3]);
        assert_eq!(m.get_midi_cache(1), vec![3]);
        m.upload_song(5, "a", "b", 9);
        m.upload_song(5, "c", "d", 10);
        let q = m.queued_upload(5).unwrap();
        assert_eq!(
            (q.author_id, q.title.as_str(), q.source_item_id),
            (5, "c", 10)
        );
        assert!(m.get_song_by_id(1).is_none());
    }

    #[test]
    fn client_packets_decode() {
        let mut s = PacketStream::new();
        s.write_u32(3)
            .write_u64(77)
            .write_u16(5)
            .write_str("t")
            .write_str("s");
        let p = CSSaveUserMusicNotesPacket::read(&mut PacketStream::from_bytes(s.into_bytes()))
            .unwrap();
        assert_eq!(
            (p.item_id, p.title.as_str(), p.song.as_str()),
            (77, "t", "s")
        );
        let mut s = PacketStream::new();
        s.write_u32(10).write_u16(2).write_u8(1).write_u8(2);
        let p = CSSendUserMusicPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
        assert_eq!(p.data, vec![1, 2]);
        let mut s = PacketStream::new();
        s.write_u64(5).write_u16(256);
        let p =
            CSRequestMusicNotesPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
        assert_eq!((p.id64, p.is_tooltip, p.show_buff_only), (5, false, true));
    }
}
