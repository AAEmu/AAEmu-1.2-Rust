//! Game server client (C2G / G2C + the proxy channel) for the 1.2 protocol.
//!
//! Every frame payload is `[0xDD][level]...`: level 1 (game) carries `[hash][count][u16 opcode]`,
//! level 2 (proxy / CryNetwork) just `[u16 opcode]`. Traffic is unencrypted; native level-4
//! game batches carry raw DEFLATE.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use aaemu_commons::net::FrameCodec;
use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, name_of, pp, sc};
use anyhow::{anyhow, bail, Context};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::record::{Channel, Dir, Recorder};

pub const LEVEL_GAME: u8 = 1;
pub const LEVEL_PROXY: u8 = 2;
/// `CompressedGamePackets` (`DD04`).
pub const LEVEL_COMPRESSED: u8 = 4;

/// The transport has a u16 frame length. Batches may compress at most 1 MiB into that
/// frame and contain at most 4096 entries (enough for the native all-skill cooldown reset).
/// Every expanded entry must also fit a normal level-1 frame. Limits apply before copies
/// and before inflater output growth, not just when events reach the recorder.
pub const MAX_GAME_FRAME: usize = u16::MAX as usize;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_PACKETS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchError {
    Inflate,
    Limit,
    Structure,
    Unsupported,
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Inflate => "invalid DD04 compression",
            Self::Limit => "DD04 limit exceeded",
            Self::Structure => "invalid DD04 structure",
            Self::Unsupported => "unsupported DD04 opcode",
        })
    }
}
impl std::error::Error for BatchError {}

/// miniz_oxide 0.8.9 core::decompress writes only into the supplied slice and returns
/// (status, input consumed, output written). Unlike DecompressError, status contains no
/// partial plaintext. A fixed 1 MiB output slice bounds allocation even for a tiny bomb.
/// Requiring all input consumed also rejects trailing or concatenated deflate streams.
fn inflate_batch(input: &[u8]) -> Result<Vec<u8>, BatchError> {
    use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
    use miniz_oxide::inflate::TINFLStatus;
    if input.len() > MAX_GAME_FRAME - 4 {
        return Err(BatchError::Limit);
    }
    let mut output = vec![0; MAX_BATCH_BYTES];
    let mut inflater = Box::<DecompressorOxide>::default();
    let (status, used, written) = decompress(
        &mut inflater,
        input,
        &mut output,
        0,
        inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    match status {
        TINFLStatus::Done if used == input.len() => {
            output.truncate(written);
            Ok(output)
        }
        TINFLStatus::HasMoreOutput => Err(BatchError::Limit),
        _ => Err(BatchError::Inflate),
    }
}

fn batch_caster(s: &mut PacketStream) -> anyhow::Result<()> {
    let kind = s.read_u8()?;
    aaemu_game::systems::skills::types::SkillCaster::read(kind, s)?;
    Ok(())
}

fn batch_cast_action(s: &mut PacketStream) -> anyhow::Result<()> {
    // CastAction has a native writer but no reader (types.rs, CastAction.Write).
    let len = match s.read_u8()? {
        0 => 6,
        1 => 14,
        2 => 13,
        3 => 11,
        4 => 7,
        _ => bail!(BatchError::Structure),
    };
    s.read_bytes(len)?;
    Ok(())
}

fn batch_plot_object(s: &mut PacketStream) -> anyhow::Result<()> {
    // PlotObject.Write: Unit = type1+BC; Position = type2+position9+rotation3.
    let len = match s.read_u8()? {
        1 => 3,
        2 => 12,
        _ => bail!(BatchError::Structure),
    };
    s.read_bytes(len)?;
    Ok(())
}

/// Consume exactly one native body. No boundary scanning or treating remaining bytes as
/// an unknown body: an unsupported opcode rejects the *whole* batch before publication.
/// Covers every current CompressedGamePackets producer: plots, damage/heal and reset_all.
fn batch_body(dir: Dir, opcode: u16, s: &mut PacketStream) -> anyhow::Result<()> {
    if dir == Dir::C2S {
        if opcode == cs::X2EnterWorldPacket {
            s.read_bytes(29)?;
            return Ok(());
        }
        bail!(BatchError::Unsupported);
    }
    match opcode {
        sc::SCReconnectAuthPacket => {
            s.read_u32()?;
        }
        sc::X2EnterWorldResponsePacket => {
            s.read_bytes(17)?; // reason, gm, stream token, port, world time
            s.read_bytes_sized()?; // pubKeySize / pubKey (both native u16-sized)
            s.read_string()?;
        }
        sc::SCSkillCooldownResetPacket => {
            s.read_bytes(12)?;
        }
        sc::SCSkillEndedPacket | sc::SCPlotEndedPacket => {
            s.read_bytes(2)?;
        }
        sc::SCErrorMsgPacket => {
            s.read_bytes(9)?;
        }
        sc::SCCastingStoppedPacket => {
            s.read_bytes(6)?;
        }
        sc::SCBuffRemovedPacket | sc::SCBuffLearnedPacket | sc::SCSkillStoppedPacket => {
            s.read_bytes(7)?;
        }
        sc::SCUnitDamagedPacket => {
            batch_cast_action(s)?;
            batch_caster(s)?;
            s.read_bytes(7)?; // casterBC, targetBC, crime
            s.read_pisc_w(3)?;
            s.read_pisc_w(3)?;
            s.read_bytes(5)?; // holdable, hit type, flag, result
        }
        sc::SCUnitHealedPacket => {
            batch_cast_action(s)?;
            batch_caster(s)?;
            s.read_bytes(14)?; // targetBC, heal/hit, value, zero, result
        }
        sc::SCPlotEventPacket => {
            s.read_bytes(10)?; // tl, event, skill
            batch_plot_object(s)?;
            batch_plot_object(s)?;
            s.read_bytes(18)?; // item, objectBC, casting, zeroBC, zeroi16
            let count = s.read_u8()?;
            s.read_bytes(usize::from(count) * 3)?;
            let flag = s.read_u8()?;
            if flag & 8 != 0 {
                s.read_bytes(13 * 4)?;
            }
        }
        sc::SCBuffCreatedPacket => {
            batch_caster(s)?;
            s.read_bytes(22)?; // caster, ownerBC, index, buff, level, ability, skill
            s.read_pisc_w(4)?;
        }
        sc::SCSkillStartedPacket | sc::SCSkillFiredPacket => {
            use aaemu_game::systems::skills::types::{SkillCastTarget, SkillObject};
            s.read_bytes(6)?; // skill and transport ids
            batch_caster(s)?;
            let kind = s.read_u8()?;
            SkillCastTarget::read(kind, s)?;
            let flag = s.read_u8()?;
            anyhow::ensure!(flag <= 7, "invalid skill object type");
            SkillObject::read(flag, s)?;
            if opcode == sc::SCSkillFiredPacket {
                s.read_bytes(10)?;
            } else {
                s.read_bytes(5)?; // real/base cast, synergy
                let extra = s.read_u8()?;
                anyhow::ensure!(extra & !7 == 0, "invalid skill extra flags");
                if extra & 1 != 0 {
                    s.read_u8()?;
                }
                if extra & 2 != 0 {
                    s.read_u16()?;
                }
                if extra & 4 != 0 {
                    s.read_u32()?;
                }
            }
        }
        _ => bail!(BatchError::Unsupported),
    }
    Ok(())
}

/// Shared native batch validation for GameClient and Recorder. The input is the already
/// bounded inflated body, whose entries are [u16 zero][u16 opcode][native body]. No nesting
/// exists in this format. Any nonzero header, bad count/trailing bytes, or unknown body is
/// rejected atomically with a fixed error, never with input bytes or an underlying cause.
pub(crate) fn split_game_batch(
    dir: Dir,
    count: u16,
    body: &[u8],
) -> Result<Vec<Packet>, BatchError> {
    if body.len() > MAX_BATCH_BYTES || usize::from(count) > MAX_BATCH_PACKETS {
        return Err(BatchError::Limit);
    }
    let mut stream = PacketStream::from_bytes(body.to_vec());
    let mut packets = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        if stream.read_u16().map_err(|_| BatchError::Structure)? != 0 {
            return Err(BatchError::Structure);
        }
        let opcode = stream.read_u16().map_err(|_| BatchError::Structure)?;
        let start = stream.pos();
        if let Err(error) = batch_body(dir, opcode, &mut stream) {
            return Err(
                if error.downcast_ref::<BatchError>() == Some(&BatchError::Unsupported) {
                    BatchError::Unsupported
                } else {
                    BatchError::Structure
                },
            );
        }
        let end = stream.pos();
        if end - start > MAX_GAME_FRAME - 6 {
            return Err(BatchError::Limit);
        }
        packets.push(Packet {
            level: LEVEL_GAME,
            opcode,
            body: body[start..end].to_vec(),
        });
    }
    if stream.remaining() != 0 {
        return Err(BatchError::Structure);
    }
    Ok(packets)
}

