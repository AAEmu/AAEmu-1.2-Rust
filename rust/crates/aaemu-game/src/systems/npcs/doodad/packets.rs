//! Doodad packets (`Core/Packets/G2C/SCDoodad*`, `SCBondDoodadPacket`, `SCUnbondDoodadPacket`,
//! and the doodad C->S packets of `Core/Packets/C2G`).
//!
//! `SCDoodadCreated/sCreated/Removed/sRemoved` live in `packets::world_objects` (visibility).
//! `CSSaveDoodadUccStringPacket` is decoded but not registered: the 1.2 C# does not register it
//! (`0x0e3` is commented out, the opcode table has no value for it).
//! `CSLootOpenBagPacket` belongs to the loot code (economy); its doodad part is
//! [`super::manager::loot_open_bag_doodad`].

use aaemu_commons::{helpers, Marshal, PacketStream};
use futures::future::BoxFuture;

use crate::error::Result;
use crate::models::doodad::{BondDoodad, Doodad};
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::{ClientPacket, PacketRegistry};
use crate::opcodes::{cs, sc};
use crate::packets::packet_def;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::world::WorldObject;

/// `ErrorMessageType` values used by the doodad code.
pub mod error_message {
    pub const NO_PERM: i16 = 2;
    pub const NO_INTERACTION_AVAILABLE: i16 = 3;
    pub const LABOR_POWER_NEEDED: i16 = 9;
    pub const TOO_FAR_AWAY: i16 = 154;
    /// Server answer for a seed use that cannot start (missing / foreign item, unknown template).
    /// **Inference:** the 1.2 client shows `failed_to_use_item` when a seed use is cancelled, but
    /// no capture of a server-to-client answer for this case exists.
    pub const FAILED_TO_USE_ITEM: i16 = 207;
    pub const INTERACTION_PERMISSION_DENY: i16 = 232;
    pub const DOODAD_SIM_AREA: i16 = 276;
    pub const DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD: i16 = 277;
    pub const DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT: i16 = 278;
    /// The 1.2 territory-statue placement refusal (`locate_invalid_dominion_area`), same value
    /// as `systems::economy::error_message::LOCATE_INVALID_DOMINION_AREA`.
    pub const LOCATE_INVALID_DOMINION_AREA: i16 = 749;
    pub const FAMILY_NOT_EXIST: i16 = 376;
    pub const ONLY_EXPEDITION_MEMBER: i16 = 808;
}

/// `Character.SendErrorMessage(type)` (`type2 = 0`, `isNotify = true`).
pub fn error_msg(error: i16) -> SCErrorMsgPacket {
    SCErrorMsgPacket::new(error, 0, true)
}

// ---------------------------------------------------------------------------------------------
// S->C
// ---------------------------------------------------------------------------------------------

/// `SCDoodadPhaseChangedPacket(doodad)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCDoodadPhaseChangedPacket {
    pub obj_id: u32,
    pub func_group_id: u32,
    pub time_left: u32,
    pub item_template_id: u32,
}
impl SCDoodadPhaseChangedPacket {
    /// Built from the doodad's state (take it while holding the doodad's lock).
    pub fn new(obj_id: u32, d: &Doodad, now_ms: i64) -> Self {
        Self {
            obj_id,
            func_group_id: d.func_group_id,
            time_left: d.time_left(now_ms),
            item_template_id: d.item_template_id,
        }
    }
}
packet_def!(
    SCDoodadPhaseChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadPhaseChangedPacket
);
impl Marshal for SCDoodadPhaseChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id)
            .write_u32(self.func_group_id)
            .write_u32(self.time_left) // growing
            .write_i32(-1) // puzzleGroup
            .write_u32(self.item_template_id);
    }
}

/// `SCDoodadPhaseMsgPacket(doodad, funcGroupId)`: the message of a func group that authors a
/// `phase_msg`. The 1.2 reader `FUN_397b6460` is `bc doodadObjId ; u32 funcGroupId`; the handler
/// `FUN_391dc720` -> `FUN_3902e0f0` looks the group up in the client's `doodad_func_groups`,
/// requires the doodad to exist locally unless `is_msg_to_zone` is true, and raises
/// `DOODAD_PHASE_MSG` (event 0xfd, a chat / center message) when `phase_msg` is not empty.
/// The C# had only the opcode, no sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCDoodadPhaseMsgPacket {
    pub doodad_obj_id: u32,
    pub func_group_id: u32,
}
packet_def!(
    SCDoodadPhaseMsgPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadPhaseMsgPacket
);
impl Marshal for SCDoodadPhaseMsgPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.doodad_obj_id).write_u32(self.func_group_id);
    }
}

