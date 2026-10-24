//! Appearance (look conversion / skinize / cleanup) QA helpers for the `script` scenario. Test
//! tooling only: nothing here changes what the server does.
//!
//! * **H1** — [`Stacks`] remembers the exact rows `item <template> <count> [as <label>]` made
//!   the server announce (id, slot, grade, flags, count, made unit, equipment image), and
//!   [`expand_body`] turns `{id:X}` / `{tpl:X}` / `{grade:X}` / `{flags:X}` / `{unit:X}` /
//!   `{count:X}` / `{slot:X}` / `{st:X}` / `{u8:N}` .. `{u64:N}` (plus `SELF` / `PEER`) into
//!   client packet bytes, so `rawcs` / `lookcs` send exactly what the native client writes for
//!   `CSConvertItemLook` (`049`) and an item-cast `CSStartSkill` (`052`).
//! * **H2** — [`decode_tasks`] decodes one `SCItemTaskSuccessPacket` strictly (every task, the
//!   force-remove list and the trailing id), [`run_flow`] sends one packet and keeps every
//!   packet that follows together with the instant it came off the socket, and [`check_flow`]
//!   asserts the ordered skill / error / item-task packets, their bytes and their arrival
//!   windows (`lookexpect`, `lookgap`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::sc;
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{anyhow, bail, ensure, Context};

use crate::game::{Arrival, GameClient, Packet, LEVEL_GAME};

// ---------------------------------------------------------------------------------------------
// H1: remembered item stacks and body placeholders
// ---------------------------------------------------------------------------------------------

/// One item stack as the server announced it in a `Create` / `Take` / `ChangeOwner` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stack {
    pub slot_type: u8,
    pub slot: u8,
    pub template: u32,
    pub id: u64,
    pub grade: u8,
    pub flags: u8,
    pub count: i32,
    /// Equipment image template (detail bytes `1..5`; 0 for every other item class).
    pub image: u32,
    pub made_unit: u32,
    /// The 128-byte detail block of the row (byte 0 = detail type).
    pub detail: Vec<u8>,
}

/// The stacks a script remembered, by label and by template id.
#[derive(Debug, Default)]
pub struct Stacks {
    by_ref: HashMap<String, Stack>,
}

impl Stacks {
    /// Remembers `stack` under its template id and `label`; every earlier entry of the same
    /// stack id is refreshed so a label never keeps a stale count.
    pub fn remember(&mut self, label: Option<&str>, stack: Stack) {
        for old in self.by_ref.values_mut().filter(|s| s.id == stack.id) {
            *old = stack.clone();
        }
        self.by_ref
            .insert(stack.template.to_string(), stack.clone());
        if let Some(label) = label {
            self.by_ref.insert(label.to_string(), stack);
        }
    }

    /// The stack named by a label or a template id.
    pub fn get(&self, reference: &str) -> anyhow::Result<&Stack> {
        self.by_ref.get(reference).with_context(|| {
            format!(
                "no remembered item '{reference}' (add it first with `item <template> <count> [as <label>]`)"
            )
        })
    }

