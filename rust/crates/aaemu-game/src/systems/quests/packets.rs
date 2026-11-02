//! Quest packets: `CSStartQuestContextPacket`, `CSCompleteQuestContextPacket`,
//! `CSDropQuestContextPacket`, `CSQuestTalkMadePacket`, `CSQuestStartWithPacket`,
//! `CSTryQuestCompleteAsLetItDonePacket`, `CSRestartMainQuestPacket`, `CSSaveTutorialPacket`,
//! `CSRequestPermissionToPlayCinemaForDirectingMode`, `CSStartedCinemaPacket`,
//! `CSCompletedCinemaPacket` and the quest S->C packets.

use aaemu_commons::{Marshal, PacketStream};
use futures::future::BoxFuture;

use super::events;
use crate::error::Result;
use crate::models::char_data::ActiveQuest;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::server::GameServer;

// ----- S->C -----

/// `SCQuestContextStartedPacket(quest, componentId)`.
#[derive(Debug, Clone)]
pub struct SCQuestContextStartedPacket {
    pub quest: ActiveQuest,
    pub component_id: u32,
    /// `DateTime.UtcNow` at write time (unix ms) for `Quest.Write`.
    pub now_ms: i64,
}
packet_def!(
    SCQuestContextStartedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestContextStartedPacket
);
impl Marshal for SCQuestContextStartedPacket {
    fn write(&self, s: &mut PacketStream) {
        self.quest.write(s, self.now_ms);
        s.write_u32(self.component_id);
    }
}

/// `SCQuestContextUpdatedPacket(quest, componentId, para1..4)`.
#[derive(Debug, Clone)]
pub struct SCQuestContextUpdatedPacket {
    pub quest: ActiveQuest,
    pub component_id: u32,
    pub para: [i32; 4],
    pub now_ms: i64,
}
impl SCQuestContextUpdatedPacket {
    pub fn new(quest: ActiveQuest, component_id: u32, now_ms: i64) -> Self {
        Self {
            quest,
            component_id,
            para: [0; 4],
            now_ms,
        }
    }
}
packet_def!(
    SCQuestContextUpdatedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestContextUpdatedPacket
);
impl Marshal for SCQuestContextUpdatedPacket {
    fn write(&self, s: &mut PacketStream) {
        self.quest.write(s, self.now_ms);
        s.write_u32(self.component_id);
        for p in self.para {
            s.write_i32(p);
        }
    }
}

/// `SCQuestContextCompletedPacket(questId, body, componentId)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCQuestContextCompletedPacket {
    pub quest_id: u32,
    pub body: [u8; 8],
    pub component_id: u32,
}
packet_def!(
    SCQuestContextCompletedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestContextCompletedPacket
);
impl Marshal for SCQuestContextCompletedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.quest_id)
            .write_raw(&self.body)
            .write_u32(self.component_id);
    }
}

/// `SCQuestContextResetPacket(questId, body, componentId)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCQuestContextResetPacket {
    pub quest_id: u32,
    pub body: [u8; 8],
    pub component_id: u32,
}
packet_def!(
    SCQuestContextResetPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestContextResetPacket
);
impl Marshal for SCQuestContextResetPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.quest_id)
            .write_raw(&self.body)
            .write_u32(self.component_id);
    }
}

/// Reason bytes of `SCQuestContextFailedPacket` (`0x146`).
///
/// The client raises `QUEST_ERROR_INFO` with the byte and shows
/// `locale.questContext.errors[err]`. `x2ui/baselib/locale_helper.alb` builds that table as a
/// 1-based Lua list (`SETLIST` at instruction 654; value 1 is `already_have`, 2
/// `invalid_quest`, ...), and `quest_context.alb` proto 22 (the `QUEST_ERROR_INFO` handler)
/// indexes it directly with its first argument. The client's own pre-checks use the same
/// codes: `FUN_3937a2d0` sends 2 when the quest is invalid and 0x14 after its can-interact
/// check fails.
pub mod quest_error {
    pub const INVALID_QUEST: u8 = 2;
    pub const INVALID_NPC_OR_QUEST: u8 = 7;
    pub const INVALID_QUEST_STATUS: u8 = 12;
    pub const INVALID_DOODAD: u8 = 19;
}