/// A received G2C / proxy packet.
#[derive(Debug)]
pub struct Packet {
    pub level: u8,
    pub opcode: u16,
    pub body: Vec<u8>,
}

impl Packet {
    pub fn name(&self) -> &'static str {
        packet_name(Dir::S2C, self.level, self.opcode)
    }
    pub fn is(&self, level: u8, opcode: u16) -> bool {
        self.level == level && self.opcode == opcode
    }
    pub fn is_sc(&self, opcode: u16) -> bool {
        self.is(LEVEL_GAME, opcode)
    }
    pub fn stream(&self) -> PacketStream {
        PacketStream::from_bytes(self.body.clone())
    }
}

/// C# constant name of a game opcode.
pub fn packet_name(dir: Dir, level: u8, opcode: u16) -> &'static str {
    let table = match (dir, level) {
        (_, LEVEL_PROXY) => pp::NAMES,
        (Dir::S2C, _) => sc::NAMES,
        (Dir::C2S, _) => cs::NAMES,
    };
    name_of(table, opcode).unwrap_or("Unknown")
}

/// A character of `SCCharacterListPacket` / `SCCreateCharacterResponsePacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharInfo {
    pub id: u32,
    pub name: String,
}

/// Consume one complete native `Character.Write` record. The list has no entry lengths;
/// stopping after id/name would parse the next character from equipment or model bytes.
fn read_character(s: &mut PacketStream) -> anyhow::Result<CharInfo> {
    use aaemu_game::models::container::{equip_slot, EQUIPMENT_CONTAINER_SIZE};
    use aaemu_game::models::item::detail_body_len;
    use aaemu_game::models::unit_model::UnitCustomModelParams;

    let id = s.read_u32()?;
    let name = s.read_string()?;
    s.read_u8()?; // race
    s.read_u8()?; // gender
    s.read_u8()?; // level
    s.read_i32()?; // hp
    s.read_i32()?; // mp
    s.read_u32()?; // zone
    s.read_u32()?; // faction
    s.read_string()?; // faction name (variable length)
    s.read_u32()?; // expedition
    s.read_u32()?; // family
    for slot in 0..EQUIPMENT_CONTAINER_SIZE {
        let template = s.read_u32()?;
        // Native appearance slots use BodyPart.Write (template only), including nonzero ids.
        if template == 0 || (equip_slot::FACE..=equip_slot::BEARD).contains(&slot) {
            continue;
        }
        s.read_u64()?; // instance id
        s.read_u8()?; // grade
        s.read_u8()?; // flags
        s.read_i32()?; // count
        let detail = s.read_u8()?;
        let len = detail_body_len(detail);
        anyhow::ensure!(detail == 0 || len != 0, "unknown equipment detail type");
        s.read_bytes(len)?;
        s.read_i64()?; // create time
        s.read_i32()?; // lifespan
        s.read_u32()?; // made unit id
        s.read_u8()?; // world id
        s.read_i64()?; // unsecure time
        s.read_i64()?; // unpack time
    }
    s.read_bytes(3)?; // abilities
    s.read_i64()?; // x
    s.read_i64()?; // y
    s.read_f32()?; // z
    let _: UnitCustomModelParams = s.read()?;
    s.read_i16()?; // labor
    s.read_i64()?; // labor modified
    s.read_i16()?; // dead count
    s.read_i64()?; // dead time
    s.read_i32()?; // rez wait
    s.read_i64()?; // rez time
    s.read_i32()?; // rez penalty
    s.read_i64()?; // last world leave
    s.read_i64()?; // money
    s.read_i64()?; // money amount
    s.read_i16()?; // crime
    s.read_i32()?; // infamy
    s.read_i16()?; // trials
    s.read_i64()?; // delete request
    s.read_i64()?; // transfer request
    s.read_i64()?; // delete delay
    s.read_i32()?; // consumed labor
    s.read_i64()?; // loyalty
    s.read_i64()?; // money2
    s.read_i64()?; // money amount
    s.read_bool()?; // auto AA point
    s.read_i32()?; // prev point
    s.read_i32()?; // point
    s.read_i32()?; // gift
    s.read_i64()?; // updated
    s.read_u8()?; // force name change
    Ok(CharInfo { id, name })
}

/// A unit seen in `SCUnitStatePacket`.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitInfo {
    pub obj_id: u32,
    pub name: String,
    pub unit_type: u8,
    /// Character id (type 0), NPC / slave template id (types 1 / 2).
    pub id: u32,
    pub pos: (f32, f32, f32),
    /// Transport-layer id of a mate (type 5, `SCUnitStatePacket`); 0 for every other unit.
    pub tl_id: u16,
}

/// A doodad seen in `SCDoodadCreatedPacket` / `SCDoodadsCreatedPacket` (`Doodad.Write`).
#[derive(Clone, Debug, PartialEq)]
pub struct DoodadInfo {
    pub obj_id: u32,
    pub template_id: u32,
    pub pos: (f32, f32, f32),
    /// Current phase (`func_group_id`), updated by `SCDoodadPhaseChangedPacket`.
    pub func_group_id: u32,
    pub owner_obj_id: u32,
    pub parent_obj_id: u32,
    pub quest_glow: u32,
    pub data: i32,
}

/// Reads one `Doodad.Write` entry (see `models/doodad.rs` of the game crate).
fn read_doodad(s: &mut PacketStream) -> anyhow::Result<DoodadInfo> {
    let obj_id = s.read_bc()?;
    let template_id = s.read_u32()?;
    let owner_obj_id = s.read_bc()?;
    let parent_obj_id = s.read_bc()?;
    s.read_u8()?; // attach point
    let pos = s.read_position()?;
    s.read_i16()?; // roll
    s.read_i16()?; // pitch
    s.read_i16()?; // yaw
    s.read_f32()?; // scale
    s.read_bool()?; // has loot item
    let func_group_id = s.read_u32()?;
    s.read_u32()?; // owner id
    s.read_u64()?; // ucc id
    s.read_u32()?; // item template id
    s.read_u32()?; // type2
    s.read_u32()?; // time left
    s.read_i64()?; // plant time
    let quest_glow = s.read_u32()?;
    s.read_i32()?; // family
    s.read_i32()?; // puzzle group
    s.read_u8()?; // owner type
    s.read_u32()?; // owner db id
    let data = s.read_i32()?;
    Ok(DoodadInfo {
        obj_id,
        template_id,
        pos,
        func_group_id,
        owner_obj_id,
        parent_obj_id,
        quest_glow,
        data,
    })
}

/// The last `SCMateSpawnedPacket`: the owner's own view of the summoned mate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MateSpawn {
    pub tl_id: u16,
    pub db_id: u32,
    pub item_id: u64,
    pub user_state: u8,
}

/// One item of an `SCItemTaskSuccessPacket` task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskItem {
    /// `ItemAction` of the task (5 create, 9 update detail, ...).
    pub task: u8,
    /// Template id (0 for tasks that do not carry it, e.g. `UpdateDetail`).
    pub template: u32,
    pub id: u64,
    /// `SummonMate` detail block: `(exp, dead flag, level)`; a non-zero dead flag marks a corpse.
    pub mate: Option<(i32, u8, u8)>,
}

/// The readable part of an `SCItemTaskSuccessPacket`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemTaskInfo {
    /// `ItemTaskType` (59 mate death, 58 repair pets, 11 GM, ...).
    pub action: u8,
    pub items: Vec<TaskItem>,
    /// Sum of the `MoneyChange` tasks (copper; negative = deducted).
    pub money: i64,
}

/// Something the server told us that scenarios assert on, in arrival order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameEvent {
    /// `SCErrorMsgPacket` (error code).
    ErrorMsg(i16),
    ItemTask(ItemTaskInfo),
}

/// `ItemAction` values of the tasks [`parse_item_task`] reads.
mod item_action {
    pub const CHANGE_MONEY_AMOUNT: u8 = 1;
    pub const CHANGE_BANK_MONEY_AMOUNT: u8 = 2;
    pub const CREATE: u8 = 5;
    pub const TAKE: u8 = 6;
    pub const UPDATE_DETAIL: u8 = 9;
    pub const CHANGE_OWNER: u8 = 15;
}

