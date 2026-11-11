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

/// `SCQuestUnitReqFailedPacket(quest, unitObjId, kind, messageId, value)` (`0x148`): the start
/// of the quest was refused by a `unit_reqs` row. The 1.2 client reader `FUN_397b5d00` reads a
/// `u32` quest, a 3-byte `bc` unit object id and a flag bag (`FUN_397b1ef0`): one flag byte,
/// then only the set parts in that order -- bit 0x01 `u8` kind, bit 0x02 `u16` message id, bit
/// 0x04 `u32` value. A zero part is not written, so an all-zero bag is the single byte `00`.
/// Handler `FUN_391dbdd0` -> `FUN_39376590(quest, unitObjId, kind | messageId << 16, value)`
/// acts only when `unitObjId` is the local player, builds the "unit requirement failed" text
/// (`FUN_39390900`: the system message id when the message id is set, otherwise text by kind
/// and value -- kind 0x16 `NO_BUFF` prints the buff name of value, kind 0x24
/// `EXCEPT_COMPLETE_QUEST_CONTEXT` the quest name) and raises `QUEST_QUICK_CLOSE_EVENT` (0xf4,
/// closes the quest directing dialog). INFERRED: `kind` = `unit_reqs.kind_id`, `message_id` = 0
/// (`unit_reqs` has no message column) and `value` = its `value1` (`ui-quests.md` 1.1,
/// CAP-QU-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCQuestUnitReqFailedPacket {
    pub quest_id: u32,
    pub unit_obj_id: u32,
    pub kind: u8,
    pub message_id: u16,
    pub value: u32,
}
packet_def!(
    SCQuestUnitReqFailedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestUnitReqFailedPacket
);
impl Marshal for SCQuestUnitReqFailedPacket {
    fn write(&self, s: &mut PacketStream) {
        let mut flags = 0u8;
        if self.kind != 0 {
            flags |= 0x01;
        }
        if self.message_id != 0 {
            flags |= 0x02;
        }
        if self.value != 0 {
            flags |= 0x04;
        }
        s.write_u32(self.quest_id)
            .write_bc(self.unit_obj_id)
            .write_u8(flags);
        if self.kind != 0 {
            s.write_u8(self.kind);
        }
        if self.message_id != 0 {
            s.write_u16(self.message_id);
        }
        if self.value != 0 {
            s.write_u32(self.value);
        }
    }
}

/// `SCQuestRewardedByMailPacket(success, questId)` (`0x14e`): the reward was sent by mail
/// because it did not fit in the bag. The client reader `FUN_397b5ec0` (class registered with
/// opcode 0x14e, vtable `399b3aa4`) reads `success` as a bool at body `+0xc` (vtable slot
/// `0x78`, the named bool reader) and then the quest id as a `u32` at `+0x10` (slot `0x3c`).
/// `OnQuestRewardMailSent` (`FUN_391dc150`) calls `FUN_3937a520`: on success it looks the
/// quest up (`FUN_39856420`) and raises `QUEST_REWARDED_BY_MAIL`; when the flag is false it
/// raises quest error 0xb (`cant_supply_rewards`) instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCQuestRewardedByMailPacket {
    pub success: bool,
    pub quest_id: u32,
}
packet_def!(
    SCQuestRewardedByMailPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestRewardedByMailPacket
);
impl Marshal for SCQuestRewardedByMailPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.success).write_u32(self.quest_id);
    }
}

/// `SCQuestMailSentPacket(id)` (`0x150`): a quest mail was sent to the player. The 1.2 client
/// reader `FUN_397b5f00` (wrapper `FUN_391b19b0`, which assigns `[1] = 0x150`) reads exactly one
/// `u32` through vtable slot `0x3c` at body `+0xc`; there is no sentinel field. The meaning of
/// the value as a `quest_mails.id` is inferred from the table, and no server trigger sends this
/// packet yet: which quest step mails what is capture-pending (case `QUEST-MAIL-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCQuestMailSentPacket {
    pub quest_mail_id: u32,
}
packet_def!(
    SCQuestMailSentPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCQuestMailSentPacket
);
impl Marshal for SCQuestMailSentPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.quest_mail_id);
    }
}

/// `SCAreaTeamMessagePacket(enter, sphereId)` (`0x155`): a player entered / left an area
/// sphere; the client shows the sphere's `team_msg`. The 1.2 client reader `FUN_397b63b0`
/// reads a bool at body `+0xc` and a `u32` at `+0x10`; the handler `FUN_391dc460` looks the
/// sphere up in its own `spheres` table and raises event `0xfd` with `team_msg`. The trigger
/// and the audience are inferred (CAP-UI-6 / CAP-UI-7, see `spheres.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCAreaTeamMessagePacket {
    pub enter: bool,
    pub sphere_id: u32,
}
packet_def!(
    SCAreaTeamMessagePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCAreaTeamMessagePacket
);
impl Marshal for SCAreaTeamMessagePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.enter).write_u32(self.sphere_id);
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

