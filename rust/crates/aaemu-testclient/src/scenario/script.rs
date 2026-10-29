//! `script`: runs the commands of the file named by `$AAEMU_SCRIPT` after the usual login /
//! select / spawn, printing what the server sends back. Meant for ad-hoc live checks of quest
//! and spawn fixes (GM account needed), so a new check needs no rebuild of the bot.
//!
//! One command per line, `#` starts a comment:
//!
//! ```text
//! gm <command line>                  GM chat command (without the leading slash)
//! tp <x> <y> <z>                     /move there, acknowledge the teleport, wait for the area
//! pump <secs>                        just read packets
//! units [<npc template>...]          NPCs in sight (all within 60 m when no template given)
//! doodads [<doodad template>...]     doodads in sight (all within 40 m when none given)
//! sq <quest> <npc template|0>        CSStartQuestContext from the nearest NPC of that template
//! cq <quest> <npc template|0> [sel]  CSCompleteQuestContext at that NPC
//! target <npc template>              select the nearest NPC of that template
//! kill <npc template>                walk next to the nearest NPC, target it, `/kill`
//! cast <skill> <npc template>        CSStartSkill on the nearest NPC
//! dcast <skill> <doodad template>    doodad-target CSStartSkill
//! item <template> <count>            `/item add self` and remember the stack id
//! icast <skill> <item template> <npc template|self|peer|player:NAME> [metres [max secs]]
//!                                    cast from the remembered item stack
//! waitpeer [secs] [name]             read packets until another player is in sight (60 s default)
//! rawcs <opcode hex> [<body hex>]    send any client packet (`SELF` / `PEER` in the body hex)
//! pkill [name]                       `/kill` with the peer targeted (GM)
//! await <Event> [secs]               wait (60 s default) for a party/guild event and consume it
//! partyinvite <name>                 CSInviteToTeam (party) by character name
//! partyaccept                        wait for SCAskToJoinTeam and accept it
//! gcreate <name>                     CSCreateExpedition
//! ginvite [name]                     CSInviteToExpedition to the peer
//! gaccept                            wait for the guild invitation and join
//! greply <faction id> <0|1>          CSReplyExpeditionInvitation without waiting for an invite
//! sync <token> [count]              suite barrier: wait until `count` bots (default: all) are here
//! expectpkt <PacketName> [secs]     fail unless the packet arrives within `secs` (default 5)
//! expectnopkt <PacketName> [secs]   fail if the packet arrives within `secs` (default 5)
//! expectlog <regex> [secs]          fail unless a new game-log line matches (AAEMU_VERIFY_GAME_LOG)
//! expectdb "<sql>" <value> [secs]   fail unless the SQL's value equals `value`
//!                                   (AAEMU_VERIFY_MYSQL; re-checked for `secs` when given)
//! rezexpect inplace|temple ok|refused  CSResurrectCharacter, then fail unless SCCharacterResurrected
//!                                    for our own unit did (ok) / did not (refused) arrive
//! lootexpect took|failed [npc obj]  like `loot`, then fail unless SCLootItemTook (took) / SCLootItemFailed
//!                                    (failed) was among the replies; `took` first walks to 2 m of
//!                                    the corpse, as the real client does
//! ```
//!
//! Every failed expectation prints a `FAIL` line and makes the run exit non-zero.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{cs, sc};
use anyhow::{bail, Context};

use super::questitem::{chat_text, gm_command, quest_state, stack_id};
use super::{pump, skill_response_result, Runner};
use crate::game::{parse_item_task, GameClient, Packet, UnitInfo};
use crate::social::{self, SocialEvent};

/// How close `lootexpect took` walks to a corpse first (retail enables looting at 3 m or less).
const LOOT_APPROACH_RANGE: f32 = 2.0;

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

/// `SCBuffCreatedPacket`: (owner obj id, buff template id).
fn buff_created(p: &Packet) -> Option<(u32, u32)> {
    use aaemu_game::systems::skills::types::SkillCaster;
    let mut s = p.stream();
    let kind = s.read_u8().ok()?;
    SkillCaster::read(kind, &mut s).ok()?;
    s.read_u32().ok()?;
    let owner = s.read_bc().ok()?;
    s.read_u32().ok()?;
    let buff = s.read_u32().ok()?;
    Some((owner, buff))
}