/// `SCQuestContextFailedPacket(questId, err)` (`0x146`). The client's reader
/// `FUN_397b5c90` (vtable `399b3a5c`) reads the field `"type"` as a u32 at `+0xc` and
/// `"err"` as one byte at `+0x10`; `OnQuestContextFailed` (`FUN_391dbcd0`) forwards both
/// to `FUN_3937a130`, which raises `QUEST_ERROR_INFO` and ends quest directing mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCQuestContextFailedPacket {
    pub quest_id: u32,
    pub err: u8,
}
packet_def!(
    SCQuestContextFailedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestContextFailedPacket
);
impl Marshal for SCQuestContextFailedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.quest_id).write_u8(self.err);
    }
}

/// `SCQuestRewardedByMailPacket(questList)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCQuestRewardedByMailPacket {
    pub quests: Vec<u32>,
}
packet_def!(
    SCQuestRewardedByMailPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestRewardedByMailPacket
);
impl Marshal for SCQuestRewardedByMailPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.quests.len() as u8);
        for q in &self.quests {
            s.write_u32(*q);
        }
    }
}

/// `SCDoodadQuestAcceptPacket(doodadObjId, questContextId)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCDoodadQuestAcceptPacket {
    pub doodad_obj_id: u32,
    pub quest_id: u32,
}
packet_def!(
    SCDoodadQuestAcceptPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadQuestAcceptPacket
);
impl Marshal for SCDoodadQuestAcceptPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.doodad_obj_id).write_u32(self.quest_id);
    }
}

/// `SCTutorialSavedPacket(id, body)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCTutorialSavedPacket {
    pub id: u32,
    pub body: [u8; 8],
}
packet_def!(
    SCTutorialSavedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCTutorialSavedPacket
);
impl Marshal for SCTutorialSavedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id).write_raw(&self.body);
    }
}

/// `SCAppellationGainedPacket(appellationId)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCAppellationGainedPacket {
    pub appellation_id: u32,
}
packet_def!(
    SCAppellationGainedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCAppellationGainedPacket
);
impl Marshal for SCAppellationGainedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.appellation_id);
    }
}

// ----- C->S -----

fn active(conn: &GameConnection) -> Option<crate::world::PlayerRef> {
    conn.active_char.clone()
}

/// `CSStartQuestContextPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSStartQuestContextPacket {
    pub quest_context_id: u32,
    pub npc_obj_id: u32,
    pub doodad_obj_id: u32,
    pub sphere_id: u32,
}

impl ClientPacket for CSStartQuestContextPacket {
    const NAME: &'static str = "CSStartQuestContextPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            quest_context_id: s.read_u32()?,
            npc_obj_id: s.read_bc()?,
            doodad_obj_id: s.read_bc()?,
            sphere_id: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(p) = active(conn) else {
                return Ok(());
            };
            if self.npc_obj_id > 0 {
                events::add_quest_from_npc(server, &p, self.quest_context_id, self.npc_obj_id);
            } else if self.doodad_obj_id > 0 {
                events::add_quest_from_doodad(
                    server,
                    &p,
                    self.quest_context_id,
                    self.doodad_obj_id,
                );
            } else if self.sphere_id > 0 {
                events::add_quest_from_sphere(server, &p, self.quest_context_id, self.sphere_id);
            } else {
                events::add_quest(server, &p, self.quest_context_id, false);
            }
            Ok(())
        })
    }
}

/// `CSCompleteQuestContextPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCompleteQuestContextPacket {
    pub quest_context_id: u32,
    pub npc_obj_id: u32,
    pub doodad_obj_id: u32,
    pub selected: i32,
}

impl ClientPacket for CSCompleteQuestContextPacket {
    const NAME: &'static str = "CSCompleteQuestContextPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            quest_context_id: s.read_u32()?,
            npc_obj_id: s.read_bc()?,
            doodad_obj_id: s.read_bc()?,
            selected: s.read_i32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(p) = active(conn) {
                tracing::info!(
                    "CompleteQuestContext, {} ({}), Quest {}, NpcObjId {}, DoodadObjId {}, Selected {}",
                    p.data().unit.name,
                    p.character_id,
                    self.quest_context_id,
                    self.npc_obj_id,
                    self.doodad_obj_id,
                    self.selected
                );
                events::on_report(
                    server,
                    &p,
                    self.quest_context_id,
                    self.npc_obj_id,
                    self.doodad_obj_id,
                    self.selected,
                );
            }
            Ok(())
        })
    }
}

/// `CSDropQuestContextPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSDropQuestContextPacket {
    pub quest_id: u32,
}

impl ClientPacket for CSDropQuestContextPacket {
    const NAME: &'static str = "CSDropQuestContextPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            quest_id: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(p) = active(conn) {
                events::drop_quest(server, &p, self.quest_id, true);
            }
            Ok(())
        })
    }
}