    /// Learns the stack of `template` that the item tasks among `packets` created (`Create`,
    /// `Take`, `ChangeOwner`) or grew (`AddStack` onto a stack already remembered); the last
    /// one wins. `None` when the reply carried no such row.
    pub fn learn(
        &mut self,
        packets: &[Packet],
        template: u32,
        label: Option<&str>,
    ) -> Option<Stack> {
        let mut found: Option<Stack> = None;
        for p in packets
            .iter()
            .filter(|p| p.level == LEVEL_GAME && p.opcode == sc::SCItemTaskSuccessPacket)
        {
            for row in decode_tasks_partial(&p.body).rows {
                match row {
                    Row::Item { stack, .. } if stack.template == template => found = Some(stack),
                    Row::Count {
                        slot_type,
                        slot,
                        id,
                        delta,
                        template: t,
                    } if t == template => {
                        if let Some(known) = self.by_ref.values().find(|s| s.id == id).cloned() {
                            found = Some(Stack {
                                slot_type,
                                slot,
                                count: known.count + delta,
                                ..known
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(stack) = &found {
            self.remember(label, stack.clone());
        }
        found
    }
}

/// `bc` encoding (3 bytes, little endian) of an object id, as hex.
fn bc_hex(id: u32) -> String {
    format!(
        "{:02x}{:02x}{:02x}",
        id & 0xff,
        (id >> 8) & 0xff,
        (id >> 16) & 0xff
    )
}

fn parse_uint(arg: &str) -> anyhow::Result<u64> {
    match arg.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => arg.parse(),
    }
    .map_err(|_| anyhow!("bad number '{arg}'"))
}

fn placeholder_hex(spec: &str, stacks: &Stacks) -> anyhow::Result<String> {
    let (kind, arg) = spec
        .split_once(':')
        .with_context(|| format!("placeholder {{{spec}}} needs the form {{kind:argument}}"))?;
    Ok(match kind {
        "u8" => hex::encode([u8::try_from(parse_uint(arg)?)?]),
        "u16" => hex::encode(u16::try_from(parse_uint(arg)?)?.to_le_bytes()),
        "u32" => hex::encode(u32::try_from(parse_uint(arg)?)?.to_le_bytes()),
        "u64" => hex::encode(parse_uint(arg)?.to_le_bytes()),
        "item" | "id" => hex::encode(stacks.get(arg)?.id.to_le_bytes()),
        "tpl" => hex::encode(stacks.get(arg)?.template.to_le_bytes()),
        "grade" => hex::encode([stacks.get(arg)?.grade]),
        "flags" => hex::encode([stacks.get(arg)?.flags]),
        "unit" => hex::encode(stacks.get(arg)?.made_unit.to_le_bytes()),
        "count" => hex::encode(stacks.get(arg)?.count.to_le_bytes()),
        "slot" => hex::encode([stacks.get(arg)?.slot]),
        "st" => hex::encode([stacks.get(arg)?.slot_type]),
        other => bail!("unknown placeholder kind '{other}' in {{{spec}}}"),
    })
}

/// Expands the placeholders of a `rawcs` / `lookcs` body: `{kind:argument}` tokens (see the
/// module docs) and then `SELF` / `PEER` (3-byte object ids), and decodes the hex.
pub fn expand_body(
    raw: &str,
    self_obj: u32,
    peer_obj: Option<u32>,
    stacks: &Stacks,
) -> anyhow::Result<Vec<u8>> {
    let mut hex = String::new();
    let mut rest = raw;
    while let Some(open) = rest.find('{') {
        hex.push_str(&rest[..open]);
        let close = rest[open..]
            .find('}')
            .map(|i| i + open)
            .with_context(|| format!("unclosed '{{' in {raw:?}"))?;
        hex.push_str(&placeholder_hex(&rest[open + 1..close], stacks)?);
        rest = &rest[close + 1..];
    }
    hex.push_str(rest);
    let mut hex = hex.replace("SELF", &bc_hex(self_obj));
    if hex.contains("PEER") {
        let peer = peer_obj.context("PEER needs another player in sight")?;
        hex = hex.replace("PEER", &bc_hex(peer));
    }
    ensure!(
        hex.len().is_multiple_of(2) && hex.is_ascii(),
        "rawcs body hex must be an even number of hex digits: {hex:?}"
    );
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| anyhow!("{e}: {hex}")))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// H2: strict SCItemTaskSuccessPacket decoding
// ---------------------------------------------------------------------------------------------

/// One task of an `SCItemTaskSuccessPacket` (`models/item_task.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// `Create` (5), `Take` (6) or `ChangeOwner` (15): a full item.
    Item { action: u8, stack: Stack },
    /// `AddStack` (4): the signed count change of an existing stack.
    Count {
        slot_type: u8,
        slot: u8,
        id: u64,
        delta: i32,
        template: u32,
    },
    /// `Remove` (7).
    Remove {
        slot_type: u8,
        slot: u8,
        id: u64,
        template: u32,
    },
    /// `UpdateDetail` (9): the item's 128-byte detail block.
    Update {
        slot_type: u8,
        slot: u8,
        id: u64,
        block: Vec<u8>,
    },
    /// `SetFlagsBits` (10): the item's complete flags.
    Bits {
        slot_type: u8,
        slot: u8,
        id: u64,
        bits: u8,
    },
    /// `Seize` (13).
    Seize { slot_type: u8, slot: u8, id: u64 },
}

impl Row {
    fn describe(&self) -> String {
        match self {
            Row::Item { action, stack: s } => format!(
                "{} st={} slot={} id={} tpl={} grade={} flags={:#04x} count={} unit={:#x} image={}",
                match action {
                    5 => "create",
                    6 => "take",
                    _ => "owner",
                },
                s.slot_type,
                s.slot,
                s.id,
                s.template,
                s.grade,
                s.flags,
                s.count,
                s.made_unit,
                s.image
            ),
            Row::Count {
                slot_type,
                slot,
                id,
                delta,
                template,
            } => format!("count st={slot_type} slot={slot} id={id} delta={delta} tpl={template}"),
            Row::Remove {
                slot_type,
                slot,
                id,
                template,
            } => format!("remove st={slot_type} slot={slot} id={id} tpl={template}"),
            Row::Update {
                slot_type,
                slot,
                id,
                block,
            } => format!(
                "update st={slot_type} slot={slot} id={id} detail_type={} image={}",
                block.first().copied().unwrap_or(0),
                image_of(block)
            ),
            Row::Bits {
                slot_type,
                slot,
                id,
                bits,
            } => format!("bits st={slot_type} slot={slot} id={id} bits={bits:#04x}"),
            Row::Seize {
                slot_type,
                slot,
                id,
            } => format!("seize st={slot_type} slot={slot} id={id}"),
        }
    }
}

/// The equipment image template of a detail block (`[1][u32 image]...`), 0 for other classes.
fn image_of(block: &[u8]) -> u32 {
    if block.len() >= 5 && block[0] == 1 {
        u32::from_le_bytes([block[1], block[2], block[3], block[4]])
    } else {
        0
    }
}

/// A decoded `SCItemTaskSuccessPacket`: `[u8 reason][u8 count][tasks][u8 n][u64 * n][u32 tail]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tasks {
    pub reason: u8,
    pub rows: Vec<Row>,
    pub force_remove: Vec<u64>,
    pub tail: u32,
}

fn decode_into(body: &[u8], out: &mut Tasks) -> anyhow::Result<()> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    out.reason = s.read_u8()?;
    let count = s.read_u8()?;
    for _ in 0..count {
        let action = s.read_u8()?;
        let row = match action {
            5 | 6 | 15 => {
                let slot_type = s.read_u8()?;
                let slot = s.read_u8()?;
                let template = s.read_u32()?;
                let id = s.read_u64()?;
                let grade = s.read_u8()?;
                let flags = s.read_u8()?;
                let count = s.read_i32()?;
                let len = s.read_i16()?;
                ensure!(len == 128, "task item detail length {len}");
                let detail = s.read_bytes(128)?;
                let _create_time = s.read_i64()?;
                let _lifespan = s.read_i32()?;
                let made_unit = s.read_u32()?;
                let _world = s.read_u8()?;
                let _unsecure = s.read_i64()?;
                let _unpack = s.read_i64()?;
                Row::Item {
                    action,
                    stack: Stack {
                        slot_type,
                        slot,
                        template,
                        id,
                        grade,
                        flags,
                        count,
                        image: image_of(&detail),
                        made_unit,
                        detail,
                    },
                }
            }
            4 => Row::Count {
                slot_type: s.read_u8()?,
                slot: s.read_u8()?,
                id: s.read_u64()?,
                delta: s.read_i32()?,
                template: s.read_u32()?,
            },
            7 => Row::Remove {
                slot_type: s.read_u8()?,
                slot: s.read_u8()?,
                id: s.read_u64()?,
                template: s.read_u32()?,
            },
            9 => {
                let slot_type = s.read_u8()?;
                let slot = s.read_u8()?;
                let id = s.read_u64()?;
                let len = s.read_i16()?;
                ensure!(len == 128, "task update detail length {len}");
                Row::Update {
                    slot_type,
                    slot,
                    id,
                    block: s.read_bytes(128)?,
                }
            }
            10 => {
                let slot_type = s.read_u8()?;
                let slot = s.read_u8()?;
                let id = s.read_u64()?;
                let bits = s.read_u8()?;
                let zero = s.read_u64()?;
                ensure!(zero == 0, "flag-bits task trailer {zero:#x} is not zero");
                Row::Bits {
                    slot_type,
                    slot,
                    id,
                    bits,
                }
            }
            13 => Row::Seize {
                slot_type: s.read_u8()?,
                slot: s.read_u8()?,
                id: s.read_u64()?,
            },
            other => bail!("unsupported item task action {other}"),
        };
        out.rows.push(row);
    }
    let removed = s.read_u8()?;
    for _ in 0..removed {
        out.force_remove.push(s.read_u64()?);
    }
    out.tail = s.read_u32()?;
    ensure!(
        s.remaining() == 0,
        "{} trailing byte(s) after the item task packet",
        s.remaining()
    );
    Ok(())
}

/// Strict decode: any unknown task, truncation or trailing byte is an error.
pub fn decode_tasks(body: &[u8]) -> anyhow::Result<Tasks> {
    let mut out = Tasks::default();
    decode_into(body, &mut out)?;
    Ok(out)
}

/// Best-effort decode: what was read before the first problem.
pub fn decode_tasks_partial(body: &[u8]) -> Tasks {
    let mut out = Tasks::default();
    let _ = decode_into(body, &mut out);
    out
}

// ---------------------------------------------------------------------------------------------
// H2: skill packets
// ---------------------------------------------------------------------------------------------

/// The part `SCSkillStarted` and `SCSkillFired` share: `[u32 skill][u16 tl][caster][target]
/// [object]`.
struct SkillHead {
    skill: u32,
    tl: u16,
    caster: SkillCaster,
    target: SkillCastTarget,
    object: SkillObject,
    /// Length of `caster..object` in bytes (what the request carries after its skill id).
    block_len: usize,
}

fn read_skill_head(s: &mut PacketStream) -> anyhow::Result<SkillHead> {
    let skill = s.read_u32()?;
    let tl = s.read_u16()?;
    let from = s.pos();
    let kind = s.read_u8()?;
    let caster = SkillCaster::read(kind, s)?;
    let kind = s.read_u8()?;
    let target = SkillCastTarget::read(kind, s)?;
    let flag = s.read_u8()?;
    let object = SkillObject::read(flag, s)?;
    Ok(SkillHead {
        skill,
        tl,
        caster,
        target,
        object,
        block_len: s.pos() - from,
    })
}

struct Started {
    head: SkillHead,
    real_ms: u32,
    base_ms: u32,
    synergy: u8,
    flags: u8,
    /// `SetSkillResult` byte (flag 1); `None` for a start that did not fail.
    result: Option<u8>,
}

fn decode_started(body: &[u8]) -> anyhow::Result<Started> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let head = read_skill_head(&mut s)?;
    let real = s.read_u16()?;
    let base = s.read_u16()?;
    let synergy = s.read_u8()?;
    let flags = s.read_u8()?;
    let result = if flags & 1 != 0 {
        Some(s.read_u8()?)
    } else {
        None
    };
    if flags & 2 != 0 {
        s.read_u16()?;
    }
    if flags & 4 != 0 {
        s.read_u32()?;
    }
    ensure!(
        s.remaining() == 0,
        "{} trailing byte(s) after SCSkillStarted",
        s.remaining()
    );
    Ok(Started {
        head,
        real_ms: u32::from(real) * 10,
        base_ms: u32::from(base) * 10,
        synergy,
        flags,
        result,
    })
}

struct Fired {
    head: SkillHead,
    /// Wire `computed_delay / 10 + 10` and `channeling / 10 + 10`.
    delay: i16,
    channel: i16,
    anim: u32,
}

fn decode_fired(body: &[u8]) -> anyhow::Result<Fired> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let head = read_skill_head(&mut s)?;
    let delay = s.read_i16()?;
    let channel = s.read_i16()?;
    let _f = s.read_u8()?;
    let anim = s.read_u32()?;
    let _flag = s.read_u8()?;
    ensure!(
        s.remaining() == 0,
        "{} trailing byte(s) after SCSkillFired",
        s.remaining()
    );
    Ok(Fired {
        head,
        delay,
        channel,
        anim,
    })
}

