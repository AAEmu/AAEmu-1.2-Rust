use std::collections::{HashMap, HashSet};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{pp, sc};
use anyhow::bail;

use super::{Packet, LEVEL_GAME, LEVEL_PROXY};

/// A character of `SCCharacterListPacket` / `SCCreateCharacterResponsePacket` /
/// `SCEditCharacterResponsePacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharInfo {
    pub id: u32,
    pub name: String,
    pub appearance: Appearance,
}

/// The customizing ids of a character block (`UnitCustomModelParams`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Appearance {
    pub kind: u8,
    pub hair_color_id: u32,
    pub skin_color_id: u32,
    pub model_id: u32,
}

/// Consume one complete native `Character.Write` record. The list has no entry lengths;
/// stopping after id/name would parse the next character from equipment or model bytes.
pub(crate) fn read_character(s: &mut PacketStream) -> anyhow::Result<CharInfo> {
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
    let appearance: UnitCustomModelParams = s.read()?;
    let appearance = Appearance {
        kind: appearance.kind.0,
        hair_color_id: appearance.hair_color_id,
        skin_color_id: appearance.skin_color_id,
        model_id: appearance.model_id,
    };
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
    Ok(CharInfo {
        id,
        name,
        appearance,
    })
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
    /// Last `SCCountUnreadMailPacket` (`0x116`): (sent, received, mia, commercial). Sent at spawn
    /// and includes mail that arrived while the character was offline.
    pub count_unread_mail: Option<(i32, i32, i32, i32)>,
    pub self_obj: u32,
    /// The guild (`expedition`) id from our last `SCUnitExpeditionChangedPacket` (0 = none).
    pub self_expedition: u32,
    /// Id of the guild the bot is in (from `SCFactionCreated` / `SCExpeditionMemberList`); 0 =
    /// none seen.
    pub guild_id: u32,
    pub self_pos: (f32, f32, f32),
    /// Last position actually received from the server, independent of local movement sends.
    pub observed_self_pos: Option<(f32, f32, f32)>,
    pub observation_seq: u64,
    pub observation_errors: u64,
    pub units: HashMap<u32, UnitInfo>,
    /// Doodads announced by `SCDoodad(s)CreatedPacket` and not removed since.
    pub doodads: HashMap<u32, DoodadInfo>,
    /// `SCDoodadCompleteQuestPacket` (`0x14d`) in arrival order: `(doodad object id, quest id)`.
    pub doodad_complete_quests: Vec<(u32, u32)>,
    /// `SCQuestUnitReqFailedPacket` (`0x148`) in arrival order: `(quest id, unit object id,
    /// kind, message id, value)`, with the omitted flag-bag parts decoded as 0.
    pub quest_unit_req_failed: Vec<(u32, u32, u8, u16, u32)>,
    /// `SCDoodadPhaseMsgPacket` (`0x157`) in arrival order: `(doodad object id, func group id)`.
    pub doodad_phase_msgs: Vec<(u32, u32)>,
    /// First item of the bag (`SCCharacterInvenContentsPacket`, slot type 2): `(item id, slot)`.
    pub bag_items: Vec<(u64, u8, u32)>,
    pub bag_slots: u8,
    /// Latest `SCUnitPointsPacket` per unit: `(hp, mp)` (the wire carries them times 100).
    pub unit_points: HashMap<u32, (i32, i32)>,
    /// Units announced dead by `SCUnitDeathPacket` (and not removed since).
    pub dead_units: HashSet<u32>,
    /// Last `SCUnitDeathPacket` for our own unit: (kill reason, resurrect wait ms, lost exp,
    /// durability loss rate); `self_death_seq` counts them.
    pub self_death: Option<(u8, i32, i32, u8)>,
    pub self_death_seq: u64,
    /// Buff indices per `(owner obj id, buff template id)` from `SCBuffCreated` / `SCBuffRemoved`.
    pub buffs: HashMap<(u32, u32), HashSet<u32>>,
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
        if p.level == LEVEL_GAME {
            let mut g = p.stream();
            if p.opcode == sc::SCFactionCreatedPacket {
                if let Ok(id) = g.read_u32() {
                    self.guild_id = id;
                }
            } else if p.opcode == sc::SCExpeditionMemberListPacket {
                if let (Ok(_), Ok(_), Ok(id)) = (g.read_u32(), g.read_u8(), g.read_u32()) {
                    self.guild_id = id;
                }
            }
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
        } else if p.is_sc(sc::SCCreateCharacterResponsePacket)
            || p.is_sc(sc::SCEditCharacterResponsePacket)
        {
            let character = read_character(&mut s)?;
            anyhow::ensure!(s.remaining() == 0, "trailing character-response bytes");
            match st.characters.iter_mut().find(|c| c.id == character.id) {
                Some(entry) => *entry = character,
                None => st.characters.push(character),
            }
        } else if p.is_sc(sc::SCCountUnreadMailPacket) {
            st.count_unread_mail =
                Some((s.read_i32()?, s.read_i32()?, s.read_i32()?, s.read_i32()?));
        } else if p.is_sc(sc::SCBuffCreatedPacket) {
            use aaemu_game::systems::skills::types::SkillCaster;
            let kind = s.read_u8()?;
            SkillCaster::read(kind, &mut s)?;
            s.read_u32()?; // caster id
            let owner = s.read_bc()?;
            let index = s.read_u32()?;
            let buff = s.read_u32()?;
            st.buffs.entry((owner, buff)).or_default().insert(index);
        } else if p.is_sc(sc::SCBuffRemovedPacket) {
            let owner = s.read_bc()?;
            let index = s.read_u32()?;
            st.buffs.retain(|(o, _), indices| {
                if *o == owner {
                    indices.remove(&index);
                }
                !indices.is_empty()
            });
        } else if p.is_sc(sc::SCUnitExpeditionChangedPacket) {
            let _obj_id = s.read_bc()?;
            let character_id = s.read_u32()?;
            let _kicker = s.read_string()?;
            let _unit_name = s.read_string()?;
            let _id = s.read_u32()?;
            let expedition_id = s.read_u32()?;
            let _expel = s.read_bool()?;
            if character_id == st.selected_char {
                st.self_expedition = expedition_id;
            }
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
        } else if p.is_sc(sc::SCDoodadCompleteQuestPacket) {
            let doodad_obj_id = s.read_bc()?;
            let quest_id = s.read_u32()?;
            st.doodad_complete_quests.push((doodad_obj_id, quest_id));
        } else if p.is_sc(sc::SCQuestUnitReqFailedPacket) {
            // Reader FUN_397b5d00 / bag FUN_397b1ef0: u32 quest, bc unit, flag byte, then
            // only the set parts (kind u8, message id u16, value u32).
            let quest_id = s.read_u32()?;
            let unit_obj_id = s.read_bc()?;
            let flags = s.read_u8()?;
            let kind = if flags & 0x01 != 0 { s.read_u8()? } else { 0 };
            let message_id = if flags & 0x02 != 0 { s.read_u16()? } else { 0 };
            let value = if flags & 0x04 != 0 { s.read_u32()? } else { 0 };
            st.quest_unit_req_failed
                .push((quest_id, unit_obj_id, kind, message_id, value));
        } else if p.is_sc(sc::SCDoodadPhaseMsgPacket) {
            let doodad_obj_id = s.read_bc()?;
            let func_group_id = s.read_u32()?;
            st.doodad_phase_msgs.push((doodad_obj_id, func_group_id));
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
            let obj = s.read_bc()?;
            let reason = s.read_u8()?;
            let wait_ms = s.read_i32()?;
            let lost_exp = s.read_i32()?;
            let durability = s.read_u8()?;
            if obj == st.self_obj {
                st.self_death = Some((reason, wait_ms, lost_exp, durability));
                st.self_death_seq += 1;
            }
            st.dead_units.insert(obj);
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