/// `SCWorldInteractionCanceledPacket(interactedUnitId, interactedDoodadId)`: the 1.2 client
/// cancels an in-progress doodad world interaction. Reader `FUN_397c6540` (`bc unitObjId`, then
/// `u32 doodadObjId`; EVIDENCED, content research 1.4). The trigger is capture-pending
/// (CAP-INT-1): nothing sends it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCWorldInteractionCanceledPacket {
    pub unit_obj_id: u32,
    pub doodad_obj_id: u32,
}
packet_def!(
    SCWorldInteractionCanceledPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCWorldInteractionCanceledPacket
);
impl Marshal for SCWorldInteractionCanceledPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.unit_obj_id).write_u32(self.doodad_obj_id);
    }
}

/// One `InteractionInfo` block of [`SCWorldInteractionSkillListPacket`] (0x40 bytes in the 1.2
/// client).
///
/// The client reader `FUN_397bef50` (`x2game.dll.c:1716218`) reads `bc`, `bc`, `u32`, `bc`, then
/// `i32 interactCount`, `i32 extraInfo` and `interactCount` `u32` skill ids, reading at most 10
/// of them. Roles of the first `bc`, the `u32` and the fourth `bc` are UNKNOWN; the handler
/// `FUN_393b7440` (`x2game.dll.c:773717`) resolves the second `bc` through the world-object
/// table (`FUN_39035110`) as the interacted doodad and starts `skill_list[0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldInteractionInfo {
    /// First `bc`; role UNKNOWN.
    pub unk1: u32,
    /// Second `bc`: the interacted doodad object id (the handler's world-object lookup).
    pub doodad_obj_id: u32,
    /// `u32`; role UNKNOWN.
    pub unk2: u32,
    /// Fourth `bc`; role UNKNOWN.
    pub unk3: u32,
    /// `extraInfo` (`i32` on the wire before the skill list).
    pub extra_info: i32,
    /// `u32` skill ids. The client reads at most 10; a sender must not write more.
    pub skill_list: Vec<u32>,
}

/// `SCWorldInteractionSkillListPacket`: the server-initiated doodad world-interaction list
/// (opcode 0x07e; the client's `x2ui/interaction.alb` window).
///
/// sent_by: none (CAP-INT-1, trigger unknown). The 1.2 client reader is `FUN_397c2d70`
/// (`x2game.dll.c:1718589`, vtable `399b36e4`) + `FUN_397bef50`: `bc`, `bc`,
/// [`WorldInteractionInfo`], `u8 mouseButton`, `i32 modifierKeys`. The handler `FUN_391d9080`
/// (`x2game.dll.c:367106`) copies the 0x40-byte info and starts the first skill when the player
/// is within the interaction range. Roles of the two leading `bc` values are UNKNOWN. See
/// `PORTING.md` ("Gap audit verdicts (2026-11-07)").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCWorldInteractionSkillListPacket {
    /// First top-level `bc`; role UNKNOWN.
    pub unk1: u32,
    /// Second top-level `bc`; role UNKNOWN.
    pub unk2: u32,
    pub info: WorldInteractionInfo,
    pub mouse_button: u8,
    pub modifier_keys: i32,
}
packet_def!(
    SCWorldInteractionSkillListPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCWorldInteractionSkillListPacket
);
impl Marshal for SCWorldInteractionSkillListPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.unk1).write_bc(self.unk2);
        s.write_bc(self.info.unk1)
            .write_bc(self.info.doodad_obj_id)
            .write_u32(self.info.unk2)
            .write_bc(self.info.unk3)
            .write_i32(self.info.skill_list.len() as i32)
            .write_i32(self.info.extra_info);
        for &id in &self.info.skill_list {
            s.write_u32(id);
        }
        s.write_u8(self.mouse_button).write_i32(self.modifier_keys);
    }
}