/// `echo`: the packet carries the request's caster / target / skill-object bytes unchanged.
fn echoes(head: &SkillHead, body: &[u8], request: &[u8]) -> bool {
    request.len() >= 4
        && head.block_len == request.len() - 4
        && body.len() >= 6 + head.block_len
        && body[6..6 + head.block_len] == request[4..]
}

// ---------------------------------------------------------------------------------------------
// H2: flows
// ---------------------------------------------------------------------------------------------

/// One request and everything the server sent afterwards.
#[derive(Debug, Clone)]
pub struct Flow {
    pub opcode: u16,
    pub request: Vec<u8>,
    pub sent: Instant,
    pub packets: Vec<Arrival>,
    /// Our own object id: skill packets are broadcast to everybody in sight, only the ones of
    /// our own casts (caster object id / timeline id) are part of the flow.
    pub own_obj: u32,
}

/// How long [`run_flow`] listens.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    /// Hard limit after the request was sent.
    pub max: Duration,
    /// Stop `grace` after the first `SCSkillEnded` instead of at `max`.
    pub until_ended: bool,
    pub grace: Duration,
}

/// Packet kinds the flow assertions look at; every other packet is listed but ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Started,
    Fired,
    Ended,
    Stopped,
    CastingStopped,
    Err,
    Task,
}

impl Kind {
    fn of(a: &Arrival) -> Option<Self> {
        if a.level != LEVEL_GAME {
            return None;
        }
        Some(match a.opcode {
            sc::SCSkillStartedPacket => Self::Started,
            sc::SCSkillFiredPacket => Self::Fired,
            sc::SCSkillEndedPacket => Self::Ended,
            sc::SCSkillStoppedPacket => Self::Stopped,
            sc::SCCastingStoppedPacket => Self::CastingStopped,
            sc::SCErrorMsgPacket => Self::Err,
            sc::SCItemTaskSuccessPacket => Self::Task,
            _ => return None,
        })
    }

    fn from_name(name: &str) -> anyhow::Result<Self> {
        Ok(match name {
            "started" => Self::Started,
            "fired" => Self::Fired,
            "ended" => Self::Ended,
            "stopped" => Self::Stopped,
            "castingstopped" => Self::CastingStopped,
            "err" => Self::Err,
            "task" => Self::Task,
            other => bail!("unknown packet kind '{other}'"),
        })
    }
}

/// A packet the assertions look at, with its arrival time (ms after the request was sent).
#[derive(Debug, Clone)]
pub struct Event {
    pub t_ms: f64,
    pub kind: Kind,
    pub body: Vec<u8>,
}

impl Flow {
    fn ms_since_send(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.sent).as_secs_f64() * 1000.0
    }

    /// The relevant packets in arrival order, and the ignored ones as `(t_ms, name, body len)`.
    /// Skill packets of other players' casts are ignored (named `...(other caster)`).
    pub fn events(&self) -> (Vec<Event>, Vec<(f64, String, usize)>) {
        // The timeline ids of our own casts, from the Started / Fired packets naming us.
        let own_tls: Vec<u16> = self
            .packets
            .iter()
            .filter(|a| matches!(Kind::of(a), Some(Kind::Started | Kind::Fired)))
            .filter_map(|a| {
                let head = read_skill_head(&mut PacketStream::from_bytes(a.body.clone())).ok()?;
                (head.caster.obj_id() == self.own_obj && head.tl != 0).then_some(head.tl)
            })
            .collect();
        let tl_of = |a: &Arrival| a.body.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let mut relevant = Vec::new();
        let mut ignored = Vec::new();
        for a in &self.packets {
            let t_ms = self.ms_since_send(a.at);
            let kind = Kind::of(a).filter(|kind| match kind {
                Kind::Started | Kind::Fired => {
                    // An undecodable packet stays in so the assertions report it.
                    read_skill_head(&mut PacketStream::from_bytes(a.body.clone()))
                        .map_or(true, |head| head.caster.obj_id() == self.own_obj)
                }
                Kind::Ended | Kind::CastingStopped => {
                    tl_of(a).is_none_or(|tl| own_tls.contains(&tl))
                }
                Kind::Stopped => PacketStream::from_bytes(a.body.clone())
                    .read_bc()
                    .map_or(true, |obj| obj == self.own_obj),
                Kind::Err | Kind::Task => true,
            });
            match kind {
                Some(kind) => relevant.push(Event {
                    t_ms,
                    kind,
                    body: a.body.clone(),
                }),
                None => {
                    let mut name =
                        crate::game::packet_name(crate::record::Dir::S2C, a.level, a.opcode)
                            .to_string();
                    if Kind::of(a).is_some() {
                        name.push_str("(other caster)");
                    }
                    ignored.push((t_ms, name, a.body.len()));
                }
            }
        }
        (relevant, ignored)
    }

    /// Arrival time (ms after the send) of the `nth` (1-based) packet of `kind`.
    fn time_of(&self, what: &str) -> anyhow::Result<f64> {
        if what == "send" {
            return Ok(0.0);
        }
        let (name, nth) = match what.split_once('#') {
            Some((n, i)) => (n, i.parse::<usize>().context("bad #index")?),
            None => (what, 1),
        };
        ensure!(nth >= 1, "#index is 1-based");
        let kind = Kind::from_name(name)?;
        self.events()
            .0
            .iter()
            .filter(|e| e.kind == kind)
            .nth(nth - 1)
            .map(|e| e.t_ms)
            .with_context(|| format!("the flow has no {what} packet"))
    }
}

/// Sends `request` as a level-1 client packet and records everything that follows.
pub async fn run_flow(
    game: &mut GameClient,
    opcode: u16,
    request: Vec<u8>,
    wait: Wait,
) -> anyhow::Result<Flow> {
    game.capture = Some(Vec::new());
    if let Err(e) = game.send_cs(opcode, &request).await {
        game.capture = None;
        return Err(e);
    }
    let sent = game.last_tx().context("request was not sent")?;
    let hard_stop = sent + wait.max;
    let mut ended_at: Option<Instant> = None;
    loop {
        let stop = match ended_at {
            Some(at) => (at + wait.grace).min(hard_stop),
            None => hard_stop,
        };
        let now = Instant::now();
        if now >= stop {
            break;
        }
        let step = (stop - now).min(Duration::from_millis(250));
        if let Err(e) = game.recv_within(step).await {
            game.capture = None;
            return Err(e);
        }
        if wait.until_ended && ended_at.is_none() {
            ended_at = game
                .capture
                .iter()
                .flatten()
                .find(|a| Kind::of(a) == Some(Kind::Ended))
                .map(|a| a.at);
        }
    }
    Ok(Flow {
        opcode,
        request,
        sent,
        packets: game.capture.take().unwrap_or_default(),
        own_obj: game.state.self_obj,
    })
}

// ---------------------------------------------------------------------------------------------
// H2: expected flows
// ---------------------------------------------------------------------------------------------

/// One expected row of an item-task packet. `R` is a remembered label or template id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WantRow {
    /// `bits(R,<hex>)`: `SetFlagsBits` with these complete flags.
    Bits(String, u8),
    /// `seize(R)`.
    Seize(String),
    /// `count(R,<delta>)`: `AddStack` with this signed change (and R's template).
    Count(String, i32),
    /// `image(R,<template>)`: `UpdateDetail` of R whose equipment image is this template and
    /// whose other detail bytes equal the ones R was created with (0 = cleaned up).
    Image(String, u32),
    /// `create(R)`: a `Create`/`Take`/`ChangeOwner` row identical to R.
    Create(String),
    /// `remove(R)`.
    Remove(String),
}

