//! Music lifecycle commands of the `script` scenario / `.vtest` suites (`m*`), driving the same
//! wire sequence as the 1.2 client: Compose item cast -> received own `SCSkillFired` ->
//! `CSSaveUserMusicNotes`; play skill -> own `SCSkillStarted` -> `CSSendUserMusic`; pause /
//! close skill -> own `SCSkillStarted` -> `CSEndMusic`; cold / hit `CSRequestMusicNotes`.
//! Everything is sent only in reaction to packets actually received.
//!
//! ```text
//! mitem <template> <count>           `/item add self`, remember the created stack and bag slot
//! mequip <template> <equip slot>     CSSwapItems bag -> equipment (instrument: slot 18)
//! mcompose <grade> "<title>" "<notes>"  item cast of skill 22215 on the remembered paper 27902
//! mdb "<sql>"                        print a scalar SQL result
//! msong "<sql>" ["<title>" "<notes>"]  take the song id from a scalar SQL (after a relogin)
//! mrequest <tooltip 0|1> <buff-only 0|1> [song id|unknown]  CSRequestMusicNotes; checks the reply
//! mskill <skill> midi|end|none|premidi [unit|item] [diag] [<buff tokens>]  cast; react to the
//!                                    own Started; fails without one (unless `diag`), without the
//!                                    own broadcast of the MIDI / pause it should cause
//! mend [<buff tokens>]               bare CSEndMusic; the game log must show its receipt
//! mwatch <secs> [send] [pause] [nosend] [nopause]  print music packets, fail on expectations
//! ```
//!
//! Buff tokens check the own buff packets of the command: `+buff:N` created, `!buff:N` not
//! created, `-buff:N` removed, `=buff:N` not removed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use aaemu_game::systems::skills::types::SkillCaster;
use anyhow::{bail, ensure, Context};

use super::questitem::{chat_text, gm_command};
use crate::game::{GameClient, Packet};

const SKILL_COMPOSE: u32 = 22215;
const PAPER: u32 = 27902;
const SHEET: u32 = 28051;
const SLOT_EQUIP: u8 = 1;
/// Format 0 SMF, one track: program change, C4 note on / off, end of track (38 bytes).
const SMF: [u8; 38] = [
    0x4d, 0x54, 0x68, 0x64, 0, 0, 0, 6, 0, 0, 0, 1, 0x01, 0xe0, 0x4d, 0x54, 0x72, 0x6b, 0, 0, 0,
    16, 0, 0xc0, 25, 0, 0x90, 60, 100, 0x83, 0x60, 0x80, 60, 0, 0, 0xff, 0x2f, 0,
];

/// One stack the bot created with `/item add self`.
#[derive(Clone, Debug, Default)]
pub(super) struct Held {
    id: u64,
    slot_type: u8,
    slot: u8,
    grade: u8,
    made_unit: u32,
    song_id: Option<u64>,
}

/// Per-bot state of the music commands.
#[derive(Default)]
pub(super) struct Music {
    held: HashMap<u32, Held>,
    last_song: Option<u64>,
    /// Song id -> (title, notes) this bot composed or was told about.
    songs: HashMap<u64, (String, String)>,
}

/// (owner obj, buff index) -> buff id of every `SCBuffCreated` any bot saw (removals carry
/// only the index).
static BUFFS: Mutex<Option<HashMap<(u32, u32), u32>>> = Mutex::new(None);
/// FNV of the last MIDI a bot sent with CS121 (the listener compares what it receives).
static SENT_MIDI: Mutex<Option<u64>> = Mutex::new(None);

fn remember_buff(owner: u32, index: u32, buff: u32) {
    if let Ok(mut g) = BUFFS.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert((owner, index), buff);
    }
}

fn buff_of(owner: u32, index: u32) -> Option<u32> {
    BUFFS.lock().ok()?.as_ref()?.get(&(owner, index)).copied()
}