fn show(packets: &[Packet]) {
    let mut other: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for p in packets {
        let name = p.name();
        if let Some(t) = chat_text(p) {
            println!("    chat: {t}");
        } else if let Some((q, st)) = quest_state(p) {
            println!(
                "    {name} quest {q} status {} objectives {:?}",
                st.status, st.objectives
            );
        } else if p.is_sc(sc::SCBuffCreatedPacket) {
            match buff_created(p) {
                Some((owner, buff)) => println!("    SCBuffCreated owner {owner} buff {buff}"),
                None => println!("    SCBuffCreated (unreadable)"),
            }
        } else if p.is_sc(sc::SCBuffRemovedPacket) {
            let mut s = p.stream();
            if let (Ok(owner), Ok(index)) = (s.read_bc(), s.read_u32()) {
                println!("    SCBuffRemoved owner {owner} index {index}");
            }
        } else if p.is_sc(sc::SCErrorMsgPacket) {
            println!("    SCErrorMsg {:?}", p.stream().read_i16());
        } else if p.is_sc(sc::SCItemTaskSuccessPacket) {
            let t = parse_item_task(&p.body);
            let items: Vec<String> = t
                .items
                .iter()
                .map(|i| format!("task{} tpl{} id{}", i.task, i.template, i.id))
                .collect();
            println!(
                "    ItemTask action {} money {} items [{}]",
                t.action,
                t.money,
                items.join("; ")
            );
        } else if name.contains("Quest")
            || name.contains("Exp")
            || name.contains("Death")
            || name.contains("Teleport")
            || name.contains("Mail")
            || name.contains("Gold")
        {
            let hex: String = p.body.iter().take(48).map(|b| format!("{b:02x}")).collect();
            println!("    {name} len {} {hex}", p.body.len());
        } else {
            *other.entry(name).or_default() += 1;
        }
    }
    if !other.is_empty() {
        let o: Vec<String> = other.iter().map(|(k, v)| format!("{k}x{v}")).collect();
        println!("    (other: {})", o.join(", "));
    }
}

/// Reads packets for `secs` and prints only the quest ones.
async fn show_quiet(game: &mut GameClient, secs: f32) -> anyhow::Result<()> {
    let ms = (secs * 1000.0) as u64;
    let got = game
        .drain(Duration::from_millis(ms), Duration::from_millis(ms))
        .await?;
    for p in got.iter().filter(|p| p.name().contains("Quest")) {
        if let Some((q, st)) = quest_state(p) {
            println!(
                "    {} quest {q} status {} objectives {:?}",
                p.name(),
                st.status,
                st.objectives
            );
        } else {
            println!("    {}", p.name());
        }
    }
    Ok(())
}

fn nearest(game: &GameClient, tpl: u32) -> Option<UnitInfo> {
    let me = game.state.self_pos;
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 1 && u.id == tpl)
        .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
        .cloned()
}

fn num<T: std::str::FromStr>(parts: &[&str], i: usize) -> anyhow::Result<T> {
    parts
        .get(i)
        .with_context(|| format!("missing argument {i}"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("bad number '{}'", parts[i]))
}

/// The other player to target: `name`, else the first other player in sight.
fn peer(game: &GameClient, name: Option<&str>) -> anyhow::Result<UnitInfo> {
    let me = game.state.self_obj;
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 0 && u.obj_id != me)
        .filter(|u| name.is_none_or(|n| u.name.eq_ignore_ascii_case(n)))
        .min_by_key(|u| u.obj_id)
        .cloned()
        .context("no other player in sight (run `pump` after both bots logged in)")
}

/// Variant name of a social event (`TeamJoined { .. }` -> `TeamJoined`).
fn event_kind(e: &SocialEvent) -> String {
    let d = format!("{e:?}");
    d.split([' ', '{', '(']).next().unwrap_or("").to_string()
}