/// One expected packet of a flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// `started(<skill>[,<cast ms>][,echo][,fail=<n>])`
    Started {
        skill: u32,
        cast_ms: Option<u32>,
        echo: bool,
        fail: Option<u8>,
    },
    /// `fired(<skill>[,echo])`
    Fired { skill: u32, echo: bool },
    /// `ended`
    Ended,
    /// `err(<code>[,<param>])`: `SCErrorMsg` with both error words equal to `<code>`.
    Err { code: i16, param: Option<u32> },
    /// `task(<reason>[:<row>,<row>...])`
    Task { reason: u8, rows: Vec<WantRow> },
}

/// A [`Want`] with its optional `@<lo>-<hi>` arrival window (ms after the request was sent).
#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub want: Want,
    pub window: Option<(f64, f64)>,
    pub text: String,
}

/// Splits `s` at `sep` outside parentheses.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut from = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&s[from..i]);
                from = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[from..]);
    parts
}

fn inner(token: &str) -> anyhow::Result<(&str, Option<&str>)> {
    match token.find('(') {
        None => Ok((token, None)),
        Some(open) => {
            let args = token[open + 1..]
                .strip_suffix(')')
                .with_context(|| format!("missing ')' in '{token}'"))?;
            Ok((&token[..open], Some(args)))
        }
    }
}

fn parse_row(token: &str) -> anyhow::Result<WantRow> {
    let (name, args) = inner(token)?;
    let args: Vec<&str> = args.map(|a| split_top(a, ',')).unwrap_or_default();
    let arg = |i: usize| -> anyhow::Result<&str> {
        args.get(i)
            .copied()
            .with_context(|| format!("row '{token}' is missing argument {}", i + 1))
    };
    let row = match name {
        "bits" => WantRow::Bits(
            arg(0)?.to_string(),
            u8::from_str_radix(arg(1)?.trim_start_matches("0x"), 16)
                .with_context(|| format!("bad flags hex in '{token}'"))?,
        ),
        "seize" => WantRow::Seize(arg(0)?.to_string()),
        "count" => WantRow::Count(arg(0)?.to_string(), arg(1)?.parse()?),
        "image" => WantRow::Image(arg(0)?.to_string(), arg(1)?.parse()?),
        "create" => WantRow::Create(arg(0)?.to_string()),
        "remove" => WantRow::Remove(arg(0)?.to_string()),
        other => bail!("unknown row '{other}' in '{token}'"),
    };
    ensure!(
        args.len()
            == if matches!(
                &row,
                WantRow::Bits(..) | WantRow::Count(..) | WantRow::Image(..)
            ) {
                2
            } else {
                1
            },
        "row '{token}' has the wrong number of arguments"
    );
    Ok(row)
}

fn parse_want(token: &str) -> anyhow::Result<Want> {
    let (name, args) = inner(token)?;
    let list: Vec<&str> = args.map(|a| split_top(a, ',')).unwrap_or_default();
    Ok(match name {
        "started" => {
            let skill = list
                .first()
                .with_context(|| format!("'{token}' needs the skill id"))?
                .parse()?;
            let (mut cast_ms, mut echo, mut fail) = (None, false, None);
            for a in &list[1..] {
                if *a == "echo" {
                    echo = true;
                } else if let Some(n) = a.strip_prefix("fail=") {
                    fail = Some(n.parse()?);
                } else {
                    cast_ms = Some(a.parse().with_context(|| format!("bad option '{a}'"))?);
                }
            }
            Want::Started {
                skill,
                cast_ms,
                echo,
                fail,
            }
        }
        "fired" => {
            let skill = list
                .first()
                .with_context(|| format!("'{token}' needs the skill id"))?
                .parse()?;
            let echo = match list.get(1).copied() {
                None => false,
                Some("echo") => true,
                Some(other) => bail!("unknown option '{other}' in '{token}'"),
            };
            Want::Fired { skill, echo }
        }
        "ended" => {
            ensure!(args.is_none(), "'ended' takes no arguments");
            Want::Ended
        }
        "err" => Want::Err {
            code: list
                .first()
                .with_context(|| format!("'{token}' needs the error code"))?
                .parse()?,
            param: list.get(1).map(|p| p.parse()).transpose()?,
        },
        "task" => {
            let body = args.with_context(|| format!("'{token}' needs (<reason>[:rows])"))?;
            let (reason, rows) = match body.split_once(':') {
                Some((reason, rows)) => (reason, rows),
                None => (body, ""),
            };
            let rows = split_top(rows, ',')
                .into_iter()
                .filter(|r| !r.is_empty())
                .map(parse_row)
                .collect::<anyhow::Result<Vec<_>>>()?;
            Want::Task {
                reason: reason.trim().parse()?,
                rows,
            }
        }
        other => bail!("unknown packet '{other}' in '{token}'"),
    })
}

/// Parses the arguments of `lookexpect`: whitespace separated packet tokens such as
/// `started(23741,5000,echo)@50-900 fired(23741,echo) task(113:bits(D,09),seize(P1)) ended`.
pub fn parse_expect(text: &str) -> anyhow::Result<Vec<Expect>> {
    // `none`: the server must send no skill / error / item-task packet at all.
    if text.trim() == "none" {
        return Ok(Vec::new());
    }
    // Whitespace separates tokens only outside parentheses.
    let mut tokens = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    ensure!(depth == 0, "unbalanced parentheses in {text:?}");
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
        .into_iter()
        .map(|token| {
            let (spec, window) = match token.rfind('@') {
                Some(at) => {
                    let (lo, hi) = token[at + 1..]
                        .split_once('-')
                        .with_context(|| format!("window of '{token}' must be @<lo>-<hi>"))?;
                    (&token[..at], Some((lo.parse()?, hi.parse()?)))
                }
                None => (token.as_str(), None),
            };
            Ok(Expect {
                want: parse_want(spec).with_context(|| format!("in token '{token}'"))?,
                window,
                text: token.clone(),
            })
        })
        .collect()
}

fn describe_event(ev: &Event, flow: &Flow) -> String {
    let at = format!("+{:>8.1} ms", ev.t_ms);
    let hex = hex::encode(&ev.body);
    let text = match ev.kind {
        Kind::Started => match decode_started(&ev.body) {
            Ok(s) => format!(
                "SCSkillStarted skill={} tl={} cast={}/{} ms synergy={} flags={:#04x} result={} \
                 caster={:?} target={:?} object={:?} echo={}",
                s.head.skill,
                s.head.tl,
                s.real_ms,
                s.base_ms,
                s.synergy,
                s.flags,
                s.result.map_or("-".to_string(), |r| r.to_string()),
                s.head.caster,
                s.head.target,
                s.head.object,
                echoes(&s.head, &ev.body, &flow.request)
            ),
            Err(e) => format!("SCSkillStarted (undecodable: {e:#})"),
        },
        Kind::Fired => match decode_fired(&ev.body) {
            Ok(f) => format!(
                "SCSkillFired skill={} tl={} delay={} channel={} anim={} caster={:?} target={:?} echo={}",
                f.head.skill,
                f.head.tl,
                f.delay,
                f.channel,
                f.anim,
                f.head.caster,
                f.head.target,
                echoes(&f.head, &ev.body, &flow.request)
            ),
            Err(e) => format!("SCSkillFired (undecodable: {e:#})"),
        },
        Kind::Ended => format!(
            "SCSkillEnded tl={}",
            ev.body
                .get(..2)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
        ),
        Kind::Stopped => "SCSkillStopped".to_string(),
        Kind::CastingStopped => "SCCastingStopped".to_string(),
        Kind::Err => {
            let mut s = PacketStream::from_bytes(ev.body.clone());
            match (|| -> anyhow::Result<String> {
                Ok(format!(
                    "SCErrorMsg error1={} error2={} param={} notify={}",
                    s.read_i16()?,
                    s.read_i16()?,
                    s.read_u32()?,
                    s.read_bool()?
                ))
            })() {
                Ok(t) => t,
                Err(e) => format!("SCErrorMsg (undecodable: {e:#})"),
            }
        }
        Kind::Task => match decode_tasks(&ev.body) {
            Ok(t) => {
                let rows: Vec<String> = t.rows.iter().map(Row::describe).collect();
                format!(
                    "SCItemTaskSuccess reason={} rows={} [{}]{}",
                    t.reason,
                    rows.len(),
                    rows.join("; "),
                    if t.force_remove.is_empty() {
                        String::new()
                    } else {
                        format!(" force_remove={:?}", t.force_remove)
                    }
                )
            }
            Err(e) => format!("SCItemTaskSuccess (undecodable: {e:#})"),
        },
    };
    format!("{at}  {text}\n{:>15}hex={hex}", "")
}