/// `SCDoodadSoundPacket(doodad, soundId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCDoodadSoundPacket {
    pub obj_id: u32,
    pub sound_id: u32,
}
packet_def!(
    SCDoodadSoundPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadSoundPacket
);
impl Marshal for SCDoodadSoundPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id).write_u32(self.sound_id);
    }
}

/// `SCDoodadChangedPacket(id, data)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCDoodadChangedPacket {
    pub obj_id: u32,
    pub data: i32,
}
packet_def!(
    SCDoodadChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadChangedPacket
);
impl Marshal for SCDoodadChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id).write_i32(self.data);
    }
}

/// `SCDoodadOriginatorPacket(objId, newOwnerId, newFaction)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCDoodadOriginatorPacket {
    pub obj_id: u32,
    pub new_owner_id: u32,
    /// `FactionsEnum`.
    pub new_faction: u32,
}
packet_def!(
    SCDoodadOriginatorPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCDoodadOriginatorPacket
);
impl Marshal for SCDoodadOriginatorPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id)
            .write_u32(self.new_owner_id)
            .write_u32(self.new_faction);
    }
}

/// `SCDoodadQuestAcceptPacket`: the quest subsystem's definition (one per opcode).
pub use crate::systems::quests::packets::SCDoodadQuestAcceptPacket;

/// `SCBondDoodadPacket(unitObjId, bond)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCBondDoodadPacket {
    pub unit_obj_id: u32,
    pub bond: BondDoodad,
}
packet_def!(
    SCBondDoodadPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCBondDoodadPacket
);
impl Marshal for SCBondDoodadPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.unit_obj_id);
        self.bond.write(s);
    }
}

/// `SCUnbondDoodadPacket(characterObjId, characterId, doodadObjId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCUnbondDoodadPacket {
    pub character_obj_id: u32,
    pub character_id: u32,
    pub doodad_obj_id: u32,
}
packet_def!(
    SCUnbondDoodadPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCUnbondDoodadPacket
);
impl Marshal for SCUnbondDoodadPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.character_obj_id)
            .write_u32(self.character_id)
            .write_bc(self.doodad_obj_id);
    }
}

// ---------------------------------------------------------------------------------------------
// C->S
// ---------------------------------------------------------------------------------------------

/// Registers the doodad C->S packets (`GameNetwork` order).
pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSUnbondDoodadPacket>(LEVEL_PLAIN, cs::CSUnbondDoodadPacket);
    reg.register::<CSCreateDoodadPacket>(LEVEL_PLAIN, cs::CSCreateDoodadPacket);
    reg.register::<CSChangeDoodadPhasePacket>(LEVEL_PLAIN, cs::CSChangeDoodadPhasePacket);
    reg.register::<CSChangeDoodadDataPacket>(LEVEL_PLAIN, cs::CSChangeDoodadDataPacket);
    reg.register::<CSSetLogicDoodadPacket>(LEVEL_PLAIN, cs::CSSetLogicDoodadPacket);
}

/// `CSChangeDoodadDataPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSChangeDoodadDataPacket {
    pub obj_id: u32,
    pub data: i32,
}
impl ClientPacket for CSChangeDoodadDataPacket {
    const NAME: &'static str = "CSChangeDoodadDataPacket";
    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            data: s.read_i32()?,
        })
    }
    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!(
                "ChangeDoodadData, ObjId: {}, Data: {}",
                self.obj_id,
                self.data
            );
            let Some(me) = conn.active_char.clone() else {
                return Ok(());
            };
            let Some(world) = server.world.world_of(&WorldObject::from(&me)) else {
                return Ok(());
            };
            if let Some(doodad) = world.doodad(self.obj_id) {
                let template_id = doodad.data().template_id;
                tracing::debug!("Doodad: ({template_id})");
                if !super::manager::change_doodad_data(server, &world, &me, &doodad, self.data) {
                    me.send(error_msg(error_message::INTERACTION_PERMISSION_DENY));
                }
            }
            Ok(())
        })
    }
}

