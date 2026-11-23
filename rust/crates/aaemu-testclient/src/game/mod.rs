//! Game server client (C2G / G2C + the proxy channel) for the 1.2 protocol.
//!
//! Every frame payload is `[0xDD][level]...`: level 1 (game) carries `[hash][count][u16 opcode]`,
//! level 2 (proxy / CryNetwork) just `[u16 opcode]`. Traffic is unencrypted; native level-4
//! game batches carry raw DEFLATE.

use std::collections::VecDeque;
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

mod batch;
mod state;

use self::batch::inflate_batch;
pub(crate) use self::batch::{split_game_batch, BatchError};
pub use self::batch::{MAX_BATCH_BYTES, MAX_BATCH_PACKETS, MAX_GAME_FRAME};
pub(crate) use self::state::read_character;
pub use self::state::{
    parse_item_task, Appearance, CharInfo, DoodadInfo, GameEvent, GameState, ItemTaskInfo,
    MateSpawn, TaskItem, UnitInfo,
};

pub const LEVEL_GAME: u8 = 1;
pub const LEVEL_PROXY: u8 = 2;
/// `CompressedGamePackets` (`DD04`).
pub const LEVEL_COMPRESSED: u8 = 4;

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
        self.hang_up().await;
    }

    /// [`Self::disconnect_hard`] on a client the caller keeps (the `hangup` script command): the
    /// server sees the socket close without a `CSLeaveWorldPacket`.
    pub async fn hang_up(&mut self) {
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

    /// `CSCreateCharacterPacket` for a Nuian male Fighter with default appearance.
    pub async fn create_character(&mut self, name: &str) -> anyhow::Result<()> {
        self.create_character_with_race(name, 1).await
    }

    /// `CSCreateCharacterPacket` for a male Fighter of the selected race.
    pub(crate) async fn create_character_with_race(
        &mut self,
        name: &str,
        race: u8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_str(name).write_u8(race).write_u8(1);
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

    /// `CSEditCharacterPacket` (0x023) for the pre-select lobby edit: the create body with a
    /// leading character id and no `introZoneId`. Sends the same race/gender/ability/level the
    /// character was created with and the given customizing colours.
    pub async fn edit_character(
        &mut self,
        id: u32,
        name: &str,
        appearance: Appearance,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(id)
            .write_str(name)
            .write_u8(1) // race
            .write_u8(1); // gender
        for _ in 0..7 {
            s.write_u32(0); // body items: template defaults
        }
        write_custom_model_appearance(
            &mut s,
            appearance.hair_color_id,
            appearance.skin_color_id,
            appearance.model_id,
        );
        s.write_u8(1) // ability1: Fight
            .write_u8(11) // ability2: None
            .write_u8(11) // ability3: None
            .write_u8(1); // level
        self.send_cs(cs::CSEditCharacterPacket, s.as_bytes()).await
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

    /// `CSStartSkillPacket` for a skill aimed at a ground position.
    pub async fn start_position_skill(
        &mut self,
        skill_id: u32,
        (x, y, z): (f32, f32, f32),
    ) -> anyhow::Result<()> {
        use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster};

        let mut s = PacketStream::new();
        s.write_u32(skill_id);
        SkillCaster::Unit(self.state.self_obj).write(&mut s);
        SkillCastTarget::Position {
            obj_id: 0,
            x,
            y,
            z,
            rot: 0.0,
            obj_id1: 0,
            obj_id2: 0,
        }
        .write(&mut s);
        s.write_u8(0);
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket` from a vehicle bar entry: mount caster (the vehicle) on itself, the
    /// way the 1.2 client casts a slave / mate skill. `mount_skill_template_id` is the client's
    /// bar-row hint; the server resolves the mount row and rider skill from `skill_id`
    /// (`mount_skills.skill_id` -> `mount_attached_skills`), not from this field.
    pub async fn start_mount_skill(
        &mut self,
        skill_id: u32,
        mount_obj: u32,
        mount_skill_template_id: u32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(3) // SkillCasterType.Mount
            .write_bc(mount_obj)
            .write_u32(mount_skill_template_id)
            .write_u8(0) // target type Unit
            .write_bc(mount_obj)
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
    write_custom_model_appearance(s, 0, 0, 0);
}

fn write_custom_model_appearance(
    s: &mut PacketStream,
    hair_color_id: u32,
    skin_color_id: u32,
    model_id: u32,
) {
    s.write_u8(3) // UnitCustomModelType.Face
        .write_u32(hair_color_id)
        .write_u32(skin_color_id)
        .write_u32(model_id);
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
mod compressed_batch_tests;
#[cfg(test)]
mod doodad_complete_quest_tests;
#[cfg(test)]
mod doodad_phase_msg_tests;
#[cfg(test)]
mod facing_tests;
#[cfg(test)]
mod observed_position_tests;
#[cfg(test)]
mod quest_unit_req_failed_tests;
#[cfg(test)]
mod socket_reader_tests;