/// Human-readable transcript of a flow (request, relevant packets, ignored packets).
pub fn transcript(flow: &Flow) -> String {
    let (events, ignored) = flow.events();
    let mut out = format!(
        "request opcode {:#05x} {} bytes hex={}\n",
        flow.opcode,
        flow.request.len(),
        hex::encode(&flow.request)
    );
    for ev in &events {
        let _ = writeln!(out, "{}", describe_event(ev, flow));
    }
    if ignored.is_empty() {
        out.push_str("ignored: none\n");
    } else {
        let list: Vec<String> = ignored
            .iter()
            .map(|(t, n, len)| format!("{n}[{len}B]@{t:.0}ms"))
            .collect();
        let _ = writeln!(out, "ignored: {}", list.join(", "));
    }
    out
}

fn match_row(row: &Row, want: &WantRow, stacks: &Stacks) -> anyhow::Result<()> {
    let wrong = || anyhow!("expected {want:?}, got {}", row.describe());
    match (want, row) {
        (
            WantRow::Bits(r, bits),
            Row::Bits {
                slot_type,
                slot,
                id,
                bits: got,
            },
        ) => {
            let s = stacks.get(r)?;
            ensure!(
                (*slot_type, *slot, *id, *got) == (s.slot_type, s.slot, s.id, *bits),
                "{}",
                wrong()
            );
        }
        (
            WantRow::Seize(r),
            Row::Seize {
                slot_type,
                slot,
                id,
            },
        ) => {
            let s = stacks.get(r)?;
            ensure!(
                (*slot_type, *slot, *id) == (s.slot_type, s.slot, s.id),
                "{}",
                wrong()
            );
        }
        (
            WantRow::Count(r, delta),
            Row::Count {
                slot_type,
                slot,
                id,
                delta: got,
                template,
            },
        ) => {
            let s = stacks.get(r)?;
            ensure!(
                (*slot_type, *slot, *id, *got, *template)
                    == (s.slot_type, s.slot, s.id, *delta, s.template),
                "{}",
                wrong()
            );
        }
        (
            WantRow::Image(r, image),
            Row::Update {
                slot_type,
                slot,
                id,
                block,
            },
        ) => {
            let s = stacks.get(r)?;
            ensure!(
                (*slot_type, *slot, *id) == (s.slot_type, s.slot, s.id),
                "{}",
                wrong()
            );
            ensure!(
                block.len() == 128 && block[0] == 1 && image_of(block) == *image,
                "{}",
                wrong()
            );
            ensure!(
                s.detail.len() == 128 && block[5..] == s.detail[5..],
                "{r}: the detail block changed beyond the image template\n  created {}\n  now     {}",
                hex::encode(&s.detail[5..]),
                hex::encode(&block[5..])
            );
        }
        (WantRow::Create(r), Row::Item { stack, .. }) => {
            ensure!(stacks.get(r)? == stack, "{}", wrong());
        }
        (
            WantRow::Remove(r),
            Row::Remove {
                slot_type,
                slot,
                id,
                template,
            },
        ) => {
            let s = stacks.get(r)?;
            ensure!(
                (*slot_type, *slot, *id, *template) == (s.slot_type, s.slot, s.id, s.template),
                "{}",
                wrong()
            );
        }
        _ => return Err(wrong()),
    }
    Ok(())
}

fn match_event(ev: &Event, ex: &Expect, flow: &Flow, stacks: &Stacks) -> anyhow::Result<()> {
    match (&ex.want, ev.kind) {
        (
            Want::Started {
                skill,
                cast_ms,
                echo,
                fail,
            },
            Kind::Started,
        ) => {
            let s = decode_started(&ev.body)?;
            ensure!(s.head.skill == *skill, "skill id {}", s.head.skill);
            if let Some(ms) = cast_ms {
                ensure!(
                    s.real_ms == *ms && s.base_ms == *ms,
                    "cast time {}/{} ms",
                    s.real_ms,
                    s.base_ms
                );
            }
            ensure!(
                s.result == *fail,
                "start result {:?}, expected {fail:?}",
                s.result
            );
            if *echo {
                ensure!(
                    echoes(&s.head, &ev.body, &flow.request),
                    "caster/target/object bytes differ from the request"
                );
            }
        }
        (Want::Fired { skill, echo }, Kind::Fired) => {
            let f = decode_fired(&ev.body)?;
            ensure!(f.head.skill == *skill, "skill id {}", f.head.skill);
            if *echo {
                ensure!(
                    echoes(&f.head, &ev.body, &flow.request),
                    "caster/target/object bytes differ from the request"
                );
            }
        }
        (Want::Ended, Kind::Ended) => ensure!(ev.body.len() == 2, "body length {}", ev.body.len()),
        (Want::Err { code, param }, Kind::Err) => {
            let mut s = PacketStream::from_bytes(ev.body.clone());
            let (e1, e2, kind, _notify) =
                (s.read_i16()?, s.read_i16()?, s.read_u32()?, s.read_bool()?);
            ensure!(s.remaining() == 0, "trailing bytes in SCErrorMsg");
            ensure!(e1 == *code && e2 == *code, "error words {e1}/{e2}");
            if let Some(p) = param {
                ensure!(kind == *p, "error parameter {kind}");
            }
        }
        (Want::Task { reason, rows }, Kind::Task) => {
            let t = decode_tasks(&ev.body)?;
            ensure!(t.reason == *reason, "reason {}", t.reason);
            ensure!(
                t.rows.len() == rows.len(),
                "{} row(s), expected {}",
                t.rows.len(),
                rows.len()
            );
            ensure!(t.force_remove.is_empty(), "force-remove list not empty");
            ensure!(t.tail == 0, "trailing id {}", t.tail);
            for (row, want) in t.rows.iter().zip(rows) {
                match_row(row, want, stacks)?;
            }
        }
        _ => bail!("different packet kind"),
    }
    if let Some((lo, hi)) = ex.window {
        ensure!(
            ev.t_ms >= lo && ev.t_ms <= hi,
            "arrived {:.1} ms after the request, expected {lo}..{hi} ms",
            ev.t_ms
        );
    }
    Ok(())
}

/// Asserts that the relevant packets of `flow` are exactly `expect`, in order (with bytes and
/// arrival windows), and that every timeline id in the flow is the same. Returns the
/// transcript on success; the error carries it on failure.
pub fn check_flow(flow: &Flow, stacks: &Stacks, expect: &[Expect]) -> anyhow::Result<String> {
    let text = transcript(flow);
    let (events, _) = flow.events();
    let fail = |msg: String| anyhow!("{msg}\n--- actual flow ---\n{text}");
    if events.len() != expect.len() {
        return Err(fail(format!(
            "expected {} packet(s) [{}], got {}",
            expect.len(),
            expect
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            events.len()
        )));
    }
    for (i, (ev, ex)) in events.iter().zip(expect).enumerate() {
        if let Err(e) = match_event(ev, ex, flow, stacks) {
            return Err(fail(format!(
                "packet #{} should be {}: {e:#}",
                i + 1,
                ex.text
            )));
        }
    }
    let mut tls = events.iter().filter_map(|ev| match ev.kind {
        Kind::Started => decode_started(&ev.body).ok().map(|s| s.head.tl),
        Kind::Fired => decode_fired(&ev.body).ok().map(|f| f.head.tl),
        Kind::Ended => ev.body.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])),
        _ => None,
    });
    if let Some(first) = tls.next() {
        if let Some(other) = tls.find(|tl| *tl != first) {
            return Err(fail(format!("timeline ids differ: {first} vs {other}")));
        }
    }
    Ok(text)
}