/// Best-effort decode of an `SCItemTaskSuccessPacket` body (`[u8 reason][u8 count]`, then the
/// tasks, see `models/item_task.rs` of the game crate). Reading stops at the first task kind it
/// does not know or on truncation; what was read so far is returned.
pub fn parse_item_task(body: &[u8]) -> ItemTaskInfo {
    fn details(s: &mut PacketStream) -> anyhow::Result<Option<(i32, u8, u8)>> {
        let len = s.read_i16()?;
        if len != 128 {
            bail!("task details length {len}");
        }
        let mut block = [0u8; 128];
        for b in &mut block {
            *b = s.read_u8()?;
        }
        // detail type 3 = SummonMate: [i32 exp][u8 dead][u8 level]
        Ok((block[0] == 3).then(|| {
            (
                i32::from_le_bytes([block[1], block[2], block[3], block[4]]),
                block[5],
                block[6],
            )
        }))
    }
    fn read(s: &mut PacketStream, info: &mut ItemTaskInfo) -> anyhow::Result<()> {
        info.action = s.read_u8()?;
        let count = s.read_u8()?;
        for _ in 0..count {
            let task = s.read_u8()?;
            match task {
                item_action::CHANGE_MONEY_AMOUNT | item_action::CHANGE_BANK_MONEY_AMOUNT => {
                    info.money += i64::from(s.read_i32()?);
                }
                item_action::CREATE | item_action::CHANGE_OWNER | item_action::TAKE => {
                    s.read_u8()?; // slot type
                    s.read_u8()?; // slot
                    let template = s.read_u32()?;
                    let id = s.read_u64()?;
                    info.items.push(TaskItem {
                        task,
                        template,
                        id,
                        mate: None,
                    });
                    s.read_u8()?; // grade
                    s.read_u8()?; // flags
                    s.read_i32()?; // count
                    let mate = details(s)?;
                    if let Some(item) = info.items.last_mut() {
                        item.mate = mate;
                    }
                    s.read_i64()?; // create time
                    s.read_i32()?; // lifespan
                    s.read_u32()?; // made unit id
                    s.read_u8()?; // world id
                    s.read_i64()?; // unsecure time
                    s.read_i64()?; // unpack time
                }
                item_action::UPDATE_DETAIL => {
                    s.read_u8()?; // slot type
                    s.read_u8()?; // slot
                    let id = s.read_u64()?;
                    let mate = details(s)?;
                    info.items.push(TaskItem {
                        task,
                        template: 0,
                        id,
                        mate,
                    });
                }
                _ => break,
            }
        }
        Ok(())
    }
    let mut s = PacketStream::from_bytes(body.to_vec());
    let mut info = ItemTaskInfo::default();
    let _ = read(&mut s, &mut info);
    info
}

/// What the bot learned from the server so far.
#[derive(Debug, Default)]
pub struct GameState {
    pub token: u32,
    pub stream_port: u16,
    pub last_change_state: Option<i32>,
    pub characters: Vec<CharInfo>,
    pub char_list_done: bool,
    pub selected_char: u32,
    pub self_obj: u32,
    pub self_pos: (f32, f32, f32),
    /// Last position actually received from the server, independent of local movement sends.
    pub observed_self_pos: Option<(f32, f32, f32)>,
    pub observation_seq: u64,
    pub observation_errors: u64,
    pub units: HashMap<u32, UnitInfo>,
    /// Doodads announced by `SCDoodad(s)CreatedPacket` and not removed since.
    pub doodads: HashMap<u32, DoodadInfo>,
    /// First item of the bag (`SCCharacterInvenContentsPacket`, slot type 2): `(item id, slot)`.
    pub bag_items: Vec<(u64, u8, u32)>,
    pub bag_slots: u8,
    /// Latest `SCUnitPointsPacket` per unit: `(hp, mp)` (the wire carries them times 100).
    pub unit_points: HashMap<u32, (i32, i32)>,
    /// Units announced dead by `SCUnitDeathPacket` (and not removed since).
    pub dead_units: HashSet<u32>,
    /// Last `SCMateSpawnedPacket`.
    pub mate_spawned: Option<MateSpawn>,
    /// Number of `SCMateSpawnedPacket`s seen so far.
    pub mate_spawn_count: u32,
    /// `SCErrorMsgPacket`s and `SCItemTaskSuccessPacket`s in arrival order; scenarios remember
    /// `events.len()` before an action and inspect what was appended.
    pub events: Vec<GameEvent>,
    /// Party / guild notifications in arrival order ([`crate::social`]), so `await` also sees
    /// packets an earlier `drain` already read.
    pub social: Vec<crate::social::SocialEvent>,
    /// Position of the last `SCTeleportUnitPacket` (a GM `/move` only tells the client to go
    /// there; the server adopts the position from the client's next movement packet).
    pub last_teleport: Option<(f32, f32, f32)>,
    pub disconnected: bool,
}

impl GameState {
    /// Updates the state from a packet.
    pub fn observe(&mut self, p: &Packet) -> anyhow::Result<()> {
        self.observation_seq += 1;
        if let Some(ev) = crate::social::decode(p) {
            self.social.push(ev);
        }
        let mut s = p.stream();
        let st = self;
        if p.is(LEVEL_PROXY, pp::ChangeStatePacket) {
            st.last_change_state = Some(s.read_i32()?);
        } else if p.is_sc(sc::X2EnterWorldResponsePacket) {
            let _reason = s.read_i16()?;
            let _gm = s.read_bool()?;
            st.token = s.read_u32()?;
            st.stream_port = s.read_u16()?;
        } else if p.is_sc(sc::SCCharacterListPacket) {
            let last = s.read_bool()?;
            let n = s.read_u8()?;
            // Parse atomically: a malformed later entry must not authorize a partial list.
            let characters = (0..n)
                .map(|_| read_character(&mut s))
                .collect::<anyhow::Result<Vec<_>>>()?;
            anyhow::ensure!(s.remaining() == 0, "trailing character-list bytes");
            for character in characters {
                if !st.characters.iter().any(|c| c.id == character.id) {
                    st.characters.push(character);
                }
            }
            st.char_list_done |= last;
        } else if p.is_sc(sc::SCCreateCharacterResponsePacket) {
            let id = s.read_u32()?;
            let name = s.read_string()?;
            st.characters.push(CharInfo { id, name });
        } else if p.is_sc(sc::SCUnitStatePacket) {
            let obj_id = s.read_bc()?;
            let name = s.read_string()?;
            let unit_type = s.read_u8()?;
            let mut tl_id = 0;
            let id = match unit_type {
                0 => {
                    let id = s.read_u32()?;
                    s.read_u64()?;
                    id
                }
                1 => {
                    s.read_bc()?;
                    let tpl = s.read_u32()?;
                    s.read_u32()?;
                    s.read_u8()?;
                    tpl
                }
                2 => {
                    // Slave: db id, tl id, template id, summoner character id.
                    s.read_u32()?;
                    tl_id = s.read_u16()?;
                    let tpl = s.read_u32()?;
                    s.read_u32()?;
                    tpl
                }
                5 => {
                    // Mate: tl id, template id, owner character id.
                    tl_id = s.read_u16()?;
                    let tpl = s.read_u32()?;
                    s.read_u32()?;
                    tpl
                }
                _ => return Ok(()),
            };
            let _owner = s.read_string()?;
            let pos = s.read_position()?;
            if unit_type == 0 && id == st.selected_char && id != 0 {
                st.self_obj = obj_id;
                st.self_pos = pos;
                st.observed_self_pos = Some(pos);
            }
            st.units.insert(
                obj_id,
                UnitInfo {
                    obj_id,
                    name,
                    unit_type,
                    id,
                    pos,
                    tl_id,
                },
            );
        } else if p.is_sc(sc::SCOneUnitMovementPacket) {
            // objId, MoveTypeEnum (1 = Unit), time, flags [+ scType, phase], position.
            let obj_id = s.read_bc()?;
            if s.read_u8()? == 1 {
                s.read_u32()?;
                let flags = s.read_u8()?;
                if flags & 0x10 != 0 {
                    s.read_u32()?;
                    s.read_u8()?;
                }
                let pos = s.read_position()?;
                if obj_id == st.self_obj {
                    st.observed_self_pos = Some(pos);
                }
                if let Some(u) = st.units.get_mut(&obj_id) {
                    u.pos = pos;
                }
            }
        } else if p.is_sc(sc::SCTeleportUnitPacket) {
            // reason, error message, position, z2
            s.read_u8()?;
            s.read_i16()?;
            let pos = s.read_position()?;
            st.last_teleport = Some(pos);
            st.self_pos = pos;
            st.observed_self_pos = Some(pos);
        } else if p.is_sc(sc::SCDoodadCreatedPacket) {
            let d = read_doodad(&mut s)?;
            st.doodads.insert(d.obj_id, d);
        } else if p.is_sc(sc::SCDoodadsCreatedPacket) {
            let n = s.read_u8()?;
            for _ in 0..n {
                let d = read_doodad(&mut s)?;
                st.doodads.insert(d.obj_id, d);
            }
        } else if p.is_sc(sc::SCDoodadRemovedPacket) {
            st.doodads.remove(&s.read_bc()?);
        } else if p.is_sc(sc::SCDoodadsRemovedPacket) {
            let n = s.read_u16()?;
            s.read_bool()?; // last
            for _ in 0..n {
                let id = s.read_bc()?;
                s.read_bool()?;
                st.doodads.remove(&id);
            }
        } else if p.is_sc(sc::SCDoodadPhaseChangedPacket) {
            let id = s.read_bc()?;
            let group = s.read_u32()?;
            if let Some(d) = st.doodads.get_mut(&id) {
                d.func_group_id = group;
            }
        } else if p.is_sc(sc::SCUnitsRemovedPacket) {
            let n = s.read_u16()?;
            for _ in 0..n {
                let id = s.read_bc()?;
                st.units.remove(&id);
                st.unit_points.remove(&id);
                st.dead_units.remove(&id);
            }
        } else if p.is_sc(sc::SCUnitPointsPacket) {
            let obj_id = s.read_bc()?;
            let hp = s.read_i32()? / 100;
            let mp = s.read_i32()? / 100;
            st.unit_points.insert(obj_id, (hp, mp));
        } else if p.is_sc(sc::SCUnitDeathPacket) {
            st.dead_units.insert(s.read_bc()?);
        } else if p.is_sc(sc::SCErrorMsgPacket) {
            st.events.push(GameEvent::ErrorMsg(s.read_i16()?));
        } else if p.is_sc(sc::SCItemTaskSuccessPacket) {
            st.events
                .push(GameEvent::ItemTask(parse_item_task(&p.body)));
        } else if p.is_sc(sc::SCMateSpawnedPacket) {
            st.mate_spawned = Some(MateSpawn {
                tl_id: s.read_u16()?,
                db_id: s.read_u32()?,
                item_id: s.read_u64()?,
                user_state: s.read_u8()?,
            });
            st.mate_spawn_count += 1;
        } else if p.is_sc(sc::SCCharacterInvenContentsPacket) {
            let slot_type = s.read_u8()?;
            let _num_chunks = s.read_u8()?;
            let chunk = s.read_u8()?;
            if slot_type == 2 {
                // 10 items per chunk; only leading items can be located (variable layout).
                let tpl = s.read_u32()?;
                if tpl != 0 {
                    let id = s.read_u64()?;
                    st.bag_items.push((id, chunk * 10, tpl));
                }
            }
        }
        Ok(())
    }
}