fn buff_created(p: &Packet) -> Option<(u32, u32, u32)> {
    if !p.is_sc(sc::SCBuffCreatedPacket) {
        return None;
    }
    let mut s = p.stream();
    let kind = s.read_u8().ok()?;
    SkillCaster::read(kind, &mut s).ok()?;
    s.read_u32().ok()?;
    let owner = s.read_bc().ok()?;
    let index = s.read_u32().ok()?;
    Some((owner, index, s.read_u32().ok()?))
}

fn buff_removed(p: &Packet) -> Option<(u32, u32)> {
    if !p.is_sc(sc::SCBuffRemovedPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_bc().ok()?, s.read_u32().ok()?))
}

/// (obj, author, midi) of an `SCSendUserMusic`.
fn send_music(p: &Packet) -> Option<(u32, String, Vec<u8>)> {
    if !p.is_sc(sc::SCSendUserMusicPacket) {
        return None;
    }
    let mut s = p.stream();
    s.read_u32().ok()?;
    let obj = s.read_bc().ok()?;
    let author = s.read_string().ok()?;
    Some((obj, author, s.read_bytes_sized().ok()?))
}

/// Checks `+buff:N` / `!buff:N` / `-buff:N` / `=buff:N` tokens against the own buff packets.
fn check_buff_tokens(tokens: &[String], packets: &[Packet], me: u32) -> anyhow::Result<()> {
    let created: Vec<u32> = packets
        .iter()
        .filter_map(buff_created)
        .filter(|(o, _, _)| *o == me)
        .map(|(_, _, b)| b)
        .collect();
    let removed: Vec<u32> = packets
        .iter()
        .filter_map(buff_removed)
        .filter(|(o, _)| *o == me)
        .filter_map(|(o, i)| buff_of(o, i))
        .collect();
    for tok in tokens {
        let Some((mode, id)) = tok
            .split_once(":")
            .and_then(|(m, id)| Some((m.strip_suffix("buff")?, id.parse::<u32>().ok()?)))
        else {
            continue;
        };
        match mode {
            "+" => ensure!(
                created.contains(&id),
                "expected own buff {id} created; got {created:?}"
            ),
            "!" => ensure!(!created.contains(&id), "own buff {id} was created"),
            "-" => ensure!(
                removed.contains(&id),
                "expected own buff {id} removed; got {removed:?}"
            ),
            "=" => ensure!(!removed.contains(&id), "own buff {id} was removed"),
            _ => {}
        }
    }
    Ok(())
}

fn game_log() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::env::var("AAEMU_VERIFY_GAME_LOG")
        .context("set AAEMU_VERIFY_GAME_LOG to the game server log")?
        .into())
}

fn log_len() -> anyhow::Result<u64> {
    Ok(std::fs::metadata(game_log()?)?.len())
}

/// The game-log text written since `offset`, once a line matching `pattern` is in it (or
/// after `secs`).
async fn log_with(
    game: &mut GameClient,
    offset: u64,
    pattern: &str,
    secs: u64,
) -> anyhow::Result<String> {
    let path = game_log()?;
    let re = regex::Regex::new(pattern)?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut off = offset;
        let text = super::script::read_log_since(&path, &mut off)?;
        if re.is_match(&text) || Instant::now() >= deadline {
            return Ok(text);
        }
        game.recv_within(Duration::from_millis(100)).await?;
    }
}