/// Waits for the first not yet consumed social event named `kind` (case-insensitive),
/// reading packets for at most `secs`.
async fn await_event(
    game: &mut GameClient,
    consumed: &mut HashSet<usize>,
    kind: &str,
    secs: u64,
) -> anyhow::Result<SocialEvent> {
    let want = kind.to_ascii_lowercase();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let found = game
            .state
            .social
            .iter()
            .enumerate()
            .find(|(i, e)| !consumed.contains(i) && event_kind(e).to_ascii_lowercase() == want)
            .map(|(i, e)| (i, e.clone()));
        if let Some((i, e)) = found {
            consumed.insert(i);
            return Ok(e);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("timed out ({secs} s) waiting for social event '{kind}'");
        }
        game.recv_within(left.min(Duration::from_millis(500)))
            .await?;
    }
}

/// Splits an argument line, keeping double-quoted spans together (`expectdb "<sql>" <value>`,
/// `expectlog "<regex>" [secs]`). Inside quotes, `\"` and `\\` are escapes.
pub(super) fn split_quoted(s: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            // Only `\"` and `\\` are escapes; `\d` in a regex stays intact.
            if c != '"' && c != '\\' {
                cur.push('\\');
            }
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if quoted {
        bail!("unterminated quote in {s:?}");
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Reads the complete lines appended to a log file since `offset`, then advances `offset`.
pub(super) fn read_log_since(path: &std::path::Path, offset: &mut u64) -> anyhow::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {path:?}"))?;
    f.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    // Only complete lines count (a line still being written is read next time).
    let complete = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    *offset += complete as u64;
    Ok(String::from_utf8_lossy(&buf[..complete]).into_owned())
}

/// `expectpkt <PacketName> [secs]`.
async fn expect_packet(game: &mut GameClient, want: &str, secs: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected packet {want} within {secs} s; it did not arrive");
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(250)))
            .await?
        {
            if p.name().eq_ignore_ascii_case(want) {
                println!("    got {want}");
                return Ok(());
            }
        }
    }
}

/// `expectnopkt <PacketName> [secs]`.
async fn expect_no_packet(game: &mut GameClient, want: &str, secs: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            println!("    no {want} in {secs} s");
            return Ok(());
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(250)))
            .await?
        {
            if p.name().eq_ignore_ascii_case(want) {
                bail!("unexpected packet {want} within {secs} s");
            }
        }
    }
}

/// `expectlog <regex> [secs]`: a line written after the command started must match.
async fn expect_log(game: &mut GameClient, pattern: &str, secs: u64) -> anyhow::Result<()> {
    let path = std::env::var("AAEMU_VERIFY_GAME_LOG")
        .context("set AAEMU_VERIFY_GAME_LOG to the game server log")?;
    let path = std::path::Path::new(&path);
    let re = regex::Regex::new(pattern).with_context(|| format!("bad regex {pattern:?}"))?;
    let mut offset = std::fs::metadata(path)
        .with_context(|| format!("reading game log {path:?}"))?
        .len();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let text = read_log_since(path, &mut offset)?;
        if let Some(line) = text.lines().find(|l| re.is_match(l)) {
            println!("    log: {line}");
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("no new line matching /{pattern}/ within {secs} s in {path:?}");
        }
        // Keep the session alive (and the packet state fresh) while polling.
        game.recv_within(left.min(Duration::from_millis(200)))
            .await?;
    }
}