/// `CSQuestTalkMadePacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSQuestTalkMadePacket {
    pub npc_obj_id: u32,
    pub quest_context_id: u32,
    pub quest_comp_id: u32,
    pub quest_act_id: u32,
}

impl ClientPacket for CSQuestTalkMadePacket {
    const NAME: &'static str = "CSQuestTalkMadePacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            npc_obj_id: s.read_bc()?,
            quest_context_id: s.read_u32()?,
            quest_comp_id: s.read_u32()?,
            quest_act_id: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!(
                "QuestTalkMade: npcObjId {}, questContextId {}, questCompId {}, questActId {}",
                self.npc_obj_id,
                self.quest_context_id,
                self.quest_comp_id,
                self.quest_act_id
            );
            if let Some(p) = active(conn) {
                events::on_talk_made(
                    server,
                    &p,
                    &p,
                    self.npc_obj_id,
                    self.quest_context_id,
                    self.quest_comp_id,
                    self.quest_act_id,
                );
            }
            Ok(())
        })
    }
}

/// `CSQuestStartWithPacket` (logged only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSQuestStartWithPacket {
    pub unk_id: u32,
    pub unk_id2: u32,
    pub unk_id3: u32,
}

impl ClientPacket for CSQuestStartWithPacket {
    const NAME: &'static str = "CSQuestStartWithPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            unk_id: s.read_u32()?,
            unk_id2: s.read_u32()?,
            unk_id3: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        _server: &'a GameServer,
        _conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!(
                "QuestStartWith, UnkId: {}, UnkId2: {}, UnkId3: {}",
                self.unk_id,
                self.unk_id2,
                self.unk_id3
            );
            Ok(())
        })
    }
}

/// `CSTryQuestCompleteAsLetItDonePacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSTryQuestCompleteAsLetItDonePacket {
    pub id: u32,
    pub obj_id: u32,
    pub selected: i32,
}

impl ClientPacket for CSTryQuestCompleteAsLetItDonePacket {
    const NAME: &'static str = "CSTryQuestCompleteAsLetItDonePacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            id: s.read_u32()?,
            obj_id: s.read_bc()?,
            selected: s.read_i32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!(
                "TryQuestCompleteAsLetItDone, Id: {}, ObjId: {}, Selected: {}",
                self.id,
                self.obj_id,
                self.selected
            );
            let Some(p) = active(conn) else {
                return Ok(());
            };
            // Check if player is actually targeting the NPC
            let target = p.data().unit.current_target;
            if self.obj_id > 0 && target != 0 && target != self.obj_id {
                tracing::warn!(
                    "TryQuestCompleteAsLetItDone refused, {} ({}), Quest {}: the target {} is not the NPC {} the client reported to",
                    p.data().unit.name,
                    p.character_id,
                    self.id,
                    target,
                    self.obj_id
                );
                p.send(SCQuestContextFailedPacket {
                    quest_id: self.id,
                    err: quest_error::INVALID_NPC_OR_QUEST,
                });
                return Ok(());
            }
            let (id, selected) = (self.id, self.selected);
            events::with_quests(server, &p, None, move |ctx| {
                ctx.try_complete_quest_as_let_it_done(id, selected)
            });
            Ok(())
        })
    }
}

/// `CSRestartMainQuestPacket` (logged only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSRestartMainQuestPacket {
    pub id: u32,
}

impl ClientPacket for CSRestartMainQuestPacket {
    const NAME: &'static str = "CSRestartMainQuestPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self { id: s.read_u32()? })
    }

    fn handle<'a>(
        self,
        _server: &'a GameServer,
        _conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!("RestartMainQuest, Id: {}", self.id);
            Ok(())
        })
    }
}

/// At most this many tutorial entries may be marked completed through `CSSaveTutorialPacket`.
/// The 1.2 data ships 101 (`quest_contexts.category_id = 45`), so this bounds a modified
/// client's growth of the completed-quest set.
pub(crate) const MAX_TUTORIAL_ENTRIES: usize = 256;

/// `CSSaveTutorialPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSaveTutorialPacket {
    pub id: u32,
}