/// The song id of a scalar SQL on the lane game database (`AAEMU_VERIFY_MYSQL`).
async fn db_scalar(sql: &str) -> anyhow::Result<String> {
    let spec = std::env::var("AAEMU_VERIFY_MYSQL")
        .context("set AAEMU_VERIFY_MYSQL to 'mysql.exe <args...> <database>'")?;
    let mut parts = super::script::split_quoted(&spec)?;
    ensure!(!parts.is_empty(), "AAEMU_VERIFY_MYSQL is empty");
    let exe = parts.remove(0);
    let sql = sql.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .args(&parts)
            .args(["-N", "-B", "-e", &sql])
            .output()
    })
    .await??;
    ensure!(out.status.success(), "mysql failed");
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// Walks the `SCItemTaskSuccess` tasks of `packets`: prints them and returns created stacks.
fn scan_items(packets: &[Packet], print: bool) -> Vec<(u32, Held)> {
    macro_rules! say {
        ($($a:tt)*) => {
            if print {
                println!($($a)*);
            }
        };
    }
    let mut out = Vec::new();
    for p in packets
        .iter()
        .filter(|p| p.is_sc(sc::SCItemTaskSuccessPacket))
    {
        let mut s = p.stream();
        let r = (|| -> anyhow::Result<()> {
            let action = s.read_u8()?;
            let count = s.read_u8()?;
            for _ in 0..count {
                let task = s.read_u8()?;
                match task {
                    1 | 2 | 16 | 17 => say!("    task {task}: amount {}", s.read_i32()?),
                    4 => {
                        let (t, sl) = (s.read_u8()?, s.read_u8()?);
                        let (id, n, tpl) = (s.read_u64()?, s.read_i32()?, s.read_u32()?);
                        say!("    task 4 count-update tpl{tpl} id{id} slot{t}/{sl} count {n}");
                        out.push((
                            tpl,
                            Held {
                                id,
                                slot_type: t,
                                slot: sl,
                                ..Held::default()
                            },
                        ));
                    }
                    5 | 6 | 15 => {
                        let (slot_type, slot) = (s.read_u8()?, s.read_u8()?);
                        let tpl = s.read_u32()?;
                        let id = s.read_u64()?;
                        let grade = s.read_u8()?;
                        s.read_u8()?;
                        let n = s.read_i32()?;
                        let dl = s.read_i16()?;
                        let details = s.read_bytes(dl.max(0) as usize)?;
                        s.read_i64()?;
                        s.read_i32()?;
                        let made_unit = s.read_u32()?;
                        s.read_u8()?;
                        s.read_i64()?;
                        s.read_i64()?;
                        let song_id = (details.first() == Some(&8)).then(|| {
                            u64::from_le_bytes(details[1..9].try_into().unwrap_or([0; 8]))
                        });
                        say!(
                            "    task {task} create tpl{tpl} id{id} slot{slot_type}/{slot} grade{grade} count{n} song{song_id:?}"
                        );
                        out.push((
                            tpl,
                            Held {
                                id,
                                slot_type,
                                slot,
                                grade,
                                made_unit,
                                song_id,
                            },
                        ));
                    }
                    7 => {
                        let (t, sl) = (s.read_u8()?, s.read_u8()?);
                        let (id, tpl) = (s.read_u64()?, s.read_u32()?);
                        say!("    task 7 remove tpl{tpl} id{id} slot{t}/{sl}");
                    }
                    other => {
                        say!("    task {other} (action {action}; rest not decoded)");
                        break;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            say!("    item task not fully decoded: {e}");
        }
    }
    out
}

/// `kind` of a skill packet's caster and whether it is our own actor (and item id for items).
struct SkillHead {
    skill: u32,
    tl: u16,
    caster: SkillCaster,
}

fn skill_head(p: &Packet) -> Option<SkillHead> {
    let mut s = p.stream();
    let skill = s.read_u32().ok()?;
    let tl = s.read_u16().ok()?;
    let kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(kind, &mut s).ok()?;
    Some(SkillHead { skill, tl, caster })
}

fn own(head: &SkillHead, me: u32) -> bool {
    match head.caster {
        SkillCaster::Unit(o)
        | SkillCaster::Unk1(o)
        | SkillCaster::Item { obj_id: o, .. }
        | SkillCaster::Mount { obj_id: o, .. } => o == me,
        SkillCaster::Doodad(_) => false,
    }
}

/// Prints what matters of one received packet (music, skill and buff lifecycle).
fn describe(p: &Packet, t0: Instant, me: u32) {
    let at = ms(t0);
    if p.is_sc(sc::SCUserNoteLoadedPacket) {
        if p.body.is_empty() {
            println!("    +{at}ms SCUserNoteLoaded (empty: unknown song)");
            return;
        }
        let mut s = p.stream();
        let r = (|| -> anyhow::Result<String> {
            let id = s.read_u32()?;
            let tooltip = s.read_bool()?;
            let len = s.read_i32()?;
            let title = s.read_string()?;
            let notes = s.read_string()?;
            Ok(format!(
                "id {id} tooltip {tooltip} len {len} title {title:?} notes {notes:?}"
            ))
        })();
        println!(
            "    +{at}ms SCUserNoteLoaded {}",
            r.unwrap_or_else(|e| format!("undecoded: {e}"))
        );
    } else if p.is_sc(sc::SCSendUserMusicPacket) {
        let mut s = p.stream();
        let r = (|| -> anyhow::Result<String> {
            let n = s.read_u32()?;
            let obj = s.read_bc()?;
            let author = s.read_string()?;
            let midi = s.read_bytes_sized()?;
            Ok(format!(
                "len {n} obj {obj} (self={}) author {author:?} midi {} bytes fnv {:016x}",
                obj == me,
                midi.len(),
                fnv(&midi)
            ))
        })();
        println!(
            "    +{at}ms SCSendUserMusic {}",
            r.unwrap_or_else(|e| format!("undecoded: {e}"))
        );
    } else if p.is_sc(sc::SCPauseUserMusicPacket) {
        let obj = p.stream().read_bc().unwrap_or(0);
        println!(
            "    +{at}ms SCPauseUserMusic obj {obj} (self={})",
            obj == me
        );
    } else if let Some((owner, index, buff)) = buff_created(p) {
        remember_buff(owner, index, buff);
        println!(
            "    +{at}ms SCBuffCreated buff {buff} index {index} owner {owner} (self={})",
            owner == me
        );
    } else if let Some((owner, index)) = buff_removed(p) {
        println!(
            "    +{at}ms SCBuffRemoved buff {:?} index {index} owner {owner} (self={})",
            buff_of(owner, index),
            owner == me
        );
    } else if p.is_sc(sc::SCCharacterLaborPowerChangedPacket) {
        let mut s = p.stream();
        println!(
            "    +{at}ms SCCharacterLaborPowerChanged amount {:?} action {:?} point {:?}",
            s.read_i32().ok(),
            s.read_i32().ok(),
            s.read_i32().ok()
        );
    } else if p.is_sc(sc::SCSkillStartedPacket)
        || p.is_sc(sc::SCSkillFiredPacket)
        || p.is_sc(sc::SCSkillEndedPacket)
    {
        match skill_head(p) {
            Some(h) => println!(
                "    +{at}ms {} skill {} tl {} caster {:?} (own={})",
                p.name(),
                h.skill,
                h.tl,
                h.caster,
                own(&h, me)
            ),
            None => println!("    +{at}ms {} undecoded", p.name()),
        }
    } else if p.is_sc(sc::SCErrorMsgPacket) {
        println!("    +{at}ms SCErrorMsg {:?}", p.stream().read_i16());
    } else if let Some(t) = chat_text(p) {
        println!("    +{at}ms chat: {t}");
    }
}

/// Reads packets until `quiet` passes without one (or `max` elapsed), describing them all.
async fn settle(
    game: &mut GameClient,
    t0: Instant,
    quiet: Duration,
    max: Duration,
) -> anyhow::Result<Vec<Packet>> {
    let me = game.state.self_obj;
    let start = Instant::now();
    let mut last = Instant::now();
    let mut all = Vec::new();
    while start.elapsed() < max && last.elapsed() < quiet {
        if let Some(p) = game.recv_within(Duration::from_millis(50)).await? {
            last = Instant::now();
            describe(&p, t0, me);
            all.push(p);
        }
    }
    scan_items(&all, true);
    Ok(all)
}

fn held(m: &Music, tpl: u32) -> anyhow::Result<&Held> {
    m.held
        .get(&tpl)
        .with_context(|| format!("item {tpl} not added with `mitem`"))
}

pub(super) fn is_command(c: &str) -> bool {
    matches!(
        c,
        "mitem"
            | "mequip"
            | "mcompose"
            | "mdb"
            | "msong"
            | "mrequest"
            | "mskill"
            | "mend"
            | "mwatch"
    )
}

fn midi_message(midi: &[u8]) -> Vec<u8> {
    let mut u = PacketStream::new();
    u.write_u32(midi.len() as u32)
        .write_u16(midi.len() as u16)
        .write_raw(midi);
    u.as_bytes().to_vec()
}

/// (song id, tooltip flag, title, notes) of every non-empty `SCUserNoteLoaded` in `packets`.
fn notes_loaded(packets: &[Packet]) -> Vec<(u32, bool, String, String)> {
    packets
        .iter()
        .filter(|p| p.is_sc(sc::SCUserNoteLoadedPacket) && !p.body.is_empty())
        .filter_map(|p| {
            let mut s = p.stream();
            let id = s.read_u32().ok()?;
            let tooltip = s.read_bool().ok()?;
            s.read_i32().ok()?;
            Some((id, tooltip, s.read_string().ok()?, s.read_string().ok()?))
        })
        .collect()
}

/// Dispatches one `m*` command. `args` are the quote-aware script arguments.
pub(super) async fn command(
    game: &mut GameClient,
    m: &mut Music,
    cmd: &str,
    args: &[String],
) -> anyhow::Result<()> {
    let arg = |i: usize| -> anyhow::Result<&str> {
        args.get(i)
            .map(String::as_str)
            .with_context(|| format!("{cmd}: missing argument {i}"))
    };
    let me = game.state.self_obj;
    match cmd {
        "mitem" => {
            let tpl: u32 = arg(0)?.parse()?;
            let count: u32 = arg(1)?.parse()?;
            let reply = gm_command(game, &format!("item add self {tpl} {count}")).await?;
            let created = scan_items(&reply, true);
            let (_, h) = created
                .into_iter()
                .rev()
                .find(|(t, _)| *t == tpl)
                .with_context(|| format!("mitem: no created stack of {tpl} in the reply"))?;
            println!("    MUSIC mitem tpl {tpl} id {} bag slot {}", h.id, h.slot);
            m.held.insert(tpl, h);
        }
        "mequip" => {
            let tpl: u32 = arg(0)?.parse()?;
            let to: u8 = arg(1)?.parse()?;
            let h = held(m, tpl)?.clone();
            let mut s = PacketStream::new();
            s.write_u64(h.id)
                .write_u64(0)
                .write_u8(h.slot_type)
                .write_u8(h.slot)
                .write_u8(SLOT_EQUIP)
                .write_u8(to);
            game.send_cs(cs::CSSwapItemsPacket, s.as_bytes()).await?;
            let t0 = Instant::now();
            let got = settle(
                game,
                t0,
                Duration::from_millis(1200),
                Duration::from_secs(6),
            )
            .await?;
            ensure!(!got.is_empty(), "mequip: no reply to the swap");
            println!(
                "    MUSIC mequip tpl {tpl} -> equip slot {to}: {} packets",
                got.len()
            );
        }
        "mcompose" => {
            let grade: u32 = arg(0)?.parse()?;
            let (title, notes) = (arg(1)?.to_string(), arg(2)?.to_string());
            let paper = held(m, PAPER)?.clone();
            let log0 = log_len()?;
            let mut s = PacketStream::new();
            s.write_u32(SKILL_COMPOSE)
                .write_u8(2) // SkillCasterType.Item
                .write_bc(me)
                .write_u64(paper.id)
                .write_u32(PAPER)
                .write_u8(paper.grade)
                .write_u32(paper.made_unit)
                .write_u8(0) // SkillCastUnitTarget
                .write_bc(me)
                .write_u8(5) // SkillObject type 5: composition grade
                .write_u32(grade);
            let t0 = Instant::now();
            game.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await?;
            println!(
                "    MUSIC mcompose: CS052 sent (paper {} grade {grade})",
                paper.id
            );
            // Wait for the own item-caster SkillFired, then upload exactly like the client.
            let mut all = Vec::new();
            let mut started_at = None;
            let mut fired_at = None;
            while t0.elapsed() < Duration::from_secs(10) && fired_at.is_none() {
                let Some(p) = game.recv_within(Duration::from_millis(20)).await? else {
                    continue;
                };
                describe(&p, t0, me);
                let head = skill_head(&p).filter(|h| h.skill == SKILL_COMPOSE && own(h, me));
                if p.is_sc(sc::SCSkillStartedPacket) && head.as_ref().is_some_and(|h| h.tl != 0) {
                    started_at = Some(ms(t0));
                }
                let ok = p.is_sc(sc::SCSkillFiredPacket)
                    && head.as_ref().is_some_and(|h| h.caster.is_item());
                all.push(p);
                if ok {
                    fired_at = Some(ms(t0));
                    let mut u = PacketStream::new();
                    u.write_u32(notes.len() as u32)
                        .write_u64(paper.id)
                        .write_u8(paper.slot_type)
                        .write_u8(paper.slot)
                        .write_str(&title)
                        .write_str_nul(&notes);
                    game.send_cs(cs::CSSaveUserMusicNotesPacket, u.as_bytes())
                        .await?;
                    println!(
                        "    MUSIC CS122 sent at +{}ms, directly after the received own SkillFired",
                        ms(t0)
                    );
                }
            }
            ensure!(
                started_at.is_some(),
                "no own nonzero-timeline Started for {SKILL_COMPOSE}"
            );
            let fired_at = fired_at.context("no own item SCSkillFired for the compose skill")?;
            let rest = settle(
                game,
                t0,
                Duration::from_millis(1500),
                Duration::from_secs(6),
            )
            .await?;
            let created = scan_items(&all, true)
                .into_iter()
                .chain(scan_items(&rest, false))
                .collect::<Vec<_>>();
            println!(
                "    MUSIC mcompose timing: started +{}ms fired +{fired_at}ms (cast 3000 ms, effect delay 200 ms)",
                started_at.unwrap_or(0)
            );
            let (_, sheet) = created
                .iter()
                .rev()
                .find(|(t, h)| *t == SHEET && h.song_id.is_some())
                .context("no MusicSheet 28051 with a detail-8 song id was created")?;
            ensure!(
                created.iter().any(|(t, h)| *t == PAPER && h.id == paper.id),
                "the paper stack was not reduced by the compose"
            );
            ensure!(
                all.iter()
                    .chain(&rest)
                    .any(|p| p.is_sc(sc::SCCharacterLaborPowerChangedPacket)),
                "no labor change after the compose"
            );
            // Server-side order: the upload must be queued before the save effect runs.
            let text = log_with(game, log0, "UserMusicSaveNotes", 5).await?;
            let up = text.find(&format!("Uploading song, title: {title}"));
            let eff = text.find("UserMusicSaveNotes");
            ensure!(
                matches!((up, eff), (Some(u), Some(e)) if u < e),
                "game log: upload not queued before the save effect: {text}"
            );
            println!(
                "    MUSIC server log: CS122 upload line precedes the UserMusicSaveNotes effect"
            );
            let song = sheet.song_id.unwrap_or(0);
            println!("    MUSIC sheet created id {} song {song}", sheet.id);
            m.last_song = Some(song);
            m.songs.insert(song, (title, notes));
            m.held.insert(SHEET, sheet.clone());
        }
        "mdb" => println!("    MUSIC db {:?} = {}", arg(0)?, db_scalar(arg(0)?).await?),
        "msong" => {
            let id: u64 = db_scalar(arg(0)?).await?.parse()?;
            m.last_song = Some(id);
            if let (Some(t), Some(n)) = (args.get(1), args.get(2)) {
                m.songs.insert(id, (t.clone(), n.clone()));
            }
            println!("    MUSIC msong: song id {id}");
        }
        "mrequest" => {
            let tooltip = arg(0)? == "1";
            let buff_only = arg(1)? == "1";
            let unknown = args.get(2).is_some_and(|v| v == "unknown");
            let id: u64 = match args.get(2) {
                _ if unknown => 4_000_000_000,
                Some(v) => v.parse()?,
                None => m
                    .last_song
                    .context("mrequest: no song id known (compose first)")?,
            };
            let mut s = PacketStream::new();
            s.write_u64(id).write_bool(tooltip).write_bool(buff_only);
            let t0 = Instant::now();
            game.send_cs(cs::CSRequestMusicNotesPacket, s.as_bytes())
                .await?;
            println!("    MUSIC CS123 id {id} tooltip {tooltip} buff-only {buff_only}");
            let got = settle(
                game,
                t0,
                Duration::from_millis(1500),
                Duration::from_secs(6),
            )
            .await?;
            let loaded = notes_loaded(&got);
            let empty_reply = got
                .iter()
                .any(|p| p.is_sc(sc::SCUserNoteLoadedPacket) && p.body.is_empty());
            if buff_only {
                ensure!(
                    loaded.is_empty() && !empty_reply,
                    "buff-only request got a note reply"
                );
                check_buff_tokens(&["+buff:6010".into()], &got, me)?;
            } else if unknown {
                ensure!(
                    empty_reply && loaded.is_empty(),
                    "unknown song: expected one empty reply"
                );
            } else {
                let (title, notes) = m.songs.get(&id).context("mrequest: unknown song text")?;
                ensure!(
                    loaded == [(id as u32, tooltip, title.clone(), notes.clone())],
                    "reply mismatch: {loaded:?}"
                );
                check_buff_tokens(&["!buff:6010".into()], &got, me)?;
            }
        }
        "mskill" => {
            let skill: u32 = arg(0)?.parse()?;
            let react = arg(1)?;
            ensure!(
                matches!(react, "midi" | "end" | "none" | "premidi"),
                "midi|end|none|premidi"
            );
            let caster = args
                .get(2)
                .filter(|a| matches!(a.as_str(), "unit" | "item"))
                .map_or("unit", String::as_str);
            let diag = args.iter().skip(2).any(|a| a == "diag");
            let tokens: Vec<String> = args
                .iter()
                .skip(2)
                .filter(|a| a.contains("buff:"))
                .cloned()
                .collect();
            let log0 = log_len().unwrap_or(0);
            let mut s = PacketStream::new();
            s.write_u32(skill);
            if caster == "item" {
                let sheet = held(m, SHEET)?;
                s.write_u8(2)
                    .write_bc(me)
                    .write_u64(sheet.id)
                    .write_u32(SHEET)
                    .write_u8(sheet.grade)
                    .write_u32(sheet.made_unit);
            } else {
                s.write_u8(0).write_bc(me);
            }
            s.write_u8(0).write_bc(me).write_u8(0);
            if react == "premidi" {
                // Diagnostic only: CS121 before the cast, not what the client does.
                game.send_cs(cs::CSSendUserMusicPacket, &midi_message(&SMF))
                    .await?;
                println!(
                    "    MUSIC DIAG CS121 sent BEFORE the cast (fnv {:016x})",
                    fnv(&SMF)
                );
            }
            let t0 = Instant::now();
            game.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await?;
            println!("    MUSIC mskill {skill} ({caster} caster) sent, react {react}");
            let mut all = Vec::new();
            let mut started = None;
            while t0.elapsed() < Duration::from_secs(8) && started.is_none() {
                let Some(p) = game.recv_within(Duration::from_millis(20)).await? else {
                    continue;
                };
                describe(&p, t0, me);
                let own_started = p.is_sc(sc::SCSkillStartedPacket)
                    && skill_head(&p).is_some_and(|h| h.skill == skill && own(&h, me) && h.tl != 0);
                all.push(p);
                if own_started {
                    started = Some(ms(t0));
                    match react {
                        "midi" => {
                            *SENT_MIDI.lock().map_err(|_| anyhow::anyhow!("poisoned"))? =
                                Some(fnv(&SMF));
                            game.send_cs(cs::CSSendUserMusicPacket, &midi_message(&SMF))
                                .await?;
                            println!(
                                "    MUSIC CS121 sent ({} bytes, fnv {:016x}) at +{}ms, after the received own Started",
                                SMF.len(),
                                fnv(&SMF),
                                ms(t0)
                            );
                        }
                        "end" => {
                            game.send_cs(cs::CSEndMusicPacket, &[]).await?;
                            println!(
                                "    MUSIC CS124 sent at +{}ms, after the received own Started",
                                ms(t0)
                            );
                        }
                        _ => {}
                    }
                }
            }
            if started.is_none() {
                println!("    MUSIC no own nonzero-timeline Started for {skill} within 8 s");
            }
            all.extend(
                settle(
                    game,
                    t0,
                    Duration::from_millis(1500),
                    Duration::from_secs(6),
                )
                .await?,
            );
            if !diag {
                ensure!(
                    started.is_some(),
                    "mskill {skill}: no own nonzero-timeline SCSkillStarted received (CS121/CS124 not sent)"
                );
                match react {
                    "midi" | "premidi" => {
                        let want = fnv(&SMF);
                        ensure!(
                            all.iter()
                                .filter_map(send_music)
                                .any(|(o, _, midi)| o == me && fnv(&midi) == want),
                            "no own SCSendUserMusic carrying the sent MIDI"
                        );
                    }
                    "end" => {
                        ensure!(
                            all.iter().any(|p| p.is_sc(sc::SCPauseUserMusicPacket)
                                && p.stream().read_bc().ok() == Some(me)),
                            "no own SCPauseUserMusic"
                        );
                        let text = log_with(game, log0, "CSEndMusicPacket", 5).await?;
                        ensure!(
                            text.contains("CSEndMusicPacket"),
                            "game log shows no CS124 receipt"
                        );
                        println!("    MUSIC server log: CSEndMusicPacket received");
                    }
                    _ => {}
                }
            }
            check_buff_tokens(&tokens, &all, me)?;
        }
        "mend" => {
            let log0 = log_len()?;
            let tokens: Vec<String> = args.to_vec();
            let t0 = Instant::now();
            game.send_cs(cs::CSEndMusicPacket, &[]).await?;
            println!("    MUSIC CS124 sent (explicit End)");
            let got = settle(
                game,
                t0,
                Duration::from_millis(1200),
                Duration::from_secs(4),
            )
            .await?;
            let text = log_with(game, log0, "CSEndMusicPacket", 5).await?;
            ensure!(
                text.contains("CSEndMusicPacket"),
                "game log shows no CS124 receipt"
            );
            println!("    MUSIC server log: CSEndMusicPacket received");
            check_buff_tokens(&tokens, &got, me)?;
        }
        "mwatch" => {
            let secs: u64 = arg(0)?.parse()?;
            let (mut send, mut pause) = (0, 0);
            let mut sends = Vec::new();
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(secs) {
                if let Some(p) = game.recv_within(Duration::from_millis(100)).await? {
                    describe(&p, t0, me);
                    send += usize::from(p.is_sc(sc::SCSendUserMusicPacket));
                    pause += usize::from(p.is_sc(sc::SCPauseUserMusicPacket));
                    sends.extend(send_music(&p));
                }
            }
            println!("    MUSIC mwatch: {send} SCSendUserMusic, {pause} SCPauseUserMusic");
            for want in &args[1..] {
                match want.as_str() {
                    "send" => {
                        ensure!(send > 0, "expected SCSendUserMusic");
                        let sent = *SENT_MIDI.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
                        ensure!(
                            sends
                                .iter()
                                .any(|(o, _, midi)| *o != me && Some(fnv(midi)) == sent),
                            "no other-actor SCSendUserMusic with the MIDI the composer sent ({sent:?})"
                        );
                    }
                    "pause" => ensure!(pause > 0, "expected SCPauseUserMusic"),
                    "nosend" => ensure!(send == 0, "unexpected SCSendUserMusic"),
                    "nopause" => ensure!(pause == 0, "unexpected SCPauseUserMusic"),
                    other => bail!("mwatch: unknown expectation {other}"),
                }
            }
        }
        _ => bail!("unknown music command {cmd}"),
    }
    Ok(())
}