pub struct GameClient {
    writer: Option<FramedWrite<Box<dyn AsyncWrite + Unpin + Send>, FrameCodec>>,
    reader: SocketReader,
    rx: mpsc::Receiver<Vec<u8>>,
    expanded: VecDeque<Packet>,
    rec: Recorder,
    count: u8,
    pub account_id: u32,
    pub state: GameState,
    pub timeout: Duration,
    /// Keep-alive: the servers drop connections without a `PingPacket` for 30 s.
    pub ping_every: Duration,
    last_ping: Option<Instant>,
    started: Instant,
}

/// Aborting a client (including a cancelled connection attempt) must release the read half.
struct SocketReader(Option<tokio::task::JoinHandle<()>>);

impl SocketReader {
    async fn close(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for SocketReader {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl GameClient {
    pub async fn connect(addr: &str, rec: Recorder) -> anyhow::Result<Self> {
        let sock = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to game server {addr}"))?;
        sock.set_nodelay(true)?;
        let (rd, wr) = sock.into_split();
        Ok(Self::from_parts(rd, wr, rec))
    }

    /// Use the same packet/state client over an owned async transport (including in-memory
    /// deterministic fixtures). Production TCP keeps its native owned socket halves.
    pub fn from_io(
        io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        rec: Recorder,
    ) -> Self {
        let (rd, wr) = tokio::io::split(io);
        Self::from_parts(rd, wr, rec)
    }

    fn from_parts(
        rd: impl AsyncRead + Unpin + Send + 'static,
        wr: impl AsyncWrite + Unpin + Send + 'static,
        rec: Recorder,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let reader = tokio::spawn(async move {
            let mut reader = FramedRead::new(rd, FrameCodec);
            loop {
                match reader.next().await {
                    Some(Ok(frame)) => {
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) => {
                        tracing::warn!("game socket read failed");
                        break;
                    }
                    None => {
                        tracing::debug!("game socket closed by the server");
                        break;
                    }
                }
            }
        });
        Self {
            writer: Some(FramedWrite::new(Box::new(wr), FrameCodec)),
            reader: SocketReader(Some(reader)),
            rx,
            expanded: VecDeque::new(),
            rec,
            count: 0,
            account_id: 0,
            state: GameState::default(),
            timeout: Duration::from_secs(20),
            ping_every: Duration::from_secs(3),
            last_ping: None,
            started: Instant::now(),
        }
    }

    pub fn recorder(&self) -> &Recorder {
        &self.rec
    }

    /// Close both game socket halves, without sending `CSLeaveWorldPacket`.
    /// Awaiting the aborted reader confirms that its socket half was dropped.
    pub async fn disconnect_hard(mut self) {
        drop(self.writer.take());
        self.reader.close().await;
        // Earlier accepted events are already flushed. Drain already-delivered evidence
        // in order for a bounded cleanup slice; do not let an inbound backlog consume the
        // command's reporting reserve. No socket remains open while this work runs.
        let until = Instant::now() + Duration::from_millis(25);
        while Instant::now() < until {
            if let Some(packet) = self.expanded.pop_front() {
                self.accept(packet);
                continue;
            }
            let Ok(frame) = self.rx.try_recv() else {
                break;
            };
            let _ = self.decode(frame);
        }
        self.rx.close();
        self.state.disconnected = true;
    }

    /// Sends a level-1 (`CS*`) packet.
    pub async fn send_cs(&mut self, opcode: u16, body: &[u8]) -> anyhow::Result<()> {
        self.rec.push(
            Channel::Game,
            Dir::C2S,
            LEVEL_GAME,
            opcode,
            packet_name(Dir::C2S, LEVEL_GAME, opcode),
            body,
        );
        let mut p = Vec::with_capacity(body.len() + 6);
        p.extend_from_slice(&[0xDD, LEVEL_GAME, 0, self.count]);
        self.count = self.count.wrapping_add(1);
        p.extend_from_slice(&opcode.to_le_bytes());
        p.extend_from_slice(body);
        self.writer
            .as_mut()
            .context("game writer closed")?
            .send(p)
            .await?;
        Ok(())
    }

    /// Sends a level-2 (proxy) packet.
    pub async fn send_proxy(&mut self, opcode: u16, body: &[u8]) -> anyhow::Result<()> {
        self.rec.push(
            Channel::Game,
            Dir::C2S,
            LEVEL_PROXY,
            opcode,
            packet_name(Dir::C2S, LEVEL_PROXY, opcode),
            body,
        );
        let mut p = Vec::with_capacity(body.len() + 4);
        p.extend_from_slice(&[0xDD, LEVEL_PROXY]);
        p.extend_from_slice(&opcode.to_le_bytes());
        p.extend_from_slice(body);
        self.writer
            .as_mut()
            .context("game writer closed")?
            .send(p)
            .await?;
        Ok(())
    }

    fn decode(&mut self, frame: Vec<u8>) -> anyhow::Result<Option<Packet>> {
        if frame.len() > MAX_GAME_FRAME {
            bail!(BatchError::Limit);
        }
        if frame.len() < 4 || frame[0] != 0xdd {
            bail!("invalid game frame");
        }
        let level = frame[1];
        if level == LEVEL_COMPRESSED {
            let count = u16::from_le_bytes([frame[2], frame[3]]);
            if usize::from(count) > MAX_BATCH_PACKETS {
                bail!(BatchError::Limit);
            }
            let body = inflate_batch(&frame[4..])?;
            if self.rec.is_incremental() {
                // Validate the whole batch before any entry reaches a capture or state.
                self.expanded
                    .extend(split_game_batch(Dir::S2C, count, &body)?);
                return Ok(self.expanded.pop_front().map(|packet| self.accept(packet)));
            }
            // Ordinary scenario captures keep their existing aggregate inflated body.
            self.rec.push(
                Channel::Game,
                Dir::S2C,
                level,
                count,
                "CompressedGamePackets",
                &body,
            );
            return Ok(Some(Packet {
                level,
                opcode: count,
                body,
            }));
        }
        if !matches!(level, LEVEL_GAME | LEVEL_PROXY) {
            bail!("unsupported game frame level");
        }
        let (opcode, body) = if level == LEVEL_GAME {
            if frame.len() < 6 {
                bail!("short level-1 frame");
            }
            (
                u16::from_le_bytes([frame[4], frame[5]]),
                frame[6..].to_vec(),
            )
        } else {
            (
                u16::from_le_bytes([frame[2], frame[3]]),
                frame[4..].to_vec(),
            )
        };
        let p = Packet {
            level,
            opcode,
            body,
        };
        Ok(Some(self.accept(p)))
    }

    fn accept(&mut self, packet: Packet) -> Packet {
        self.rec.push(
            Channel::Game,
            Dir::S2C,
            packet.level,
            packet.opcode,
            packet.name(),
            &packet.body,
        );
        if self.observe(&packet).is_err() {
            self.state.observation_errors += 1;
            // Native/parser errors can retain input; never format their Display/Debug/cause.
            tracing::debug!(packet = packet.name(), "could not parse game packet");
        }
        packet
    }

    /// `PingPacket` (proxy): `tPhy`, `ping` and `local`, like the client's keep-alive.
    pub async fn ping(&mut self) -> anyhow::Result<()> {
        let ms = self.started.elapsed().as_millis() as i64;
        let mut s = PacketStream::new();
        s.write_i64(ms).write_i64(ms).write_u32(ms as u32);
        self.last_ping = Some(Instant::now());
        self.send_proxy(pp::PingPacket, s.as_bytes()).await
    }

    async fn keep_alive(&mut self) -> anyhow::Result<()> {
        if self.account_id != 0
            && self
                .last_ping
                .is_none_or(|t| t.elapsed() >= self.ping_every)
        {
            self.ping().await?;
        }
        Ok(())
    }

    /// Next packet, or `None` after `wait` without one. Sends keep-alive pings as needed.
    pub async fn recv_within(&mut self, wait: Duration) -> anyhow::Result<Option<Packet>> {
        self.keep_alive().await?;
        if let Some(packet) = self.expanded.pop_front() {
            return Ok(Some(self.accept(packet)));
        }
        let wait = wait.min(self.ping_every);
        match tokio::time::timeout(wait, self.rx.recv()).await {
            Err(_) => Ok(None),
            Ok(None) => {
                self.state.disconnected = true;
                Err(anyhow!("game server closed the connection"))
            }
            Ok(Some(frame)) => self.decode(frame),
        }
    }

    /// Reads packets until one matches `pred` (returned) or the timeout elapses.
    pub async fn wait_for<F>(&mut self, what: &str, mut pred: F) -> anyhow::Result<Packet>
    where
        F: FnMut(&Packet) -> bool,
    {
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("timed out waiting for {what}");
            }
            match self.recv_within(left).await? {
                Some(p) if pred(&p) => return Ok(p),
                Some(_) | None => {}
            }
        }
    }

    /// Waits for a level-1 packet with `opcode`.
    pub async fn wait_sc(&mut self, opcode: u16) -> anyhow::Result<Packet> {
        let name = packet_name(Dir::S2C, LEVEL_GAME, opcode);
        self.wait_for(name, |p| p.is_sc(opcode)).await
    }

    /// Reads everything that arrives until the connection is quiet for `quiet` (or `max`).
    pub async fn drain(&mut self, quiet: Duration, max: Duration) -> anyhow::Result<Vec<Packet>> {
        let deadline = Instant::now() + max;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(out);
            }
            match self.recv_within(quiet.min(left)).await? {
                Some(p) => out.push(p),
                None => return Ok(out),
            }
        }
    }

    /// Updates [`GameState`] from a packet.
    fn observe(&mut self, p: &Packet) -> anyhow::Result<()> {
        self.state.observe(p)
    }

    // ----- C2G packets -----

    /// `X2EnterWorldPacket`.
    pub async fn enter_world(&mut self, account_id: u32, cookie: u32) -> anyhow::Result<()> {
        self.account_id = account_id;
        let mut s = PacketStream::new();
        s.write_u32(0) // pFrom
            .write_u32(0) // pTo
            .write_u32(account_id)
            .write_u32(cookie)
            .write_i32(0) // zoneId
            .write_u8(0) // tb
            .write_u64(0); // revision
        self.send_cs(cs::X2EnterWorldPacket, s.as_bytes()).await
    }

    /// `FinishStatePacket` (proxy).
    pub async fn finish_state(&mut self, state: i32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_i32(state);
        self.send_proxy(pp::FinishStatePacket, s.as_bytes()).await
    }

    /// `CSListCharacterPacket`.
    pub async fn list_characters(&mut self) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_i32(0).write_bytes_sized(&[]);
        self.send_cs(cs::CSListCharacterPacket, s.as_bytes()).await
    }

    /// `CSCreateCharacterPacket`: Nuian (1) male (1) Fighter (ability 1), default appearance.
    pub async fn create_character(&mut self, name: &str) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_str(name).write_u8(1).write_u8(1);
        for _ in 0..7 {
            s.write_u32(0); // body items: template defaults
        }
        write_custom_model(&mut s);
        s.write_u8(1) // ability1: Fight
            .write_u8(11) // ability2: None
            .write_u8(11) // ability3: None
            .write_u8(1); // level
        self.send_cs(cs::CSCreateCharacterPacket, s.as_bytes())
            .await
    }

    /// `CSSelectCharacterPacket`.
    pub async fn select_character(&mut self, id: u32) -> anyhow::Result<()> {
        self.state.selected_char = id;
        let mut s = PacketStream::new();
        s.write_u32(id).write_bool(false).write_u8(0);
        self.send_cs(cs::CSSelectCharacterPacket, s.as_bytes())
            .await
    }

    /// `CSSpawnCharacterPacket` (visual options: none).
    /// `CSSpawnCharacterPacket` with the visual options the client sends (all flags set).
    pub async fn spawn_character(&mut self) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u8(0x1F) // stp, helmet, back holdable, cosplay, cosplay backpack
            .write_raw(&[0u8; 6])
            .write_bool(true)
            .write_bool(true)
            .write_bool(true)
            .write_bool(true);
        self.send_cs(cs::CSSpawnCharacterPacket, s.as_bytes()).await
    }

    pub async fn notify_in_game(&mut self) -> anyhow::Result<()> {
        self.send_cs(cs::CSNotifyInGamePacket, &[]).await
    }

    pub async fn notify_in_game_completed(&mut self) -> anyhow::Result<()> {
        self.send_cs(cs::CSNotifyInGameCompletedPacket, &[]).await
    }

    /// `CSMoveUnitPacket` with a `UnitMoveType` for our own character.
    pub async fn move_to(&mut self, pos: (f32, f32, f32), time: u32) -> anyhow::Result<()> {
        self.move_to_with_direction(pos, time, 0).await
    }

    /// Move into range while facing a unit, so angle restrictions and auto attacks apply.
    pub async fn move_to_facing(
        &mut self,
        pos: (f32, f32, f32),
        target: (f32, f32, f32),
        time: u32,
    ) -> anyhow::Result<()> {
        self.move_to_with_direction(pos, time, facing_direction(pos, target))
            .await
    }

    /// `CSMoveUnitPacket` for another object of ours (the mate we ride), without touching
    /// our own tracked position.
    pub async fn move_obj_to(
        &mut self,
        obj_id: u32,
        pos: (f32, f32, f32),
        time: u32,
    ) -> anyhow::Result<()> {
        let own = self.state.self_pos;
        self.move_obj_inner(obj_id, pos, time, 0).await?;
        self.state.self_pos = own;
        if let Some(u) = self.state.units.get_mut(&obj_id) {
            u.pos = pos;
        }
        Ok(())
    }

    async fn move_to_with_direction(
        &mut self,
        pos: (f32, f32, f32),
        time: u32,
        direction: i8,
    ) -> anyhow::Result<()> {
        let obj = self.state.self_obj;
        self.move_obj_inner(obj, pos, time, direction).await
    }

    async fn move_obj_inner(
        &mut self,
        obj_id: u32,
        pos: (f32, f32, f32),
        time: u32,
        direction: i8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(obj_id)
            .write_u8(1) // MoveTypeEnum.Unit
            .write_u32(time)
            .write_u8(0x02) // Moving
            .write_position(pos.0, pos.1, pos.2)
            .write_i16(0)
            .write_i16(0)
            .write_i16(0) // velocity
            .write_i8(0)
            .write_i8(0)
            .write_i8(direction) // yaw
            .write_i8(0)
            .write_i8(0)
            .write_i8(0) // delta movement
            .write_i8(1) // stance: idle
            .write_u8(0) // alertness
            .write_u8(0); // actor flags
        self.state.self_pos = pos;
        self.send_cs(cs::CSMoveUnitPacket, s.as_bytes()).await
    }

    /// `CSSendChatMessagePacket`.
    pub async fn chat(&mut self, chat_type: i16, message: &str) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_i16(chat_type)
            .write_i16(0)
            .write_i32(0)
            .write_str("") // target
            .write_str(message)
            .write_u8(0) // language
            .write_i32(0); // ability
        self.send_cs(cs::CSSendChatMessagePacket, s.as_bytes())
            .await
    }

    /// `CSStartSkillPacket`: unit caster (us) on a unit target.
    pub async fn start_skill(&mut self, skill_id: u32, target_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(0)
            .write_bc(self.state.self_obj)
            .write_u8(0)
            .write_bc(target_obj)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSSwapAbilityPacket` (replace ability `old` with `new` in the character's slots).
    pub async fn swap_ability(&mut self, old: u8, new: u8) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(self.state.self_obj)
            .write_u8(old)
            .write_u8(new)
            .write_bool(false); // auap
        self.send_cs(cs::CSSwapAbilityPacket, s.as_bytes()).await
    }

    /// `CSLearnSkillPacket`.
    pub async fn learn_skill(&mut self, skill_id: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id);
        self.send_cs(cs::CSLearnSkillPacket, s.as_bytes()).await
    }

    /// `CSCreateDoodadPacket` (0x0e6): plant / place doodad `id` from the bag item `item_id`.
    pub async fn create_doodad(
        &mut self,
        id: u32,
        pos: (f32, f32, f32),
        z_rot: f32,
        scale: f32,
        item_id: u64,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(id)
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.0))
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.1))
            .write_f32(pos.2)
            .write_f32(z_rot)
            .write_f32(scale)
            .write_u64(item_id);
        self.send_cs(cs::CSCreateDoodadPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket`: unit caster (us) on a doodad target (`SkillCastDoodadTarget`), what
    /// the client sends for a doodad interaction skill.
    pub async fn start_doodad_skill(
        &mut self,
        skill_id: u32,
        doodad_obj: u32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(0) // SkillCasterType.Unit
            .write_bc(self.state.self_obj)
            .write_u8(4) // SkillCastTargetType.Doodad
            .write_bc(doodad_obj)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket` cast from an inventory item (the item's use skill) on ourselves,
    /// as the client does for a summon item.
    pub async fn use_item_skill(
        &mut self,
        skill_id: u32,
        item_id: u64,
        item_template_id: u32,
    ) -> anyhow::Result<()> {
        let me = self.state.self_obj;
        self.start_item_skill(skill_id, item_id, item_template_id, me)
            .await
    }

    /// `CSStartSkillPacket` cast by an item (the 1.2 client's item-use skill): item caster
    /// (our object, the item id and template), unit target, no skill object.
    pub async fn start_item_skill(
        &mut self,
        skill_id: u32,
        item_id: u64,
        item_template_id: u32,
        target_obj: u32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(2) // SkillCasterType.Item
            .write_bc(self.state.self_obj)
            .write_u64(item_id)
            .write_u32(item_template_id)
            .write_u8(0) // type1
            .write_u32(0) // type2
            .write_u8(0) // SkillCastUnitTarget
            .write_bc(target_obj)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket` cast by an item on another item (`SkillCastItemTarget`): lunafrost /
    /// lunagem / enchanting scrolls. The caster is the item `item_id`, the target the item
    /// `target_item_id`.
    pub async fn start_item_skill_on_item(
        &mut self,
        skill_id: u32,
        item_id: u64,
        item_template_id: u32,
        target_item_id: u64,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(2) // SkillCasterType.Item
            .write_bc(self.state.self_obj)
            .write_u64(item_id)
            .write_u32(item_template_id)
            .write_u8(0) // type1
            .write_u32(0) // type2
            .write_u8(3) // SkillCastItemTarget
            .write_bc(self.state.self_obj)
            .write_u64(target_item_id)
            .write_u32(0) // type1
            .write_u8(0) // type2
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSSwapItemsPacket` between any two slots (`slot type` 1 = equipment, 2 = bag); an empty
    /// destination slot has item id 0.
    pub async fn swap_slots(
        &mut self,
        from: (u64, u8, u8),
        to: (u64, u8, u8),
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u64(from.0)
            .write_u64(to.0)
            .write_u8(from.1)
            .write_u8(from.2)
            .write_u8(to.1)
            .write_u8(to.2);
        self.send_cs(cs::CSSwapItemsPacket, s.as_bytes()).await
    }

    /// `CSTeleportEndedPacket`: the client's answer to `SCTeleportUnitPacket` (the server
    /// then accepts our movement packets again and re-announces the objects around).
    pub async fn teleport_ended(&mut self, pos: (f32, f32, f32)) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.0))
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.1))
            .write_f32(pos.2)
            .write_raw(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x3f]); // identity
        self.send_cs(cs::CSTeleportEndedPacket, s.as_bytes()).await
    }

    /// `CSRemoveMatePacket`: dismiss the mate with transport id `tl_id`.
    pub async fn remove_mate(&mut self, tl_id: u16) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u16(tl_id);
        self.send_cs(cs::CSRemoveMatePacket, s.as_bytes()).await
    }

    /// `CSRepairPetItemsPacket`: the stablehand revive of dead summon items; the NPC must be the
    /// current interaction object (`CSInteractNPCPacket` first).
    pub async fn repair_pet_items(&mut self, npc_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj);
        self.send_cs(cs::CSRepairPetItemsPacket, s.as_bytes()).await
    }

    /// `CSInteractNPCPacket`.
    pub async fn interact_npc(&mut self, npc_obj: u32, target_changed: bool) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj).write_bool(target_changed);
        self.send_cs(cs::CSInteractNPCPacket, s.as_bytes()).await
    }

    /// `CSStartInteractionPacket` (asks the NPC for its interaction skill list).
    pub async fn start_interaction(&mut self, npc_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj)
            .write_bc(npc_obj)
            .write_i32(0) // extra info
            .write_i32(0) // pick id
            .write_u8(1) // mouse button
            .write_i32(0); // modifier keys
        self.send_cs(cs::CSStartInteractionPacket, s.as_bytes())
            .await
    }

    /// Select the unit used by auto attacks, as the real client does before attacking.
    pub async fn change_target(&mut self, target_obj: u32) -> anyhow::Result<()> {
        let mut packet = PacketStream::new();
        packet.write_bc(target_obj);
        self.send_cs(cs::CSChangeTargetPacket, packet.as_bytes())
            .await
    }

    /// `CSSwapItemsPacket` (bag -> bag).
    pub async fn swap_items(
        &mut self,
        from_item: u64,
        from_slot: u8,
        to_item: u64,
        to_slot: u8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u64(from_item)
            .write_u64(to_item)
            .write_u8(2)
            .write_u8(from_slot)
            .write_u8(2)
            .write_u8(to_slot);
        self.send_cs(cs::CSSwapItemsPacket, s.as_bytes()).await
    }

    /// `CSStartQuestContextPacket` without an NPC / doodad / sphere.
    pub async fn start_quest(&mut self, quest_id: u32, npc_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(quest_id)
            .write_bc(npc_obj)
            .write_bc(0)
            .write_u32(0);
        self.send_cs(cs::CSStartQuestContextPacket, s.as_bytes())
            .await
    }

    /// `CSLeaveWorldPacket` (0 = quit, 1 = character select, 2 = server select).
    pub async fn leave_world(&mut self, target: u8) -> anyhow::Result<()> {
        self.send_cs(cs::CSLeaveWorldPacket, &[target]).await
    }

    /// Closest unit of `unit_type` to our position.
    pub fn nearest_unit(&self, unit_type: u8) -> Option<UnitInfo> {
        let me = self.state.self_pos;
        self.state
            .units
            .values()
            .filter(|u| u.unit_type == unit_type && u.obj_id != self.state.self_obj)
            .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
            .cloned()
    }

    /// A unit of `unit_type` within `range` picked independently of object ids and wandering:
    /// the lowest template / character id, then the closest. Used where two servers must be
    /// driven identically (parity runs).
    pub fn stable_unit(&self, unit_type: u8, range: f32) -> Option<UnitInfo> {
        let me = self.state.self_pos;
        self.state
            .units
            .values()
            .filter(|u| {
                u.unit_type == unit_type
                    && u.obj_id != self.state.self_obj
                    && dist2(u.pos, me) <= range * range
            })
            .min_by(|a, b| {
                a.id.cmp(&b.id)
                    .then(dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
            })
            .cloned()
    }
}

#[cfg(test)]
mod socket_reader_tests {
    use super::SocketReader;
    use tokio::sync::oneshot;

    struct OnDrop(Option<oneshot::Sender<()>>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    async fn reader() -> (SocketReader, oneshot::Receiver<()>) {
        let (started, ready) = oneshot::channel();
        let (stopped, done) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _resource = OnDrop(Some(stopped));
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        (SocketReader(Some(task)), done)
    }

    #[tokio::test]
    async fn explicit_close_and_cancelled_owner_release_reader_resources() {
        let (mut task, stopped) = reader().await;
        task.close().await;
        assert!(stopped.await.is_ok());
        task.close().await;

        let (task, stopped) = reader().await;
        drop(task);
        assert!(stopped.await.is_ok());
    }
}

#[cfg(test)]
mod observed_position_tests {
    use super::*;

    #[test]
    fn server_observation_does_not_report_a_locally_predicted_position() {
        let mut state = GameState {
            self_obj: 42,
            self_pos: (900.0, 900.0, 900.0),
            ..GameState::default()
        };
        assert_eq!(state.observed_self_pos, None);
        let mut body = PacketStream::new();
        body.write_bc(42)
            .write_u8(1)
            .write_u32(123)
            .write_u8(0)
            .write_position(10.0, 20.0, 30.0);
        state
            .observe(&Packet {
                level: LEVEL_GAME,
                opcode: sc::SCOneUnitMovementPacket,
                body: body.as_bytes().to_vec(),
            })
            .unwrap();
        // The native position codec quantizes the authored Z=30 to 29.9995.
        assert_eq!(state.observed_self_pos, Some((10.0, 20.0, 29.9995)));
        assert_eq!(state.self_pos, (900.0, 900.0, 900.0));
        assert_eq!(state.observation_seq, 1);
    }
}

fn dist2(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    dx * dx + dy * dy + dz * dz
}

fn facing_direction(pos: (f32, f32, f32), target: (f32, f32, f32)) -> i8 {
    let (dx, dy) = (target.0 - pos.0, target.1 - pos.1);
    if dx == 0.0 && dy == 0.0 {
        return 0;
    }
    let yaw = (f64::from(dy).atan2(f64::from(dx)) - std::f64::consts::FRAC_PI_2
        + std::f64::consts::PI)
        .rem_euclid(std::f64::consts::TAU)
        - std::f64::consts::PI;
    aaemu_game::models::movement::convert_radian_to_direction(yaw)
}

/// `UnitCustomModelParams` with a full face block (type 3), all defaults.
fn write_custom_model(s: &mut PacketStream) {
    s.write_u8(3) // UnitCustomModelType.Face
        .write_u32(0) // hair color
        .write_u32(0) // skin color
        .write_u32(0); // model id
                       // UnitCustomFace
    s.write_u32(0)
        .write_f32(0.0)
        .write_f32(0.0)
        .write_f32(0.0)
        .write_i16(0)
        .write_i16(0);
    for _ in 0..4 {
        s.write_u32(0).write_f32(0.0); // fixed decal assets
    }
    for _ in 0..3 {
        s.write_u32(0); // diffuse, normal, eyelash maps
    }
    s.write_f32(0.0); // normal map weight
    for _ in 0..5 {
        s.write_u32(0); // lip, pupils, eyebrow, deco colors
    }
    s.write_bytes_sized(&[0u8; 128]); // modifier
}

#[cfg(test)]
mod facing_tests {
    use super::*;

    #[test]
    fn movement_faces_each_compass_target_using_the_1_2_yaw() {
        let origin = (0.0, 0.0, 0.0);
        for (target, direction) in [
            ((0.0, 1.0, 0.0), 0),
            ((-1.0, 0.0, 0.0), 32),
            ((1.0, 0.0, 0.0), -32),
            ((0.0, -1.0, 0.0), 64),
        ] {
            assert_eq!(facing_direction(origin, target), direction);
        }
    }
}

#[cfg(test)]
mod compressed_batch_tests {
    use super::*;
    use crate::record::Event;
    use aaemu_game::net::packet::encode;
    use aaemu_game::packets::g2c::{SCReconnectAuthPacket, X2EnterWorldResponsePacket};
    use aaemu_game::systems::skills::packets::*;
    use aaemu_game::systems::skills::types::{
        CastAction, HealHitType, HealType, PlotObject, SkillCaster,
    };
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Bytes(Arc<Mutex<Vec<u8>>>);
    impl Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn safe_native_batch() -> CompressedGamePackets {
        let mut batch = CompressedGamePackets::new();
        batch.add(&SCSkillCooldownResetPacket {
            obj_id: 42,
            skill_id: 15307,
            tag_id: 0,
            gcd: false,
        });
        batch.add(&SCPlotEventPacket {
            tl: 7,
            event_id: 8,
            skill_id: 15307,
            caster: PlotObject::Unit(42),
            target: PlotObject::Position {
                x: 1.0,
                y: 2.0,
                z: 3.0,
                roll: 0,
                pitch: 0,
                yaw: 0,
            },
            obj_id: 43,
            casting_time: 100,
            flag: 8,
            item_id: 100,
            target_unit_count: 2,
        });
        batch.add(&SCUnitDamagedPacket::new(
            CastAction::Plot {
                plot_id: 1,
                tl_id: 7,
                event_id: 8,
                skill_id: 15307,
            },
            SkillCaster::Item {
                obj_id: 42,
                item_id: 100,
                item_template_id: 23398,
                type1: 0,
                type2: 0,
            },
            42,
            43,
            20000,
            1000,
        ));
        batch.add(&SCUnitHealedPacket {
            cast_action: CastAction::Buff {
                buff_id: 1,
                owner_obj_id: 42,
                index: 2,
            },
            skill_caster: SkillCaster::Mount {
                obj_id: 42,
                mount_skill_template_id: 3,
            },
            target_id: 43,
            heal_type: HealType::Health,
            heal_hit_type: HealHitType::HealHit,
            value: 400,
        });
        batch.add(&SCBuffCreatedPacket {
            skill_caster: SkillCaster::Unit(42),
            caster_id: 1,
            owner_obj_id: 42,
            index: 2,
            buff_id: 3,
            caster_level: 10,
            ab_level: 10,
            skill_id: 15307,
            charge: 300,
            duration: 20000,
            tick: 100.0,
        });
        batch.add(&SCSkillEndedPacket { tl: 7 });
        batch.add(&SCBuffRemovedPacket {
            obj_id: 42,
            index: 2,
        });
        batch.add(&aaemu_game::packets::world::SCErrorMsgPacket::new(
            7, 0, false,
        ));
        let mut started = SCSkillStartedPacket::new(
            15307,
            7,
            SkillCaster::Unit(42),
            aaemu_game::systems::skills::types::SkillCastTarget::Position {
                obj_id: 0,
                x: 1.0,
                y: 2.0,
                z: 3.0,
                rot: 0.0,
                obj_id1: 0,
                obj_id2: 0,
            },
            aaemu_game::systems::skills::types::SkillObject::Text("safe".into()),
        );
        started
            .set_skill_result(2)
            .set_result_ushort(300)
            .set_result_uint(400);
        batch.add(&started);
        batch.add(&SCSkillFiredPacket {
            id: 15307,
            tl: 7,
            caster: SkillCaster::Unit(42),
            target: aaemu_game::systems::skills::types::SkillCastTarget::Doodad(43),
            skill_object: aaemu_game::systems::skills::types::SkillObject::None,
            computed_delay: 100,
            channeling_time: 0,
            fire_anim_id: 1,
        });
        batch
    }

    fn client(rec: Recorder) -> GameClient {
        let (io, _peer) = tokio::io::duplex(64 * 1024);
        GameClient::from_io(io, rec)
    }

    fn control_events(frame: Vec<u8>) -> (Vec<Event>, Vec<Event>) {
        let sink = Bytes::default();
        let rec = Recorder::incremental_game_to(sink.clone());
        rec.control_game_ready(true);
        let mut game = client(rec.clone());
        game.decode(frame).unwrap();
        while let Some(packet) = game.expanded.pop_front() {
            game.accept(packet);
        }
        let output = rec.take_live_events().unwrap();
        let capture = String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (output, capture)
    }

    #[tokio::test]
    async fn real_repo_encoded_plot_combat_and_cooldown_batches_reach_both_sinks() {
        let batch = safe_native_batch();
        let (events, capture) = control_events(encode(&batch));
        assert_eq!(events, capture);
        assert_eq!(events.len(), batch.len());
        for (event, (opcode, body)) in events.iter().zip(&batch.packets) {
            assert_eq!(event.level, LEVEL_GAME);
            assert_eq!(event.opcode, *opcode);
            assert_eq!(event.body_bytes(), *body);
        }
        let mut large = CompressedGamePackets::new();
        for skill in 0..1024 {
            large.add(&SCSkillCooldownResetPacket {
                obj_id: 42,
                skill_id: skill,
                tag_id: 0,
                gcd: false,
            });
        }
        assert_eq!(control_events(encode(&large)).0.len(), 1024);
        assert!(control_events(encode(&CompressedGamePackets::new()))
            .0
            .is_empty());
    }

    #[tokio::test]
    async fn mixed_auth_batches_only_publish_the_safe_inner_packets() {
        const TOKEN: u32 = u32::from_le_bytes(*b"KEY!");
        let safe = safe_native_batch();
        let mut mixed = CompressedGamePackets::new();
        mixed.add(&SCReconnectAuthPacket { token: TOKEN });
        mixed.packets.extend(safe.packets.clone());
        mixed.add(&X2EnterWorldResponsePacket {
            reason: 0,
            gm: true,
            token: TOKEN,
            port: 25250,
            world_time: 0,
        });
        let (events, capture) = control_events(encode(&mixed));
        assert_eq!(events, capture);
        assert_eq!(events.len(), safe.len());
        for event in &events {
            assert_eq!(event.level, 1);
            assert!(!matches!(
                event.opcode,
                sc::SCReconnectAuthPacket | sc::X2EnterWorldResponsePacket
            ));
            assert!(!event.body.contains("4b455921"));
        }
        // A recorder receiving an already-inflated aggregate uses the identical splitter.
        let sink = Bytes::default();
        let rec = Recorder::incremental_game_to(sink.clone());
        rec.control_game_ready(true);
        rec.push(
            Channel::Game,
            Dir::S2C,
            4,
            mixed.len() as u16,
            "CompressedGamePackets",
            &mixed.raw_payload(),
        );
        let direct = rec.take_live_events().unwrap();
        assert_eq!(
            direct
                .iter()
                .map(|e| (e.opcode, e.body.clone()))
                .collect::<Vec<_>>(),
            events
                .iter()
                .map(|e| (e.opcode, e.body.clone()))
                .collect::<Vec<_>>()
        );
        assert!(!String::from_utf8_lossy(&sink.0.lock().unwrap()).contains("4b455921"));
        // C2S's credential transition is X2EnterWorld (native 29-byte body).
        let mut auth = PacketStream::new();
        auth.write_u32(0)
            .write_u32(0)
            .write_u32(1)
            .write_u32(TOKEN)
            .write_i32(0)
            .write_u8(0)
            .write_u64(0);
        let mut inbound = CompressedGamePackets::new();
        inbound
            .packets
            .push((cs::X2EnterWorldPacket, auth.into_bytes()));
        let sink = Bytes::default();
        let rec = Recorder::incremental_game_to(sink.clone());
        rec.control_game_ready(true);
        rec.push(
            Channel::Game,
            Dir::C2S,
            4,
            1,
            "CompressedGamePackets",
            &inbound.raw_payload(),
        );
        assert!(rec.take_live_events().unwrap().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
        // Ordinary private scenario captures preserve their existing inflated aggregate.
        let ordinary = Recorder::new(true);
        let mut game = client(ordinary.clone());
        let packet = game.decode(encode(&mixed)).unwrap().unwrap();
        assert_eq!(packet.level, 4);
        assert_eq!(ordinary.events()[0].body_bytes(), mixed.raw_payload());
        assert_eq!(ordinary.events().len(), 1);
    }

    fn redacted(error: &anyhow::Error) {
        for text in [
            format!("{error}"),
            format!("{error:#}"),
            format!("{error:?}"),
        ] {
            for secret in ["KEY!", "4b455921", "75, 69, 89, 33"] {
                assert!(!text.contains(secret), "payload in formatted parser error");
            }
        }
    }

    #[tokio::test]
    async fn malformed_compressed_auth_never_reaches_errors_or_debug_logs() {
        let mut raw = PacketStream::new();
        raw.write_u16(0)
            .write_u16(sc::SCReconnectAuthPacket)
            .write_raw(b"KEY!");
        let mut bad = deflate_stored(raw.as_bytes());
        bad[0] = 0; // valid non-final stored block produces the secret before the invalid block
        bad.push(7); // final reserved BTYPE=3
        let mut frame = vec![0xdd, 4, 1, 0];
        frame.extend_from_slice(&bad);
        let logs = Bytes::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let sink = Bytes::default();
        let rec = Recorder::incremental_game_to(sink.clone());
        rec.control_game_ready(true);
        let mut game = client(rec.clone());
        let error = game.decode(frame).unwrap_err();
        assert_eq!(
            error.downcast_ref::<BatchError>(),
            Some(&BatchError::Inflate)
        );
        redacted(&error);
        tracing::debug!(debug=?error,display=%error,"sanitized batch error");
        // The observation path also avoids formatting errors with retained native input.
        game.decode(aaemu_game::net::packet::encode_raw(
            1,
            sc::X2EnterWorldResponsePacket,
            b"KEY!",
        ))
        .unwrap();
        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("sanitized batch error"));
        for secret in ["KEY!", "4b455921", "75, 69, 89, 33"] {
            assert!(!logs.contains(secret));
        }
        assert!(rec.take_live_events().unwrap().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn count_nested_unknown_truncated_and_trailing_batches_fail_atomically() {
        let safe = safe_native_batch();
        let raw = safe.raw_payload();
        let mut nonzero = raw.clone();
        nonzero[0] = 1;
        let mut trailing = raw.clone();
        trailing.push(0xff);
        let mut inner = CompressedGamePackets::new();
        inner.add(&SCSkillEndedPacket { tl: 7 });
        let mut nested = CompressedGamePackets::new();
        nested.add(&inner);
        let cases = [
            (safe.len() as u16, nonzero),
            (safe.len() as u16, trailing),
            (safe.len() as u16 + 1, raw.clone()),
            (safe.len() as u16 - 1, raw.clone()),
            (safe.len() as u16, raw[..raw.len() - 1].to_vec()),
            (1, vec![0, 0, 0xff, 0xff, b'K', b'E', b'Y', b'!']),
            (1, nested.raw_payload()),
            (1, encode(&inner)), // nested DD04 level header is not an inner zero/opcode header
        ];
        for (count, body) in cases {
            let sink = Bytes::default();
            let rec = Recorder::incremental_game_to(sink.clone());
            rec.control_game_ready(true);
            let mut game = client(rec.clone());
            let mut frame = vec![0xdd, 4];
            frame.extend_from_slice(&count.to_le_bytes());
            frame.extend_from_slice(&deflate_stored(&body));
            redacted(&game.decode(frame).unwrap_err());
            assert!(rec.take_live_events().unwrap().is_empty());
            assert!(sink.0.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn inflater_refuses_bombs_and_frame_count_limits_before_unbounded_growth() {
        // Only cap+1 plaintext bytes are needed to test a bomb; no huge fixture allocation.
        let bomb = miniz_oxide::deflate::compress_to_vec(&vec![b'A'; MAX_BATCH_BYTES + 1], 9);
        assert!(bomb.len() < MAX_GAME_FRAME - 4);
        assert_eq!(inflate_batch(&bomb).unwrap_err(), BatchError::Limit);
        let exact = miniz_oxide::deflate::compress_to_vec(&vec![b'A'; MAX_BATCH_BYTES], 9);
        assert_eq!(inflate_batch(&exact).unwrap().len(), MAX_BATCH_BYTES);
        let mut trailing = exact;
        trailing.extend_from_slice(&[0xaa; 64]);
        assert_eq!(inflate_batch(&trailing).unwrap_err(), BatchError::Inflate);
        let mut game = client(Recorder::new(false));
        let error = game.decode(vec![0; MAX_GAME_FRAME + 1]).unwrap_err();
        redacted(&error);
        let mut frame = vec![0xdd, 4];
        frame.extend_from_slice(&((MAX_BATCH_PACKETS + 1) as u16).to_le_bytes());
        frame.extend_from_slice(&deflate_stored(&[]));
        assert_eq!(
            game.decode(frame).unwrap_err().downcast_ref::<BatchError>(),
            Some(&BatchError::Limit)
        );
        assert_eq!(
            split_game_batch(Dir::S2C, 0, &vec![0; MAX_BATCH_BYTES + 1]).unwrap_err(),
            BatchError::Limit
        );
    }
}