impl ClientPacket for CSSaveTutorialPacket {
    const NAME: &'static str = "CSSaveTutorialPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self { id: s.read_u32()? })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!("SaveTutorial, Id: {}", self.id);
            let Some(p) = active(conn) else {
                return Ok(());
            };
            if !server.game_data.systems.quests.is_tutorial(self.id) {
                tracing::warn!(
                    "SaveTutorial refused, {} ({}): {} is not a tutorial entry",
                    p.data().unit.name,
                    p.character_id,
                    self.id
                );
                return Ok(());
            }
            let block = {
                let mut c = p.data();
                let already = super::engine::has_quest_completed(&c.quests.completed, self.id);
                if !already {
                    let completed = server
                        .game_data
                        .systems
                        .quests
                        .tutorial_ids
                        .iter()
                        .filter(|id| super::engine::has_quest_completed(&c.quests.completed, **id))
                        .count();
                    if completed >= MAX_TUTORIAL_ENTRIES {
                        drop(c);
                        tracing::warn!(
                            "SaveTutorial refused, {} ({}): {MAX_TUTORIAL_ENTRIES} tutorial entries are already completed",
                            p.data().unit.name,
                            p.character_id
                        );
                        return Ok(());
                    }
                }
                super::engine::set_completed_quest_flag(&mut c.quests.completed, self.id, true)
            };
            conn.send(&SCTutorialSavedPacket {
                id: self.id,
                body: block.body8(),
            });
            Ok(())
        })
    }
}

/// `CSRequestPermissionToPlayCinemaForDirectingMode`: the 1.2 client sends it before it plays
/// the cinema of a quest component it just accepted / turned in / talked about, and never waits
/// for an answer. The `u32` is the quest **component** id (not the quest context id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSRequestPermissionToPlayCinemaForDirectingMode {
    pub component_id: u32,
    pub npc_obj_id: u32,
    pub doodad_obj_id: u32,
}

impl ClientPacket for CSRequestPermissionToPlayCinemaForDirectingMode {
    const NAME: &'static str = "CSRequestPermissionToPlayCinemaForDirectingMode";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            component_id: s.read_u32()?,
            npc_obj_id: s.read_bc()?,
            doodad_obj_id: s.read_bc()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(p) = active(conn) else {
                return Ok(());
            };
            let data = &server.game_data.systems.quests;
            let quest_id = data.component(self.component_id).map_or(0, |c| c.quest_id);
            let cinema_id = data.component_cinema_id(self.component_id);
            tracing::info!(
                "RequestPermissionToPlayCinema, {} ({}), Quest {}, Component {}, Cinema {}, NpcObjId {}, DoodadObjId {}",
                p.data().unit.name,
                p.character_id,
                quest_id,
                self.component_id,
                cinema_id.map_or("none".to_string(), |id| data.cinema_label(id)),
                self.npc_obj_id,
                self.doodad_obj_id
            );
            events::request_cinema(server, &p, cinema_id.unwrap_or(0));
            Ok(())
        })
    }
}

/// `CSStartedCinemaPacket` (empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSStartedCinemaPacket;

impl ClientPacket for CSStartedCinemaPacket {
    const NAME: &'static str = "CSStartedCinemaPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(p) = active(conn) {
                events::on_cinema_started(server, &p);
            }
            Ok(())
        })
    }
}

/// `CSCompletedCinemaPacket` (empty): restores the neighbouring objects a cut scene removed
/// from the client, then raises the quest events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCompletedCinemaPacket;

impl ClientPacket for CSCompletedCinemaPacket {
    const NAME: &'static str = "CSCompletedCinemaPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(p) = active(conn) else {
                return Ok(());
            };
            let data = &server.game_data.systems.quests;
            let end = events::cinema_ending(&p);
            let cinema = data.cinema(end.cinema_id);
            // Only a CUT_SCENE sequence wipes the client's units and doodads. A cinema that is
            // unknown (no request seen, no client file) is treated as one: an empty world after
            // a cut scene is worse than a redundant re-send.
            let resend = cinema.and_then(|c| c.cut_scene) != Some(false);
            tracing::info!(
                "CompletedCinema, {} ({}), Cinema {}, played {}, objects re-sent: {}",
                p.data().unit.name,
                p.character_id,
                data.cinema_label(end.cinema_id),
                end.played_ms
                    .map_or("? ms".to_string(), |ms| format!("{ms} ms")),
                resend
            );
            if resend {
                // The cut-scene wipe also clears the client's own-mate records; the mates
                // re-created below need theirs first (a rider attached to a mate without a
                // record crashes the client).
                crate::systems::vehicles::mates::resend_owner_mates_spawned(server, &p);
                crate::systems::npcs::portal::resend_visible_objects_to_character(server, &p);
            }
            events::on_cinema_ended(server, &p);
            Ok(())
        })
    }
}