/// `expectdb "<sql>" <expected> [secs]`: the SQL's single value must equal `expected`
/// (re-checked every 250 ms when `secs` is given).
async fn expect_db(sql: &str, expected: &str, secs: u64) -> anyhow::Result<()> {
    let spec = std::env::var("AAEMU_VERIFY_MYSQL")
        .context("set AAEMU_VERIFY_MYSQL to 'mysql.exe <args...> <database>'")?;
    let mut parts = split_quoted(&spec)?;
    anyhow::ensure!(!parts.is_empty(), "AAEMU_VERIFY_MYSQL is empty");
    let exe = parts.remove(0);
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        // mysql.exe blocks: run it off the async runtime so the bot's connection keeps going.
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&parts).args(["-N", "-B", "-e", sql]);
        let out = tokio::task::spawn_blocking(move || cmd.output())
            .await?
            .with_context(|| format!("running {exe}"))?;
        if !out.status.success() {
            bail!(
                "mysql failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if got == expected {
            println!("    db {sql:?} = {got}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("expected {expected:?} from {sql:?}; got {got:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &super::BotConfig,
    r: &mut Runner<'_>,
    input: &super::ScriptInput<'_>,
) -> anyhow::Result<()> {
    let mut current = String::new();
    run_lines(game, cfg, r, input, &mut current)
        .await
        .with_context(|| format!("at script line {current}"))
}

async fn run_lines(
    game: &mut GameClient,
    cfg: &super::BotConfig,
    r: &mut Runner<'_>,
    input: &super::ScriptInput<'_>,
    current_line: &mut String,
) -> anyhow::Result<()> {
    let text = input.text;
    let s = r.begin("game.script");
    let mut items: HashMap<u32, u64> = HashMap::new();
    let mut music = super::music::Music::default();
    let mut clock = 40_000u32;
    let mut last_npc = 0u32;
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut mate: Option<(u32, u16)> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        *current_line = format!("{}: {line}", n + 1);
        println!("> [{}] {line}", n + 1);
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let parts: Vec<&str> = rest.split_whitespace().collect();
        match cmd {
            "gm" => {
                let reply = gm_command(game, rest).await?;
                show(&reply);
            }
            "tp" => {
                let (x, y, z): (f32, f32, f32) =
                    (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                game.state.last_teleport = None;
                game.chat(0, &format!("/move {x} {y} {z}")).await?;
                game.wait_for("SCTeleportUnitPacket", |p| {
                    p.is_sc(sc::SCTeleportUnitPacket)
                })
                .await?;
                let at = game.state.self_pos;
                game.teleport_ended(at).await?;
                clock += 1500;
                game.move_to(at, clock).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                println!(
                    "    teleported to {at:?}; {} units, {} doodads in sight",
                    game.state.units.len(),
                    game.state.doodads.len()
                );
                show(&got);
            }
            "pump" => show(&pump(game, num(&parts, 0)?).await?),
            "units" => {
                let tpls: Vec<u32> = parts.iter().filter_map(|p| p.parse().ok()).collect();
                let me = game.state.self_pos;
                let mut us: Vec<&UnitInfo> = game
                    .state
                    .units
                    .values()
                    .filter(|u| {
                        u.unit_type == 1
                            && if tpls.is_empty() {
                                dist(u.pos, me) < 60.0
                            } else {
                                tpls.contains(&u.id)
                            }
                    })
                    .collect();
                us.sort_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)));
                println!("    me {me:?}, {} NPC(s):", us.len());
                for u in us {
                    println!(
                        "    npc tpl {} obj {} '{}' at ({:.1}, {:.1}, {:.1}) dist {:.1}",
                        u.id,
                        u.obj_id,
                        u.name,
                        u.pos.0,
                        u.pos.1,
                        u.pos.2,
                        dist(u.pos, me)
                    );
                }
            }
            "doodads" => {
                let tpls: Vec<u32> = parts.iter().filter_map(|p| p.parse().ok()).collect();
                let me = game.state.self_pos;
                let mut ds: Vec<_> = game
                    .state
                    .doodads
                    .values()
                    .filter(|d| {
                        if tpls.is_empty() {
                            dist(d.pos, me) < 40.0
                        } else {
                            tpls.contains(&d.template_id)
                        }
                    })
                    .collect();
                ds.sort_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)));
                println!("    me {me:?}, {} doodad(s):", ds.len());
                for d in ds {
                    println!(
                        "    doodad tpl {} obj {} at ({:.1}, {:.1}, {:.1}) phase {} dist {:.1}",
                        d.template_id,
                        d.obj_id,
                        d.pos.0,
                        d.pos.1,
                        d.pos.2,
                        d.func_group_id,
                        dist(d.pos, me)
                    );
                }
            }
            "sq" | "cq" => {
                let quest: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let mut obj = 0;
                if tpl != 0 {
                    let Some(npc) = nearest(game, tpl) else {
                        bail!("NPC {tpl} not in sight");
                    };
                    clock += 1500;
                    game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                        .await?;
                    game.drain(Duration::from_millis(300), Duration::from_secs(1))
                        .await?;
                    obj = npc.obj_id;
                }
                if cmd == "sq" {
                    game.start_quest(quest, obj).await?;
                } else {
                    let selected: i32 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
                    let mut b = aaemu_commons::PacketStream::new();
                    b.write_u32(quest)
                        .write_bc(obj)
                        .write_bc(0)
                        .write_i32(selected);
                    game.send_cs(
                        aaemu_game::opcodes::cs::CSCompleteQuestContextPacket,
                        b.as_bytes(),
                    )
                    .await?;
                }
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "target" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                game.change_target(npc.obj_id).await?;
                println!("    target obj {}", npc.obj_id);
            }
            "kill" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.change_target(npc.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                let got = gm_command(game, "kill").await?;
                println!("    killed obj {} (tpl {tpl})", npc.obj_id);
                last_npc = npc.obj_id;
                show(&got);
            }
            "cast" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.change_target(npc.obj_id).await?;
                game.start_skill(skill, npc.obj_id).await?;
                last_npc = npc.obj_id;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "dcast" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let me = game.state.self_pos;
                let d = game
                    .state
                    .doodads
                    .values()
                    .filter(|d| d.template_id == tpl)
                    .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
                    .cloned()
                    .with_context(|| format!("doodad {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((d.pos.0 + 1.0, d.pos.1, d.pos.2), clock)
                    .await?;
                game.start_doodad_skill(skill, d.obj_id).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            c if c.starts_with('m') && super::music::is_command(c) => {
                let args = split_quoted(rest)?;
                super::music::command(game, &mut music, c, &args).await?;
            }
            "item" => {
                let tpl: u32 = num(&parts, 0)?;
                let count: u32 = num(&parts, 1)?;
                let reply = gm_command(game, &format!("item add self {tpl} {count}")).await?;
                if let Some(id) = stack_id(&reply, tpl) {
                    items.insert(tpl, id);
                    println!("    item {tpl} stack id {id}");
                }
                show(&reply);
            }
            "icast" => {
                let skill: u32 = num(&parts, 0)?;
                let item_tpl: u32 = num(&parts, 1)?;
                let item = *items
                    .get(&item_tpl)
                    .with_context(|| format!("item {item_tpl} not added with `item`"))?;
                let target = match parts.get(2).copied() {
                    Some("self") | None => game.state.self_obj,
                    Some(t) if t == "peer" || t.starts_with("player:") => {
                        let other = peer(game, t.strip_prefix("player:"))?;
                        clock += 1500;
                        game.move_to((other.pos.0 + 1.0, other.pos.1, other.pos.2), clock)
                            .await?;
                        game.change_target(other.obj_id).await?;
                        other.obj_id
                    }
                    Some(t) => {
                        let tpl: u32 = t.parse()?;
                        let npc = nearest(game, tpl)
                            .with_context(|| format!("NPC {tpl} not in sight"))?;
                        clock += 1500;
                        // Optional 4th argument: cast from that many metres away (on the side
                        // the bot came from) instead of 1 m next to the NPC, like a player does.
                        let at = match parts.get(3).and_then(|d| d.parse::<f32>().ok()) {
                            Some(d) => {
                                let me = game.state.self_pos;
                                let (dx, dy) = (me.0 - npc.pos.0, me.1 - npc.pos.1);
                                let len = dx.hypot(dy).max(0.001);
                                (
                                    npc.pos.0 + dx / len * d,
                                    npc.pos.1 + dy / len * d,
                                    npc.pos.2,
                                )
                            }
                            None => (npc.pos.0 + 1.0, npc.pos.1, npc.pos.2),
                        };
                        game.move_to(at, clock).await?;
                        game.change_target(npc.obj_id).await?;
                        npc.obj_id
                    }
                };
                game.start_item_skill(skill, item, item_tpl, target).await?;
                // Optional 5th argument: read packets for at most that many seconds.
                let max_wait = parts
                    .get(4)
                    .and_then(|d| d.parse::<u64>().ok())
                    .unwrap_or(10);
                let got = game
                    .drain(Duration::from_millis(2000), Duration::from_secs(max_wait))
                    .await?;
                for p in &got {
                    if p.is_sc(sc::SCSkillStartedPacket) {
                        println!("    SCSkillStarted");
                    }
                    if p.is_sc(sc::SCSkillEndedPacket) {
                        println!("    SCSkillEnded");
                    }
                    if let Ok(res) = skill_response_result(p) {
                        println!("    {} result {res}", p.name());
                    }
                }
                show(&got);
            }
            "walk" => {
                // Steps of 4 m every 800 ms (5 m/s).
                let to: (f32, f32, f32) = (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let mut at = game.state.self_pos;
                while dist(at, to) > 0.5 {
                    let d = dist(at, to);
                    let k = (4.0 / d).min(1.0);
                    at = (
                        at.0 + (to.0 - at.0) * k,
                        at.1 + (to.1 - at.1) * k,
                        at.2 + (to.2 - at.2) * k,
                    );
                    clock += 800;
                    game.move_to(at, clock).await?;
                    show_quiet(game, 0.8).await?;
                }
                println!("    walked to {:?}", game.state.self_pos);
            }
            "mate" => {
                let reply =
                    gm_command(game, &format!("item add self {} 1", cfg.mate_item_template))
                        .await?;
                let item =
                    stack_id(&reply, cfg.mate_item_template).context("mate item was not added")?;
                let view = super::summon_mate(game, cfg, item, None).await?;
                mate = Some((view.obj_id, view.tl_id));
                println!(
                    "    mate obj {} tl {} hp {}",
                    view.obj_id, view.tl_id, view.hp
                );
            }
            "mount" => {
                let (_, tl) = mate.context("no mate: run `mate` first")?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u16(tl).write_u8(1).write_u8(0);
                game.send_cs(aaemu_game::opcodes::cs::CSMountMatePacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1000), Duration::from_secs(4))
                    .await?;
                show(&got);
            }
            "mwalk" => {
                let (obj, _) = mate.context("no mate: run `mate` first")?;
                let to: (f32, f32, f32) = (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let mut at = game
                    .state
                    .units
                    .get(&obj)
                    .map_or(game.state.self_pos, |u| u.pos);
                while dist(at, to) > 0.5 {
                    let d = dist(at, to);
                    let k = (4.0 / d).min(1.0);
                    at = (
                        at.0 + (to.0 - at.0) * k,
                        at.1 + (to.1 - at.1) * k,
                        at.2 + (to.2 - at.2) * k,
                    );
                    clock += 800;
                    game.move_obj_to(obj, at, clock).await?;
                    show_quiet(game, 0.8).await?;
                }
                println!("    mate walked to {at:?}");
            }
            "loaded" => {
                // The real client's answer to SCLoadInstancePacket, then the world settles.
                game.send_cs(aaemu_game::opcodes::cs::CSInstanceLoadedPacket, &[])
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "sync" => {
                // Suite barrier: wait until `count` bots (default: every bot in the suite)
                // reached the same token.
                let token = parts.first().context("sync needs a token")?;
                let token: String = token
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                let dir = input
                    .sync_dir
                    .context("sync is only available in a verify suite")?;
                let count: usize = parts
                    .get(1)
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(input.bots);
                std::fs::create_dir_all(dir)?;
                std::fs::write(dir.join(format!("{token}.{}", input.bot)), b"")?;
                let deadline = Instant::now() + Duration::from_secs(120);
                loop {
                    let reached = std::fs::read_dir(dir)?
                        .filter_map(Result::ok)
                        .filter(|e| {
                            e.file_name()
                                .to_string_lossy()
                                .starts_with(&format!("{token}."))
                        })
                        .count();
                    if reached >= count {
                        println!("    sync {token}: {reached} bot(s)");
                        break;
                    }
                    if Instant::now() >= deadline {
                        bail!("sync {token}: {reached} of {count} bots reached it within 120 s");
                    }
                    game.recv_within(Duration::from_millis(200)).await?;
                }
            }
            "expectpkt" => {
                let name = parts.first().context("expectpkt <PacketName> [secs]")?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(5);
                expect_packet(game, name, secs).await?;
            }
            "expectnopkt" => {
                let name = parts.first().context("expectnopkt <PacketName> [secs]")?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(5);
                expect_no_packet(game, name, secs).await?;
            }
            "expectlog" => {
                let args = split_quoted(rest)?;
                let pattern = args.first().context("expectlog <regex> [secs]")?;
                let secs: u64 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(5);
                expect_log(game, pattern, secs).await?;
            }
            "expectdb" => {
                let args = split_quoted(rest)?;
                anyhow::ensure!(
                    args.len() >= 2,
                    "expectdb \"<sql>\" <expected> [secs] (one argument line)"
                );
                let secs: u64 = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
                expect_db(&args[0], &args[1], secs).await?;
            }
            "rezexpect" => {
                let in_place = match parts.first().copied() {
                    Some("inplace") => true,
                    Some("temple") => false,
                    _ => bail!("rezexpect inplace|temple ok|refused"),
                };
                let want_ok = match parts.get(1).copied() {
                    Some("ok") => true,
                    Some("refused") => false,
                    _ => bail!("rezexpect inplace|temple ok|refused"),
                };
                game.send_cs(cs::CSResurrectCharacterPacket, &[u8::from(in_place)])
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(5))
                    .await?;
                show(&got);
                let me = game.state.self_obj;
                let revived = got.iter().any(|p| {
                    p.is_sc(sc::SCCharacterResurrectedPacket)
                        && p.stream().read_bc().is_ok_and(|id| id == me)
                });
                if revived != want_ok {
                    bail!("FAIL rezexpect: resurrected={revived}, expected {want_ok}");
                }
                println!("    rezexpect ok (resurrected={revived})");
            }
            "lootexpect" => {
                let want_took = match parts.first().copied() {
                    Some("took") => true,
                    Some("failed") => false,
                    _ => bail!("lootexpect took|failed [npc obj]"),
                };
                let obj: u32 = parts
                    .get(1)
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(last_npc);
                if obj == 0 {
                    bail!("no NPC to loot: run `kill`/`cast` first or pass the object id");
                }
                // The real client walks to the corpse before it loots (retail enables looting
                // at 3 m or less): `took` steps to 2 m of it first, `failed` loots from here.
                if want_took {
                    let corpse = game.state.units.get(&obj).map(|u| u.pos);
                    let me = game.state.self_pos;
                    if let Some(c) = corpse.filter(|&c| dist(me, c) > LOOT_APPROACH_RANGE) {
                        let d = dist(me, c);
                        let k = LOOT_APPROACH_RANGE / d;
                        let at = (c.0 + (me.0 - c.0) * k, c.1 + (me.1 - c.1) * k, c.2);
                        clock += 1000;
                        game.move_to(at, clock).await?;
                        pump(game, 1).await?;
                        println!("    walked to the corpse ({d:.1} m -> {LOOT_APPROACH_RANGE} m)");
                    }
                }
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(obj).write_bc(0).write_bool(true);
                game.send_cs(cs::CSLootOpenBagPacket, b.as_bytes()).await?;
                let got = game
                    .drain(Duration::from_millis(1000), Duration::from_secs(4))
                    .await?;
                show(&got);
                let took = got.iter().any(|p| p.is_sc(sc::SCLootItemTookPacket));
                let failed = got.iter().any(|p| p.is_sc(sc::SCLootItemFailedPacket));
                if want_took && !took {
                    bail!("FAIL lootexpect took: no SCLootItemTook (failed: {failed})");
                }
                if !want_took && (took || !failed) {
                    bail!("FAIL lootexpect failed: SCLootItemFailed expected (took: {took}, failed: {failed})");
                }
                println!(
                    "    lootexpect {} ok",
                    if want_took { "took" } else { "failed" }
                );
            }
            "waitpeer" => {
                let secs: u64 = parts.first().and_then(|x| x.parse().ok()).unwrap_or(60);
                let deadline = Instant::now() + Duration::from_secs(secs);
                loop {
                    if let Ok(p) = peer(game, parts.get(1).copied()) {
                        println!("    peer '{}' char {} obj {}", p.name, p.id, p.obj_id);
                        break;
                    }
                    if Instant::now() >= deadline {
                        bail!("no other player in sight after {secs} s");
                    }
                    game.recv_within(Duration::from_millis(500)).await?;
                }
            }
            "await" => {
                let kind = parts.first().context("await needs an event name")?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(60);
                let e = await_event(game, &mut consumed, kind, secs).await?;
                println!("    awaited: {e:?}");
            }
            "pkill" => {
                let target = peer(game, parts.first().copied())?;
                clock += 1500;
                game.move_to((target.pos.0 + 1.0, target.pos.1, target.pos.2), clock)
                    .await?;
                game.change_target(target.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                let got = gm_command(game, "kill").await?;
                show(&got);
            }
            "rawcs" => {
                let opcode = u16::from_str_radix(parts.first().context("opcode")?, 16)?;
                // `SELF` / `PEER` stand for our own / the first other player's object id in
                // the packet's `bc` encoding.
                let bc = |id: u32| {
                    format!(
                        "{:02x}{:02x}{:02x}",
                        id & 0xff,
                        (id >> 8) & 0xff,
                        (id >> 16) & 0xff
                    )
                };
                let raw = parts.get(1).copied().unwrap_or("");
                let peer_bc = if raw.contains("PEER") {
                    bc(peer(game, None)
                        .context("PEER needs another player in sight")?
                        .obj_id)
                } else {
                    String::new()
                };
                let hex = raw
                    .replace("SELF", &bc(game.state.self_obj))
                    .replace("PEER", &peer_bc);
                anyhow::ensure!(
                    hex.len() % 2 == 0 && hex.is_ascii(),
                    "rawcs body hex must be an even number of hex digits"
                );
                let body: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
                    .collect::<Result<_, _>>()?;
                game.send_cs(opcode, &body).await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "partyinvite" => {
                let target = parts.first().context("name")?;
                let (op, body) = social::invite_to_team(0, true, target);
                game.send_cs(op, &body).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "partyaccept" => {
                let SocialEvent::TeamAsk {
                    team_id,
                    sender_id,
                    is_party,
                    ..
                } = await_event(game, &mut consumed, "TeamAsk", 60).await?
                else {
                    bail!("not a team invitation");
                };
                let (op, body) =
                    social::reply_to_join_team(team_id, is_party, sender_id, false, &cfg.character);
                game.send_cs(op, &body).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "gcreate" => {
                let (op, body) = social::create_expedition(rest.trim());
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(4))
                    .await?;
            }
            "ginvite" => {
                let target = peer(game, parts.first().copied())?;
                let (op, body) = social::invite_to_expedition(&target.name);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "gaccept" => {
                let SocialEvent::GuildInvite { faction_id, .. } =
                    await_event(game, &mut consumed, "GuildInvite", 60).await?
                else {
                    bail!("not a guild invitation");
                };
                let (op, body) = social::reply_expedition_invitation(faction_id, true);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "greply" => {
                // Answer a guild invitation that was never received (id is the faction id).
                let id: u32 = num(&parts, 0)?;
                let join = num::<u8>(&parts, 1)? != 0;
                let (op, body) = social::reply_expedition_invitation(id, join);
                game.send_cs(op, &body).await?;
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
                show(&got);
            }
            other => bail!("unknown script command '{other}' at line {}", n + 1),
        }
    }
    r.end(s, format!("{} done", input.label));

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await.ok();
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_quoted;

    #[test]
    fn quoted_arguments_keep_spaces_and_escapes() {
        assert_eq!(
            split_quoted(" \"SELECT COUNT(*) FROM t WHERE name = 'a b'\" 1 ").unwrap(),
            vec!["SELECT COUNT(*) FROM t WHERE name = 'a b'", "1"]
        );
        assert_eq!(
            split_quoted(r#""a \"b\"" x"#).unwrap(),
            vec![r#"a "b""#, "x"]
        );
        assert_eq!(
            split_quoted(r#""object \d+ ignored""#).unwrap(),
            vec![r"object \d+ ignored"]
        );
        assert!(split_quoted("\"unterminated").is_err());
    }
}