/// `lookgap <from> <to> <min ms> <max ms>`: the time between two packets of the last flow
/// (`send`, `started`, `fired`, `ended`, `err`, `task`, optionally `#n`). Returns the gap.
pub fn gap(flow: &Flow, from: &str, to: &str, min_ms: f64, max_ms: f64) -> anyhow::Result<f64> {
    let (a, b) = (flow.time_of(from)?, flow.time_of(to)?);
    let gap = b - a;
    ensure!(
        gap >= min_ms && gap <= max_ms,
        "{from} -> {to} took {gap:.1} ms, expected {min_ms}..{max_ms} ms\n{}",
        transcript(flow)
    );
    Ok(gap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_commons::Marshal;
    use aaemu_game::models::item::{Item, ItemClass};
    use aaemu_game::models::item_task::ItemTask;
    use aaemu_game::packets::world::SCErrorMsgPacket;
    use aaemu_game::systems::economy::item_packets::SCItemTaskSuccessPacket;
    use aaemu_game::systems::skills::packets::{
        SCSkillEndedPacket, SCSkillFiredPacket, SCSkillStartedPacket,
    };

    const ACTOR: u32 = 0x010203;

    fn bytes(p: &dyn Marshal) -> Vec<u8> {
        let mut s = PacketStream::new();
        p.write(&mut s);
        s.into_bytes()
    }

    fn item(template: u32, id: u64, slot: i32, grade: u8, flags: u8, count: i32) -> Item {
        let mut i = Item::new(1);
        i.class = ItemClass::Weapon;
        i.template_id = template;
        i.id = id;
        i.slot_type = 2;
        i.slot = slot;
        i.grade = grade;
        i.flags = flags;
        i.count = count;
        i.made_unit_id = 0x6677_8899;
        i
    }

    fn stack_of(i: &Item) -> Stack {
        let detail = {
            let mut d = i.detail_type_and_details();
            d.resize(128, 0);
            d
        };
        Stack {
            slot_type: i.slot_type,
            slot: i.slot as u8,
            template: i.template_id,
            id: i.id,
            grade: i.grade,
            flags: i.flags,
            count: i.count,
            image: image_of(&detail),
            made_unit: i.made_unit_id,
            detail,
        }
    }

    fn task_packet(reason: u8, tasks: Vec<ItemTask>) -> Vec<u8> {
        bytes(&SCItemTaskSuccessPacket {
            action: reason,
            tasks,
            force_remove: Vec::new(),
        })
    }

    /// Golden: root aa-net `crates/aa-net/tests/fixtures/look-conversion/skinize.bin`
    /// (sha256 eb6cf3ea...), `cleanup.bin` (4c30ea3e...) and `convert.bin` (a7b38a5c...), frozen
    /// by the native-client disassembly: actor 0x010203, base id 0x0102030405060708, image id
    /// 0x1112131415161718, source grade 6, made unit 0x66778899, target grade 7, target template
    /// 0xaabbccdd.
    const SKINIZE_GOLDEN: &str =
        "bd5c0000020302010807060504030201b87400000699887766030302011817161514131211ddccbbaa0700";
    const CLEANUP_GOLDEN: &str =
        "a15b0000020302010807060504030201087400000699887766030302011817161514131211ddccbbaa0700";
    const CONVERT_GOLDEN: &str = "08070605040302011817161514131211";

    /// The stacks behind the frozen goldens: source `S` (the powder of `source_template`) and
    /// target `T` (template 0xaabbccdd, grade 7).
    fn golden_stacks(source_template: u32) -> Stacks {
        let mut stacks = Stacks::default();
        let mut source = stack_of(&item(source_template, 0x0102_0304_0506_0708, 0, 6, 0, 1));
        source.made_unit = 0x6677_8899;
        stacks.remember(Some("S"), source);
        let mut target = stack_of(&item(0xaabb_ccdd, 0x1112_1314_1516_1718, 1, 7, 0, 1));
        target.made_unit = 0;
        stacks.remember(Some("T"), target);
        stacks
    }

    #[test]
    fn placeholders_rebuild_the_frozen_native_client_bodies() {
        let skinize = expand_body(
            "{u32:23741}02SELF{id:S}{tpl:S}{grade:S}{unit:S}03SELF{id:T}{tpl:T}{grade:T}00",
            ACTOR,
            None,
            &golden_stacks(29880),
        )
        .unwrap();
        assert_eq!(hex::encode(skinize), SKINIZE_GOLDEN);
        let cleanup = expand_body(
            "{u32:23457}02SELF{id:S}{tpl:S}{grade:S}{unit:S}03SELF{id:T}{tpl:T}{grade:T}00",
            ACTOR,
            None,
            &golden_stacks(29704),
        )
        .unwrap();
        assert_eq!(hex::encode(cleanup), CLEANUP_GOLDEN);
        let convert = expand_body("{id:S}{id:T}", ACTOR, None, &golden_stacks(29880)).unwrap();
        assert_eq!(hex::encode(convert), CONVERT_GOLDEN);
    }

    #[test]
    fn placeholder_forms_labels_and_errors() {
        let mut stacks = golden_stacks(29880);
        assert_eq!(
            expand_body("{u8:0x7f}{u16:300}{u64:1}", 0, None, &stacks).unwrap(),
            [0x7f, 0x2c, 0x01, 1, 0, 0, 0, 0, 0, 0, 0]
        );
        // a template id refers to the latest stack of that template
        assert_eq!(
            expand_body("{tpl:29880}{slot:T}{st:T}", 0, None, &stacks).unwrap(),
            [0xb8, 0x74, 0, 0, 1, 2]
        );
        stacks.remember(None, stack_of(&item(29880, 77, 5, 0, 1, 3)));
        assert_eq!(
            expand_body("{id:29880}", 0, None, &stacks).unwrap(),
            77u64.to_le_bytes()
        );
        assert_eq!(
            expand_body("PEER", 0, Some(0x0a0b0c), &stacks).unwrap(),
            [0x0c, 0x0b, 0x0a]
        );
        assert!(expand_body("PEER", 0, None, &stacks).is_err());
        for bad in ["{id:nope}", "{wat:1}", "{u8:256}", "{u8}", "{id:P", "0"] {
            assert!(
                expand_body(bad, 0, None, &stacks).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn strict_decoder_reads_what_the_server_writer_wrote() {
        let mut base = item(26060, 1000, 3, 3, 0x01, 1);
        base.details.image_item_template_id = 26118;
        let powder = item(29880, 1001, 0, 0, 0, 9);
        let body = task_packet(
            111,
            vec![
                ItemTask::update(&base),
                ItemTask::update_bits(&base),
                ItemTask::count_update(&powder, -9),
                ItemTask::remove_slot(&powder),
                ItemTask::remove(&powder),
                ItemTask::add(&base),
            ],
        );
        let t = decode_tasks(&body).unwrap();
        assert_eq!((t.reason, t.tail), (111, 0));
        assert!(t.force_remove.is_empty());
        let Row::Update { id, block, .. } = &t.rows[0] else {
            panic!("{:?}", t.rows[0])
        };
        assert_eq!(
            (*id, block.len(), block[0], image_of(block)),
            (1000, 128, 1, 26118)
        );
        assert_eq!(
            t.rows[1],
            Row::Bits {
                slot_type: 2,
                slot: 3,
                id: 1000,
                bits: 0x01
            }
        );
        assert_eq!(
            t.rows[2],
            Row::Count {
                slot_type: 2,
                slot: 0,
                id: 1001,
                delta: -9,
                template: 29880
            }
        );
        assert_eq!(
            t.rows[3],
            Row::Seize {
                slot_type: 2,
                slot: 0,
                id: 1001
            }
        );
        assert_eq!(
            t.rows[4],
            Row::Remove {
                slot_type: 2,
                slot: 0,
                id: 1001,
                template: 29880
            }
        );
        let Row::Item { action, stack } = &t.rows[5] else {
            panic!("{:?}", t.rows[5])
        };
        assert_eq!(*action, 5);
        assert_eq!(
            (
                stack.template,
                stack.id,
                stack.grade,
                stack.flags,
                stack.count
            ),
            (26060, 1000, 3, 0x01, 1)
        );
        assert_eq!((stack.made_unit, stack.image), (0x6677_8899, 26118));
        // the empty receipt is real: `SC090 reason, 0 rows`
        let empty = task_packet(113, Vec::new());
        assert_eq!(empty, [113, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_tasks(&empty).unwrap().rows, Vec::new());
    }

    #[test]
    fn strict_decoder_rejects_everything_unexpected() {
        let base = item(26060, 1000, 3, 3, 1, 1);
        let good = task_packet(113, vec![ItemTask::update_bits(&base)]);
        assert!(decode_tasks(&good).is_ok());
        assert!(decode_tasks(&good[..good.len() - 1]).is_err(), "truncated");
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_tasks(&trailing).is_err(), "trailing byte");
        let mut unknown = good.clone();
        unknown[2] = 99;
        assert!(decode_tasks(&unknown).is_err(), "unknown action");
        let partial = decode_tasks_partial(&unknown);
        assert_eq!((partial.reason, partial.rows.len()), (113, 0));
    }

    #[test]
    fn learn_captures_the_created_row_and_follows_merges() {
        let mut stacks = Stacks::default();
        let mut powder = item(29880, 500, 4, 0, 0, 2);
        powder.made_unit_id = 0x1234;
        let reply = Packet {
            level: LEVEL_GAME,
            opcode: sc::SCItemTaskSuccessPacket,
            body: task_packet(11, vec![ItemTask::add(&powder)]),
        };
        let learned = stacks.learn(&[reply], 29880, Some("P")).unwrap();
        assert_eq!(
            (
                learned.id,
                learned.slot,
                learned.grade,
                learned.flags,
                learned.count
            ),
            (500, 4, 0, 0, 2)
        );
        assert_eq!(learned.made_unit, 0x1234);
        // a later `item 29880 11` merges into the known stack: the label follows
        let merged = Packet {
            level: LEVEL_GAME,
            opcode: sc::SCItemTaskSuccessPacket,
            body: task_packet(11, vec![ItemTask::count_update(&powder, 11)]),
        };
        let after = stacks.learn(&[merged], 29880, None).unwrap();
        assert_eq!((after.id, after.count), (500, 13));
        assert_eq!(stacks.get("P").unwrap().count, 13);
        assert_eq!(stacks.get("29880").unwrap().count, 13);
        // a merge into a stack the script never saw is not guessed
        let mut empty = Stacks::default();
        let merged = Packet {
            level: LEVEL_GAME,
            opcode: sc::SCItemTaskSuccessPacket,
            body: task_packet(11, vec![ItemTask::count_update(&powder, 11)]),
        };
        assert!(empty.learn(&[merged], 29880, None).is_none());
    }

    #[test]
    fn expect_tokens_parse_with_nested_rows_and_windows() {
        let e = parse_expect(
            "started(23741,5000,echo)@50-900 fired(23741,echo) \
             task(113:bits(D,09),seize(P1)) task(113) err(722,29880) ended@5000-6000",
        )
        .unwrap();
        assert_eq!(e.len(), 6);
        assert_eq!(
            e[0].want,
            Want::Started {
                skill: 23741,
                cast_ms: Some(5000),
                echo: true,
                fail: None
            }
        );
        assert_eq!(e[0].window, Some((50.0, 900.0)));
        assert_eq!(
            e[2].want,
            Want::Task {
                reason: 113,
                rows: vec![WantRow::Bits("D".into(), 0x09), WantRow::Seize("P1".into())]
            }
        );
        assert_eq!(
            e[3].want,
            Want::Task {
                reason: 113,
                rows: Vec::new()
            }
        );
        assert_eq!(
            e[4].want,
            Want::Err {
                code: 722,
                param: Some(29880)
            }
        );
        assert_eq!(e[5].want, Want::Ended);
        assert_eq!(e[5].window, Some((5000.0, 6000.0)));
        assert!(parse_expect("none").unwrap().is_empty());
        for bad in [
            "started",
            "task(113:wat(D))",
            "task(x)",
            "fired(1,loud)",
            "ended(1)",
            "task(113:bits(D))",
            "started(1)@5",
            "task(113:seize(D)",
        ] {
            assert!(parse_expect(bad).is_err(), "{bad} should be rejected");
        }
    }

    /// A Skinize cast of powder `P` onto donor `D` with the server's own packet writers.
    struct Cast {
        flow: Flow,
        stacks: Stacks,
        request: Vec<u8>,
        donor: Item,
        powder: Item,
    }

    fn skinize_cast(start_ms: u64, fire_ms: u64) -> Cast {
        let powder = item(29880, 0x0102_0304_0506_0708, 0, 0, 0, 5);
        let donor = item(26118, 0x1112_1314_1516_1718, 1, 1, 0x01, 1);
        let mut stacks = Stacks::default();
        stacks.remember(Some("P"), stack_of(&powder));
        stacks.remember(Some("D"), stack_of(&donor));
        let request = expand_body(
            "{u32:23741}02SELF{id:P}{tpl:P}{grade:P}{unit:P}03SELF{id:D}{tpl:D}{grade:D}00",
            ACTOR,
            None,
            &stacks,
        )
        .unwrap();
        let caster = SkillCaster::Item {
            obj_id: ACTOR,
            item_id: powder.id,
            item_template_id: powder.template_id,
            type1: powder.grade,
            type2: powder.made_unit_id,
        };
        let target = SkillCastTarget::Item {
            obj_id: ACTOR,
            id: donor.id,
            type1: donor.template_id,
            type2: donor.grade,
        };
        let mut started = SCSkillStartedPacket::new(23741, 7, caster, target, SkillObject::None);
        started.real_cast_time_div10 = 500;
        started.base_cast_time_div10 = 500;
        let fired = SCSkillFiredPacket {
            id: 23741,
            tl: 7,
            caster,
            target,
            skill_object: SkillObject::None,
            computed_delay: 0,
            channeling_time: 0,
            fire_anim_id: 0,
        };
        let mut skinized = donor.clone();
        skinized.flags |= 0x08;
        let t0 = Instant::now();
        let at = |ms: u64, level: u8, opcode: u16, body: Vec<u8>| Arrival {
            at: t0 + Duration::from_millis(ms),
            level,
            opcode,
            body,
        };
        let packets = vec![
            at(
                start_ms,
                LEVEL_GAME,
                sc::SCSkillStartedPacket,
                bytes(&started),
            ),
            // an unrelated packet between the two (mana), ignored by the assertions
            at(start_ms + 5, LEVEL_GAME, sc::SCUnitPointsPacket, vec![0; 4]),
            at(fire_ms, LEVEL_GAME, sc::SCSkillFiredPacket, bytes(&fired)),
            at(
                fire_ms,
                LEVEL_GAME,
                sc::SCItemTaskSuccessPacket,
                task_packet(113, vec![ItemTask::update_bits(&skinized)]),
            ),
            at(
                fire_ms + 1,
                LEVEL_GAME,
                sc::SCItemTaskSuccessPacket,
                task_packet(113, vec![ItemTask::remove_slot(&powder)]),
            ),
            at(
                fire_ms + 1,
                LEVEL_GAME,
                sc::SCItemTaskSuccessPacket,
                task_packet(113, Vec::new()),
            ),
            at(
                fire_ms + 2,
                LEVEL_GAME,
                sc::SCSkillEndedPacket,
                bytes(&SCSkillEndedPacket { tl: 7 }),
            ),
        ];
        Cast {
            flow: Flow {
                opcode: 0x52,
                request: request.clone(),
                sent: t0,
                packets,
                own_obj: ACTOR,
            },
            stacks,
            request,
            donor: skinized,
            powder,
        }
    }

    const SKINIZE_FLOW: &str = "started(23741,5000,echo)@50-900 fired(23741,echo)@5050-6500 \
         task(113:bits(D,09)) task(113:seize(P)) task(113) ended";

    #[test]
    fn a_skinize_flow_matches_byte_for_byte_and_in_time() {
        let cast = skinize_cast(100, 5103);
        assert_eq!(cast.request.len(), 43);
        assert_eq!(cast.donor.flags, 0x09);
        assert_eq!(cast.powder.count, 5);
        let text = check_flow(
            &cast.flow,
            &cast.stacks,
            &parse_expect(SKINIZE_FLOW).unwrap(),
        )
        .unwrap();
        assert!(text.contains("SCSkillStarted skill=23741 tl=7 cast=5000/5000 ms"));
        assert!(text.contains(&format!("bits st=2 slot=1 id={} bits=0x09", cast.donor.id)));
        assert!(text.contains("ignored: SCUnitPoints"));
        // the gaps the suites assert
        let near = |got: f64, want: f64| assert!((got - want).abs() < 1e-6, "{got} != {want}");
        near(
            gap(&cast.flow, "send", "started", 50.0, 900.0).unwrap(),
            100.0,
        );
        near(
            gap(&cast.flow, "started", "fired", 5000.0, 5300.0).unwrap(),
            5003.0,
        );
        near(gap(&cast.flow, "fired", "ended", 0.0, 100.0).unwrap(), 2.0);
        near(gap(&cast.flow, "task#2", "task#3", 0.0, 5.0).unwrap(), 0.0);
        assert!(gap(&cast.flow, "started", "fired", 0.0, 100.0).is_err());
        assert!(gap(&cast.flow, "task#4", "ended", 0.0, 100.0).is_err());
    }

    #[test]
    fn flow_mismatches_are_reported_with_the_actual_transcript() {
        let cast = skinize_cast(100, 5103);
        let run = |expected: &str| {
            check_flow(&cast.flow, &cast.stacks, &parse_expect(expected).unwrap())
                .unwrap_err()
                .to_string()
        };
        // wrong bits, wrong order, wrong count, a fired that is too early, wrong cast time
        assert!(run(
            "started(23741) fired(23741) task(113:bits(D,08)) task(113:seize(P)) task(113) ended"
        )
        .contains("packet #3"));
        assert!(run(
            "started(23741) fired(23741) task(113:seize(P)) task(113:bits(D,09)) task(113) ended"
        )
        .contains("packet #3"));
        assert!(
            run("started(23741) fired(23741) task(113:bits(D,09)) ended")
                .contains("expected 4 packet(s)")
        );
        assert!(run("started(23741) fired(23741)@0-4000 task(113:bits(D,09)) task(113:seize(P)) task(113) ended")
            .contains("expected 0..4000 ms"));
        assert!(run("started(23741,3000) fired(23741) task(113:bits(D,09)) task(113:seize(P)) task(113) ended")
            .contains("cast time 5000/5000"));
        assert!(run(
            "started(23457) fired(23741) task(113:bits(D,09)) task(113:seize(P)) task(113) ended"
        )
        .contains("skill id 23741"));
        let message = run(
            "started(23741) fired(23741) task(113:bits(D,08)) task(113:seize(P)) task(113) ended",
        );
        assert!(message.contains("--- actual flow ---"));
        assert!(message.contains("request opcode 0x052 43 bytes"));
        // a start that failed, and a tampered echo
        let mut tampered = cast.flow.clone();
        tampered.request[10] ^= 0xff;
        assert!(check_flow(
            &tampered,
            &cast.stacks,
            &parse_expect(SKINIZE_FLOW).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("differ from the request"));
        // timeline ids of our own packets must agree
        let mut other_tl = cast.flow.clone();
        let fired = other_tl
            .packets
            .iter_mut()
            .find(|a| a.opcode == sc::SCSkillFiredPacket)
            .unwrap();
        fired.body[4..6].copy_from_slice(&8u16.to_le_bytes());
        assert!(check_flow(
            &other_tl,
            &cast.stacks,
            &parse_expect(SKINIZE_FLOW).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("timeline ids differ"));
    }

    #[test]
    fn other_players_casts_in_sight_are_not_part_of_the_flow() {
        let mut cast = skinize_cast(100, 5103);
        let foreign = SkillCaster::Item {
            obj_id: ACTOR + 1,
            item_id: 77,
            item_template_id: 29880,
            type1: 0,
            type2: 0,
        };
        let target = SkillCastTarget::Item {
            obj_id: ACTOR + 1,
            id: 78,
            type1: 26118,
            type2: 0,
        };
        let mut started = SCSkillStartedPacket::new(23741, 9, foreign, target, SkillObject::None);
        started.real_cast_time_div10 = 500;
        started.base_cast_time_div10 = 500;
        let fired = SCSkillFiredPacket {
            id: 23741,
            tl: 9,
            caster: foreign,
            target,
            skill_object: SkillObject::None,
            computed_delay: 0,
            channeling_time: 0,
            fire_anim_id: 0,
        };
        let t0 = cast.flow.sent;
        let at = |ms: u64, opcode: u16, body: Vec<u8>| Arrival {
            at: t0 + Duration::from_millis(ms),
            level: LEVEL_GAME,
            opcode,
            body,
        };
        cast.flow.packets.extend([
            at(90, sc::SCSkillStartedPacket, bytes(&started)),
            at(5100, sc::SCSkillFiredPacket, bytes(&fired)),
            at(
                5104,
                sc::SCSkillEndedPacket,
                bytes(&SCSkillEndedPacket { tl: 9 }),
            ),
        ]);
        cast.flow.packets.sort_by_key(|a| a.at);
        let text = check_flow(
            &cast.flow,
            &cast.stacks,
            &parse_expect(SKINIZE_FLOW).unwrap(),
        )
        .unwrap();
        assert!(text.contains("SCSkillStartedPacket(other caster)"));
        assert!(text.contains("SCSkillEndedPacket(other caster)"));
    }

    #[test]
    fn image_rows_compare_the_detail_block_and_errors_the_code() {
        let mut base = item(26060, 4242, 6, 3, 0x01, 1);
        let mut stacks = Stacks::default();
        stacks.remember(Some("B"), stack_of(&base));
        base.details.image_item_template_id = 26118;
        let t0 = Instant::now();
        let packets = vec![
            Arrival {
                at: t0 + Duration::from_millis(3),
                level: LEVEL_GAME,
                opcode: sc::SCItemTaskSuccessPacket,
                body: task_packet(111, vec![ItemTask::update(&base)]),
            },
            Arrival {
                at: t0 + Duration::from_millis(4),
                level: LEVEL_GAME,
                opcode: sc::SCErrorMsgPacket,
                body: bytes(&SCErrorMsgPacket {
                    error1: 722,
                    error2: 722,
                    kind: 29880,
                    is_notify: true,
                }),
            },
        ];
        let flow = Flow {
            opcode: 0x49,
            request: vec![0; 16],
            sent: t0,
            packets,
            own_obj: ACTOR,
        };
        let expect = |s: &str| parse_expect(s).unwrap();
        check_flow(
            &flow,
            &stacks,
            &expect("task(111:image(B,26118)) err(722,29880)"),
        )
        .unwrap();
        for bad in [
            "task(111:image(B,0)) err(722,29880)",
            "task(111:image(B,26118)) err(722,1)",
            "task(111:image(B,26118)) err(737)",
            "task(111:bits(B,01)) err(722)",
            "task(112:image(B,26118)) err(722)",
        ] {
            assert!(check_flow(&flow, &stacks, &expect(bad)).is_err(), "{bad}");
        }
        // any change beyond the image template is flagged
        let mut hurt = flow.clone();
        base.details.durability = 99;
        hurt.packets[0].body = task_packet(111, vec![ItemTask::update(&base)]);
        assert!(check_flow(
            &hurt,
            &stacks,
            &expect("task(111:image(B,26118)) err(722,29880)")
        )
        .unwrap_err()
        .to_string()
        .contains("detail block changed"));
    }
}