/// `CSChangeDoodadPhasePacket`: sent by the 1.2 client when it finishes a
/// `DoodadFuncBubble` (0x3f) or `DoodadFuncOpenPaper` (0x46) whose func row has
/// `next_phase > 0` (`FUN_393b8240` / `FUN_393b8450`; builder `FUN_397ac8d0`, writer
/// `FUN_397bbbb0`). Body: `bc objId`, `u32 skill`, `u32 nextPhase`, `u32 funcId`
/// (`doodad_funcs.id`).
///
/// The C# only logged it, so the phase never advanced. The handler accepts the client's
/// phase change only when `funcId` is a Bubble / OpenPaper func of the doodad's current
/// phase group whose `next_phase` is exactly `nextPhase`, from a live caster within the func's
/// own reach (never the packet's `skill`) and a doodad not scheduled for despawn; the movement
/// itself is the normal phase machinery (`DoChangePhase`: phase funcs, then
/// `SCDoodadPhaseChangedPacket`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSChangeDoodadPhasePacket {
    pub obj_id: u32,
    /// The skill that ran the func (the client's first argument to the builder).
    pub skill: u32,
    pub next_phase: u32,
    /// `doodad_funcs.id` (`FuncKey`) of the Bubble / OpenPaper func.
    pub func_id: u32,
}
impl ClientPacket for CSChangeDoodadPhasePacket {
    const NAME: &'static str = "CSChangeDoodadPhasePacket";
    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            skill: s.read_u32()?,
            next_phase: s.read_u32()?,
            func_id: s.read_u32()?,
        })
    }
    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(me) = conn.active_char.clone() else {
                return Ok(());
            };
            let Some(world) = server.world.world_of(&WorldObject::from(&me)) else {
                return Ok(());
            };
            let Some(doodad) = world.doodad(self.obj_id) else {
                tracing::debug!(
                    "ChangeDoodadPhase, no doodad objId: {}, funcId: {}",
                    self.obj_id,
                    self.func_id
                );
                return Ok(());
            };
            // A dead character cannot run a doodad interaction (`Doodad.Use` has no dead check,
            // but every skill / item path that reaches it does).
            if me.data().unit.is_dead() {
                tracing::debug!(
                    "ChangeDoodadPhase, dead caster {} objId: {}, funcId: {}",
                    me.data().unit.name,
                    self.obj_id,
                    self.func_id
                );
                return Ok(());
            }
            // `blocked_child_doodads`: a player cannot start the child doodad's phase funcs
            // while its parent unit carries the blocking buff.
            if super::phase::refuse_blocked_child_doodad(
                server,
                &world,
                &doodad,
                Some(&WorldObject::from(&me)),
            ) {
                return Ok(());
            }
            // Same refusal as `Doodad.Use` (phase.rs): a doodad scheduled for despawn must not
            // change phase.
            if server
                .systems
                .npcs
                .spawns
                .despawn_field(doodad.obj_id)
                .is_some()
            {
                tracing::debug!(
                    "ChangeDoodadPhase refused: doodad {} is scheduled for despawn",
                    self.obj_id
                );
                me.send(error_msg(error_message::NO_INTERACTION_AVAILABLE));
                return Ok(());
            }
            // Check the current phase and move it under the same operation reservation.
            let Some(_operation) = super::phase::Operation::enter(&doodad) else {
                return Ok(());
            };
            let data = &server.game_data.systems.npcs.doodad;
            let Some(func) = data.func(self.func_id) else {
                tracing::debug!(
                    "ChangeDoodadPhase, no doodadFunc objId: {}, funcId: {}",
                    self.obj_id,
                    self.func_id
                );
                return Ok(());
            };
            let group = doodad.data().func_group_id;
            let expected = i32::try_from(self.next_phase).unwrap_or(i32::MAX);
            if func.group_id != group
                || !matches!(
                    func.func_type.as_str(),
                    "DoodadFuncBubble" | "DoodadFuncOpenPaper"
                )
                || func.next_phase != expected
                || func.next_phase <= 0
            {
                tracing::debug!(
                    "ChangeDoodadPhase, func not usable: objId: {}, funcId: {}, group: {} (current {}), nextPhase: {}, requested {}",
                    self.obj_id,
                    self.func_id,
                    func.group_id,
                    group,
                    func.next_phase,
                    self.next_phase
                );
                return Ok(());
            }
            if !in_func_reach(server, &me, &doodad, func.skill_id) {
                tracing::debug!(
                    "ChangeDoodadPhase, out of range: objId: {}, funcId: {}, func skill: {} (client sent {})",
                    self.obj_id,
                    self.func_id,
                    func.skill_id,
                    self.skill
                );
                return Ok(());
            }
            super::manager::do_change_phase(
                server,
                &world,
                &doodad,
                Some(&WorldObject::from(&me)),
                func.next_phase,
            );
            Ok(())
        })
    }
}