/// `SCDoodadCompleteQuestPacket(doodadObjId, questContextId)` (`0x14d`): the quest can be
/// handed in at this doodad. The 1.2 client reader `FUN_397b5e50` reads the doodad object id
/// as a 3-byte `bc` at body `+0xc` and the quest id as a `u32` at `+0x10`;
/// `OnDoodadCompleteQuest` (`FUN_391dc050`) raises `COMPLETE_QUEST_CONTEXT_DOODAD`, after
/// which the client sends `CSCompleteQuestContextPacket` with this doodad object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCDoodadCompleteQuestPacket {
    pub doodad_obj_id: u32,
    pub quest_id: u32,
}
packet_def!(
    SCDoodadCompleteQuestPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadCompleteQuestPacket
);
impl Marshal for SCDoodadCompleteQuestPacket {
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
            events::start_quest_context(
                server,
                &p,
                self.quest_context_id,
                self.npc_obj_id,
                self.doodad_obj_id,
                self.sphere_id,
            );
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

/// The typed tail of [`CSQuestStartWithPacket`] (`FUN_397c5450` + `FUN_397be540`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestStartWithTail {
    /// Type 1: six `u32` (meaning unknown).
    SixU32([u32; 6]),
    /// Type 2: the player level that was reached (`QuestActConAcceptLevelUp`).
    Level(u8),
    /// Type 3: two `u32` (meaning unknown).
    TwoU32([u32; 2]),
    /// Type 4: one `u32` (meaning unknown).
    OneU32(u32),
    /// An unknown type: the 1.2 client sends only types 1-4, so no tail is read.
    Unknown,
}

/// `CSQuestStartWithPacket` (logged only at debug): `u32 quest, u32 component, u32 act,
/// u8 type` and a typed tail. The 1.2 client writer `FUN_397c5450` / `FUN_397be540` has only
/// two send sites: type 2 = a player level reached (`FUN_39378660`, a
/// `QuestActConAcceptLevelUp` start) and type 3 = an NPC emotion
/// (`FUN_39378930`); types 1 / 4 are never sent. The server starts those quests itself, so the
/// packet needs no action; see `PORTING.md` ("Gap audit verdicts (2026-11-07)").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSQuestStartWithPacket {
    pub quest: u32,
    pub component: u32,
    pub act: u32,
    pub kind: u8,
    pub tail: QuestStartWithTail,
}

impl ClientPacket for CSQuestStartWithPacket {
    const NAME: &'static str = "CSQuestStartWithPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let quest = s.read_u32()?;
        let component = s.read_u32()?;
        let act = s.read_u32()?;
        let kind = s.read_u8()?;
        let tail = match kind {
            1 => {
                let mut v = [0u32; 6];
                for x in &mut v {
                    *x = s.read_u32()?;
                }
                QuestStartWithTail::SixU32(v)
            }
            2 => QuestStartWithTail::Level(s.read_u8()?),
            3 => QuestStartWithTail::TwoU32([s.read_u32()?, s.read_u32()?]),
            4 => QuestStartWithTail::OneU32(s.read_u32()?),
            _ => QuestStartWithTail::Unknown,
        };
        Ok(Self {
            quest,
            component,
            act,
            kind,
            tail,
        })
    }

    fn handle<'a>(
        self,
        _server: &'a GameServer,
        _conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!(
                "QuestStartWith, Quest: {}, Component: {}, Act: {}, Type: {}, Tail: {:?}",
                self.quest,
                self.component,
                self.act,
                self.kind,
                self.tail
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

/// `CSRestartMainQuestPacket` (0x0fd): the player asked to restart quest `id` (the template
/// id). Handled by [`Ctx::try_restart_quest`](super::engine::Ctx::try_restart_quest).
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
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(p) = active(conn) {
                tracing::info!(
                    "RestartMainQuest, {} ({}), Quest {}",
                    p.data().unit.name,
                    p.character_id,
                    self.id
                );
                let id = self.id;
                events::with_quests(server, &p, None, move |ctx| ctx.try_restart_quest(id));
            }
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
                // A rider leaves the cut scene on foot (Rust addition). Re-attaching it to the
                // re-created mate left the 1.2 client unable to move or dismount (tester report:
                // quest 1352 "Another One Down", cinema 147 `zq_09_helmire`, accepted from
                // Pederson while mounted) while the server still ignored the rider's own moves;
                // only a desummon recovered. The detach names only the rider, which the wipe
                // keeps, and the repaint below then shows the mate without a passenger.
                crate::systems::vehicles::force_dismount(
                    server,
                    &p,
                    crate::systems::vehicles::slaves::attach_reason::PREFAB_CHANGED,
                );
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