/// Reach of a Bubble / OpenPaper func, never using the packet's `skill` (it is client input and
/// the skill-less funcs carry none). A known positive func skill range uses it plus the doodad
/// allowance the cast target check adds; a skill-less / unknown / unbounded func gets the 5 m
/// doodad interaction distance (`portal::NAVI_USE_RANGE`, also `looting::LOOT_TAKE_RANGE`) plus
/// that allowance.
pub(crate) fn in_func_reach(
    server: &GameServer,
    player: &crate::world::PlayerRef,
    doodad: &crate::world::DoodadRef,
    func_skill_id: u32,
) -> bool {
    use crate::systems::skills::geometry::{distance_to, Target};
    let allowance = crate::systems::skills::validation::DOODAD_RANGE_ALLOWANCE;
    let limit = server
        .game_data
        .systems
        .skills
        .skill(func_skill_id)
        .filter(|skill| skill.max_range > 0)
        .map_or_else(
            || f64::from(crate::systems::npcs::portal::NAVI_USE_RANGE) + allowance,
            |skill| f64::from(skill.max_range) + allowance,
        );
    let distance = distance_to(
        server,
        &Target::Obj(WorldObject::from(player)),
        &Target::Obj(WorldObject::from(doodad)),
        true,
    );
    f64::from(distance) <= limit
}

/// `CSSetLogicDoodadPacket`: provider and subscriber object IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSSetLogicDoodadPacket {
    pub obj_id: u32,
    pub obj2_id: u32,
}
impl ClientPacket for CSSetLogicDoodadPacket {
    const NAME: &'static str = "CSSetLogicDoodadPacket";
    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            obj2_id: s.read_bc()?,
        })
    }
    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(player) = &conn.active_char {
                if !super::logic::link(server, player, self.obj_id, self.obj2_id) {
                    player.send(error_msg(error_message::NO_INTERACTION_AVAILABLE));
                }
            }
            Ok(())
        })
    }
}

/// `CSSaveDoodadUccStringPacket` (logged only; not registered, see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSaveDoodadUccStringPacket {
    pub obj_id: u32,
    pub data: String,
}
impl ClientPacket for CSSaveDoodadUccStringPacket {
    const NAME: &'static str = "CSSaveDoodadUccStringPacket";
    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            data: s.read_string()?,
        })
    }
    fn handle<'a>(
        self,
        _server: &'a GameServer,
        _conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!("SaveDoodadUccString, ObjId: {}", self.obj_id);
            Ok(())
        })
    }
}

/// `CSUnbondDoodadPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSUnbondDoodadPacket {
    pub character_obj_id: u32,
    pub doodad_obj_id: u32,
}
impl ClientPacket for CSUnbondDoodadPacket {
    const NAME: &'static str = "CSUnbondDoodadPacket";
    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            character_obj_id: s.read_bc()?,
            doodad_obj_id: s.read_bc()?,
        })
    }
    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(me) = conn.active_char.clone() else {
                return Ok(());
            };
            super::manager::unbond_doodad(server, &me, self.character_obj_id, self.doodad_obj_id);
            Ok(())
        })
    }
}

/// `CSCreateDoodadPacket`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CSCreateDoodadPacket {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub z_rot: f32,
    pub scale: f32,
    pub item_id: u64,
}
impl ClientPacket for CSCreateDoodadPacket {
    const NAME: &'static str = "CSCreateDoodadPacket";
    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            id: s.read_u32()?,
            x: helpers::convert_long_to_coord(s.read_i64()?),
            y: helpers::convert_long_to_coord(s.read_i64()?),
            z: s.read_f32()?,
            z_rot: s.read_f32()?,
            scale: s.read_f32()?,
            item_id: s.read_u64()?,
        })
    }
    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(me) = conn.active_char.clone() else {
                return Ok(());
            };
            super::manager::handle_create_doodad(server, &me, &self);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::packet::{encode, ServerPacket};

    fn body(p: &dyn ServerPacket) -> Vec<u8> {
        encode(p)[6..].to_vec()
    }

    fn bc(v: u32) -> [u8; 3] {
        let b = v.to_le_bytes();
        [b[0], b[1], b[2]]
    }

    #[test]
    fn sc_doodad_phase_changed() {
        let mut d = Doodad::new(2336);
        d.func_group_id = 4652;
        d.item_template_id = 27606;
        d.growth_time_ms = 31_000;
        let p = SCDoodadPhaseChangedPacket::new(0x01_0203, &d, 1_000);
        assert_eq!(p.opcode(), sc::SCDoodadPhaseChangedPacket);
        let mut e = bc(0x01_0203).to_vec();
        e.extend(4652u32.to_le_bytes());
        e.extend(30_000u32.to_le_bytes());
        e.extend((-1i32).to_le_bytes());
        e.extend(27606u32.to_le_bytes());
        assert_eq!(body(&p), e);
    }

    #[test]
    fn sc_doodad_phase_msg() {
        // Reader FUN_397b6460: `bc doodadObjId ; u32 funcGroupId`.
        let p = SCDoodadPhaseMsgPacket {
            doodad_obj_id: 0x01_0203,
            func_group_id: 4699,
        };
        assert_eq!(p.opcode(), sc::SCDoodadPhaseMsgPacket);
        assert_eq!(body(&p), vec![0x03, 0x02, 0x01, 0x5b, 0x12, 0x00, 0x00]);
    }

    #[test]
    fn sc_world_interaction_canceled_bytes() {
        let p = SCWorldInteractionCanceledPacket {
            unit_obj_id: 0x123,
            doodad_obj_id: 77,
        };
        assert_eq!(p.opcode(), sc::SCWorldInteractionCanceledPacket);
        assert_eq!(body(&p), vec![0x23, 0x01, 0x00, 0x4d, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn sc_world_interaction_skill_list_bytes() {
        // Reader FUN_397c2d70 + FUN_397bef50 (x2game.dll.c:1718589 / :1716218):
        // bc, bc, { bc, bc, u32, bc, i32 count, i32 extraInfo, u32[count] }, u8 mouse, i32 mods.
        let p = SCWorldInteractionSkillListPacket {
            unk1: 0x01_0203,
            unk2: 0x04_0506,
            info: WorldInteractionInfo {
                unk1: 0x07_0809,
                doodad_obj_id: 0x0a_0b0c,
                unk2: 0x1122_3344,
                unk3: 0x0d_0e0f,
                extra_info: -3,
                skill_list: vec![0x1516_1718, 0x191a_1b1c],
            },
            mouse_button: 0x21,
            modifier_keys: -5,
        };
        assert_eq!(p.opcode(), sc::SCWorldInteractionSkillListPacket);
        assert_eq!(p.opcode(), 0x07e);
        #[rustfmt::skip]
        let want: Vec<u8> = vec![
            0x03, 0x02, 0x01, // unk1
            0x06, 0x05, 0x04, // unk2
            0x09, 0x08, 0x07, // info.unk1
            0x0c, 0x0b, 0x0a, // info.doodad_obj_id
            0x44, 0x33, 0x22, 0x11, // info.unk2
            0x0f, 0x0e, 0x0d, // info.unk3
            0x02, 0x00, 0x00, 0x00, // interactCount
            0xfd, 0xff, 0xff, 0xff, // extraInfo = -3
            0x18, 0x17, 0x16, 0x15, // skill_list[0]
            0x1c, 0x1b, 0x1a, 0x19, // skill_list[1]
            0x21, // mouseButton
            0xfb, 0xff, 0xff, 0xff, // modifierKeys = -5
        ];
        assert_eq!(body(&p), want);
    }

    #[test]
    fn sc_doodad_sound_changed_originator_quest_accept() {
        let mut e = bc(7).to_vec();
        e.extend(55u32.to_le_bytes());
        assert_eq!(
            body(&SCDoodadSoundPacket {
                obj_id: 7,
                sound_id: 55
            }),
            e
        );

        let mut e = bc(8).to_vec();
        e.extend((-3i32).to_le_bytes());
        assert_eq!(
            body(&SCDoodadChangedPacket {
                obj_id: 8,
                data: -3
            }),
            e
        );

        let mut e = bc(9).to_vec();
        e.extend(10u32.to_le_bytes());
        e.extend(148u32.to_le_bytes());
        let p = SCDoodadOriginatorPacket {
            obj_id: 9,
            new_owner_id: 10,
            new_faction: 148,
        };
        assert_eq!(body(&p), e);

        let mut e = bc(11).to_vec();
        e.extend(4321u32.to_le_bytes());
        let p = SCDoodadQuestAcceptPacket {
            doodad_obj_id: 11,
            quest_id: 4321,
        };
        assert_eq!(body(&p), e);
        assert_eq!(p.opcode(), sc::SCDoodadQuestAcceptPacket);
    }

    #[test]
    fn sc_bond_unbond_doodad() {
        let bond = BondDoodad {
            owner_obj_id: 0x55,
            attach_point: 2,
            kind: 6,
            space: 1,
            spot: 0,
        };
        let p = SCBondDoodadPacket {
            unit_obj_id: 0x44,
            bond,
        };
        let mut e = bc(0x44).to_vec();
        e.push(2);
        e.extend(bc(0x55));
        e.push(6);
        e.extend(1i32.to_le_bytes());
        e.extend(0i32.to_le_bytes());
        assert_eq!(body(&p), e);

        let p = SCUnbondDoodadPacket {
            character_obj_id: 0x44,
            character_id: 3,
            doodad_obj_id: 0x55,
        };
        let mut e = bc(0x44).to_vec();
        e.extend(3u32.to_le_bytes());
        e.extend(bc(0x55));
        assert_eq!(body(&p), e);
    }

    fn stream(f: impl FnOnce(&mut PacketStream)) -> PacketStream {
        let mut s = PacketStream::new();
        f(&mut s);
        PacketStream::from_bytes(s.into_bytes())
    }

    #[test]
    fn cs_packets_decode() {
        let mut s = stream(|s| {
            s.write_bc(0x123).write_i32(3);
        });
        assert_eq!(
            CSChangeDoodadDataPacket::read(&mut s).unwrap(),
            CSChangeDoodadDataPacket {
                obj_id: 0x123,
                data: 3
            }
        );
        let mut s = stream(|s| {
            s.write_bc(5).write_u32(13356).write_u32(2).write_u32(3);
        });
        assert_eq!(
            CSChangeDoodadPhasePacket::read(&mut s).unwrap(),
            CSChangeDoodadPhasePacket {
                obj_id: 5,
                skill: 13356,
                next_phase: 2,
                func_id: 3
            }
        );
        let mut s = stream(|s| {
            s.write_bc(5).write_bc(6);
        });
        assert_eq!(
            CSSetLogicDoodadPacket::read(&mut s).unwrap(),
            CSSetLogicDoodadPacket {
                obj_id: 5,
                obj2_id: 6
            }
        );
        let mut s = stream(|s| {
            s.write_bc(5).write_bc(6);
        });
        assert_eq!(
            CSUnbondDoodadPacket::read(&mut s).unwrap(),
            CSUnbondDoodadPacket {
                character_obj_id: 5,
                doodad_obj_id: 6
            }
        );
        let mut s = stream(|s| {
            s.write_bc(5).write_str("hello");
        });
        assert_eq!(
            CSSaveDoodadUccStringPacket::read(&mut s).unwrap(),
            CSSaveDoodadUccStringPacket {
                obj_id: 5,
                data: "hello".into()
            }
        );
        let mut s = stream(|s| {
            s.write_u32(2336)
                .write_i64(helpers::convert_coord_to_long(1000.5))
                .write_i64(helpers::convert_coord_to_long(2000.25))
                .write_f32(100.0)
                .write_f32(1.5)
                .write_f32(1.0)
                .write_u64(77);
        });
        assert_eq!(
            CSCreateDoodadPacket::read(&mut s).unwrap(),
            CSCreateDoodadPacket {
                id: 2336,
                x: 1000.5,
                y: 2000.25,
                z: 100.0,
                z_rot: 1.5,
                scale: 1.0,
                item_id: 77
            }
        );
    }
}
