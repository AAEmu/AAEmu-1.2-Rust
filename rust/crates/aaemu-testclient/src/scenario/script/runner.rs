use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{cs, sc};
use anyhow::{bail, ensure, Context};

use super::super::questitem::{gm_command, stack_id, stack_location};
use super::super::{pump, skill_response_result};
use super::expectations::{
    await_event, expect_buff, expect_buff_removed, expect_chat, expect_db, expect_hp_drop,
    expect_log, expect_no_packet, expect_packet, expect_trade_canceled,
};
use super::packet_views::{
    auction_bid_body, auction_post_body, complete_quest_body, decode_auction_bid,
    decode_auction_lot, decode_auction_message, decode_count_unread, decode_craft_failed,
    decode_craft_unlock, decode_mail_body, decode_mail_header, execute_craft_body,
    parse_craft_args, read_mail_body, show, show_quiet, unit_attached,
};
use super::{dist, nearest, nearest_slave, num, peer, split_quoted, Runner};
use crate::game::{GameClient, UnitInfo};
use crate::social::{self, SocialEvent};

/// How close `lootexpect took` walks to a corpse first (retail enables looting at 3 m or less).
const LOOT_APPROACH_RANGE: f32 = 2.0;

/// `/move` to `(x, y, z)`, acknowledge it and wait for the area around the new position.
async fn teleport_to(
    game: &mut GameClient,
    clock: &mut u32,
    x: f32,
    y: f32,
    z: f32,
    drain: bool,
) -> anyhow::Result<()> {
    game.state.last_teleport = None;
    game.chat(0, &format!("/move {x} {y} {z}")).await?;
    game.wait_for("SCTeleportUnitPacket", |p| {
        p.is_sc(sc::SCTeleportUnitPacket)
    })
    .await?;
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    *clock += 1500;
    game.move_to(at, *clock).await?;
    if !drain {
        println!("    moved to {at:?}");
        return Ok(());
    }
    let got = game
        .drain(Duration::from_millis(1500), Duration::from_secs(8))
        .await?;
    println!(
        "    teleported to {at:?}; {} units, {} doodads in sight",
        game.state.units.len(),
        game.state.doodads.len()
    );
    show(&got);
    Ok(())
}

pub(super) async fn run_lines(
    game: &mut GameClient,
    cfg: &super::BotConfig,
    r: &mut Runner<'_>,
    input: &super::ScriptInput<'_>,
    current_line: &mut String,
) -> anyhow::Result<()> {
    let text = input.text;
    let s = r.begin("game.script");
    let mut items: HashMap<u32, u64> = HashMap::new();
    let mut item_slots: HashMap<u32, (u8, u8)> = HashMap::new();
    let mut music = super::music::Music::default();
    let mut clock = 40_000u32;
    let mut last_npc = 0u32;
    let mut last_death_seq = 0u64;
    let mut consumed: HashSet<usize> = HashSet::new();
    // `expectseen`: the `game.state.events` index the next search starts at.
    let mut errors_from = 0usize;
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
            "gmraw" => {
                game.chat(0, &format!("/{rest}")).await?;
            }
            "tp" => {
                let (x, y, z): (f32, f32, f32) =
                    (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                teleport_to(game, &mut clock, x, y, z, true).await?;
            }
            "moverel" => {
                let (dx, dy, dz): (f32, f32, f32) =
                    (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let (x, y, z) = game.state.self_pos;
                // No drain: the caller asserts the buff enter / leave packets that follow.
                teleport_to(game, &mut clock, x + dx, y + dy, z + dz, false).await?;
            }
            "walkrel" => {
                let (dx, dy, dz): (f32, f32, f32) =
                    (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let (x, y, z) = game.state.self_pos;
                clock += 1500;
                // A plain CSMoveUnit, no GM `/move`: e.g. to leave a trade's range.
                game.move_to((x + dx, y + dy, z + dz), clock).await?;
                println!(
                    "    walked to ({:.1}, {:.1}, {:.1})",
                    x + dx,
                    y + dy,
                    z + dz
                );
            }
            "gopeer" => {
                let p = peer(game, parts.first().copied())?;
                teleport_to(game, &mut clock, p.pos.0, p.pos.1, p.pos.2, false).await?;
            }
            "pump" => show(&pump(game, num(&parts, 0)?).await?),
            "targetself" => {
                game.change_target(game.state.self_obj).await?;
                println!("    target self {}", game.state.self_obj);
            }
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
                    let body = complete_quest_body(quest, obj, 0, selected);
                    game.send_cs(aaemu_game::opcodes::cs::CSCompleteQuestContextPacket, &body)
                        .await?;
                }
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "cqd" => {
                let quest: u32 = num(&parts, 0)?;
                let doodad: u32 = num(&parts, 1)?;
                let selected: i32 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
                let body = complete_quest_body(quest, 0, doodad, selected);
                game.send_cs(aaemu_game::opcodes::cs::CSCompleteQuestContextPacket, &body)
                    .await?;
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
                // Optional 3rd argument: cast from that many metres away (on the side the bot
                // came from) instead of 1 m next to the NPC, like `icast`'s distance argument.
                match parts.get(2).and_then(|d| d.parse::<f32>().ok()) {
                    Some(d) => {
                        let me = game.state.self_pos;
                        let (dx, dy) = (me.0 - npc.pos.0, me.1 - npc.pos.1);
                        let len = dx.hypot(dy).max(0.001);
                        let at = (
                            npc.pos.0 + dx / len * d,
                            npc.pos.1 + dy / len * d,
                            npc.pos.2,
                        );
                        // Face the NPC: a skill's damage effects can require the target in
                        // front (unchanged for the default 1 m form).
                        game.move_to_facing(at, npc.pos, clock).await?;
                    }
                    None => {
                        game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                            .await?
                    }
                }
                game.change_target(npc.obj_id).await?;
                game.start_skill(skill, npc.obj_id).await?;
                last_npc = npc.obj_id;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "castraw" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.change_target(npc.obj_id).await?;
                game.start_skill(skill, npc.obj_id).await?;
                last_npc = npc.obj_id;
                // No drain: delayed effect packets are the caller's assertions.
            }
            "slave" => {
                let tpl: u32 = num(&parts, 0)?;
                let s = nearest_slave(game, tpl)
                    .with_context(|| format!("slave {tpl} not in sight"))?;
                println!(
                    "    slave obj {} tl {} tpl {} at {:?}",
                    s.obj_id, s.tl_id, s.id, s.pos
                );
            }
            "interact" => {
                let tpl: u32 = num(&parts, 0)?;
                let s = nearest_slave(game, tpl)
                    .with_context(|| format!("slave {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((s.pos.0 + 1.0, s.pos.1, s.pos.2), clock)
                    .await?;
                game.change_target(s.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                game.start_interaction(s.obj_id).await?;
                let list = game
                    .wait_for("SCNpcInteractionSkillListPacket", |p| {
                        p.is_sc(sc::SCNpcInteractionSkillListPacket)
                            && super::interaction_skill_list(&p.body)
                                .is_ok_and(|(npc, _)| npc == s.obj_id)
                    })
                    .await?;
                let (_, skills) = super::interaction_skill_list(&list.body)?;
                println!(
                    "    slave obj {} tl {} offers {skills:?}",
                    s.obj_id, s.tl_id
                );
                if let Some(want) = parts.get(1) {
                    let want: u32 = want.parse()?;
                    anyhow::ensure!(
                        skills.contains(&want),
                        "slave {} offers {skills:?}, not {want}",
                        s.obj_id
                    );
                }
                last_npc = s.obj_id;
            }
            "bind" => {
                let tpl: u32 = num(&parts, 0)?;
                let s = nearest_slave(game, tpl)
                    .with_context(|| format!("slave {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((s.pos.0 + 1.0, s.pos.1, s.pos.2), clock)
                    .await?;
                let mut body = aaemu_commons::PacketStream::new();
                body.write_u16(s.tl_id);
                game.send_cs(cs::CSBindSlavePacket, body.as_bytes()).await?;
                if let Some(point) = parts.get(1) {
                    let point: u8 = point.parse()?;
                    let me = game.state.self_obj;
                    let p = game
                        .wait_for("SCUnitAttachedPacket", |p| {
                            p.is_sc(sc::SCUnitAttachedPacket)
                                && unit_attached(&p.body)
                                    .is_ok_and(|(child, pt, _, _)| child == me && pt == point)
                        })
                        .await?;
                    let (child, pt, parent, reason) = unit_attached(&p.body)?;
                    println!(
                        "    SCUnitAttached child {child} point {pt} parent {parent} reason {reason}"
                    );
                } else {
                    let got = game
                        .drain(Duration::from_millis(1500), Duration::from_secs(8))
                        .await?;
                    show(&got);
                }
            }
            "scast" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let s = nearest_slave(game, tpl)
                    .with_context(|| format!("slave {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((s.pos.0 + 1.0, s.pos.1, s.pos.2), clock)
                    .await?;
                game.change_target(s.obj_id).await?;
                game.start_skill(skill, s.obj_id).await?;
                if let Some(point) = parts.get(2) {
                    let point: u8 = point.parse()?;
                    let me = game.state.self_obj;
                    let p = game
                        .wait_for("SCUnitAttachedPacket", |p| {
                            p.is_sc(sc::SCUnitAttachedPacket)
                                && unit_attached(&p.body)
                                    .is_ok_and(|(child, pt, _, _)| child == me && pt == point)
                        })
                        .await?;
                    let (child, pt, parent, reason) = unit_attached(&p.body)?;
                    println!(
                        "    SCUnitAttached child {child} point {pt} parent {parent} reason {reason}"
                    );
                } else {
                    let got = game
                        .drain(Duration::from_millis(1500), Duration::from_secs(8))
                        .await?;
                    show(&got);
                }
            }
            "scastfail" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let code: i16 = num(&parts, 2)?;
                let s = nearest_slave(game, tpl)
                    .with_context(|| format!("slave {tpl} not in sight"))?;
                game.start_skill(skill, s.obj_id).await?;
                game.wait_for("SCErrorMsgPacket", |p| {
                    p.is_sc(sc::SCErrorMsgPacket) && p.stream().read_i16().ok() == Some(code)
                })
                .await?;
                println!("    SCErrorMsg {code}");
            }
            "expectattach" => {
                let point: u8 = num(&parts, 0)?;
                let me = game.state.self_obj;
                let p = game
                    .wait_for("SCUnitAttachedPacket", |p| {
                        p.is_sc(sc::SCUnitAttachedPacket)
                            && unit_attached(&p.body)
                                .is_ok_and(|(child, pt, _, _)| child == me && pt == point)
                    })
                    .await?;
                let (child, pt, parent, reason) = unit_attached(&p.body)?;
                println!(
                    "    SCUnitAttached child {child} point {pt} parent {parent} reason {reason}"
                );
            }
            "mcast" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let mount_row: u32 = num(&parts, 2)?;
                let s = nearest_slave(game, tpl)
                    .with_context(|| format!("slave {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((s.pos.0 + 1.0, s.pos.1, s.pos.2), clock)
                    .await?;
                game.change_target(s.obj_id).await?;
                game.start_mount_skill(skill, s.obj_id, mount_row).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "expecterror" => {
                let code: i16 = num(&parts, 0)?;
                game.wait_for("SCErrorMsgPacket", |p| {
                    p.is_sc(sc::SCErrorMsgPacket) && p.stream().read_i16().ok() == Some(code)
                })
                .await?;
                println!("    SCErrorMsg {code}");
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
                // An attached doodad's position is parent-local; the bot is already on the
                // vehicle, so moving to it would walk away from the ship.
                if d.parent_obj_id == 0 {
                    clock += 1500;
                    game.move_to((d.pos.0 + 1.0, d.pos.1, d.pos.2), clock)
                        .await?;
                }
                game.start_doodad_skill(skill, d.obj_id).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "pcast" => {
                let skill = num(&parts, 0)?;
                let position = (num(&parts, 1)?, num(&parts, 2)?, num(&parts, 3)?);
                game.start_position_skill(skill, position).await?;
                show(&pump(game, 2).await?);
            }
            c if c.starts_with('m') && super::music::is_command(c) => {
                let args = split_quoted(rest)?;
                super::music::command(game, &mut music, c, &args).await?;
            }
            "buycoin" | "buycoinraw" => {
                let tpl: u32 = num(&parts, 0)?;
                let func: u32 = num(&parts, 1)?;
                let me = game.state.self_pos;
                let d = game
                    .state
                    .doodads
                    .values()
                    .filter(|d| d.template_id == tpl)
                    .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
                    .cloned()
                    .with_context(|| format!("doodad {tpl} not in sight"))?;
                if d.parent_obj_id == 0 {
                    clock += 1500;
                    game.move_to((d.pos.0 + 1.0, d.pos.1, d.pos.2), clock)
                        .await?;
                }
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(d.obj_id).write_u32(func);
                game.send_cs(aaemu_game::opcodes::cs::CSBuyCoinItemPacket, b.as_bytes())
                    .await?;
                if cmd == "buycoinraw" {
                    continue;
                }
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "goods" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1200;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                let mut body = aaemu_commons::PacketStream::new();
                body.write_bc(npc.obj_id);
                game.send_cs(cs::CSListSpecialtyGoodsPacket, body.as_bytes())
                    .await?;
                let packet = game.wait_sc(sc::SCSpecialtyGoodsPacket).await?;
                let mut s = packet.stream();
                let count = s.read_i32()?;
                println!("    SCSpecialtyGoods count {count}");
                for i in 0..count.max(0) {
                    let item = s.read_u32()?;
                    let refund = s.read_i32()?;
                    let currency = s.read_u8()?;
                    let unknown = s.read_u8()?;
                    println!(
                        "      good {i}: item {item} refund {refund} currency {currency} unknown {unknown}"
                    );
                }
            }
            "sellpack" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1200;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                let mut body = aaemu_commons::PacketStream::new();
                body.write_bc(npc.obj_id);
                game.send_cs(cs::CSSellBackpackGoodsPacket, body.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(10))
                    .await?;
                show(&got);
                let labor = got
                    .iter()
                    .find(|p| p.is_sc(sc::SCCharacterLaborPowerChangedPacket))
                    .context("sellpack: no SCCharacterLaborPowerChanged")?;
                let mut s = labor.stream();
                let amount = s.read_i32()?;
                let action = s.read_i32()?;
                let point = s.read_i32()?;
                let step = s.read_u8()?;
                println!("    labor change {amount} action {action} point {point} step {step}");
                ensure!(
                    got.iter().any(|p| p.is_sc(sc::SCItemTaskSuccessPacket)),
                    "sellpack: no SCItemTaskSuccess"
                );
            }
            "trader" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1200;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.start_interaction(npc.obj_id).await?;
                // The captured first talk has the target-changed flag set; the tail repeat is 00.
                game.interact_npc(npc.obj_id, true).await?;
                game.start_skill(25205, npc.obj_id).await?;
                let mut body = aaemu_commons::PacketStream::new();
                body.write_u32(34159); // the pack template the client checks
                game.send_cs(cs::CSSpecialtyRatioPacket, body.as_bytes())
                    .await?;
                game.interact_npc(npc.obj_id, false).await?;
                let mut body = aaemu_commons::PacketStream::new();
                body.write_bc(npc.obj_id);
                game.send_cs(cs::CSSellBackpackGoodsPacket, body.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
                ensure!(
                    got.iter().any(|p| p.is_sc(sc::SCSpecialtyRatioPacket)),
                    "trader: no SCSpecialtyRatio answer"
                );
                ensure!(
                    !got.iter().any(|p| p.is_sc(sc::SCSpecialtyRecordsPacket)),
                    "trader: the discarded SCSpecialtyRecords must not be sent"
                );
            }
            "priceboard" => {
                let from: u16 = num(&parts, 0)?;
                let to: u16 = num(&parts, 1)?;
                let mut body = aaemu_commons::PacketStream::new();
                body.write_u16(from).write_u16(to);
                game.send_cs(cs::CSRequestSpecialtyCurrentPacket, body.as_bytes())
                    .await?;
                let packet = game.wait_sc(sc::SCSpecialtyCurrentPacket).await?;
                let mut s = packet.stream();
                let count = s.read_i32()?;
                let got_from = s.read_u16()?;
                let got_to = s.read_u16()?;
                ensure!(
                    (0..=0x80).contains(&count),
                    "priceboard: invalid count {count}"
                );
                println!("    SCSpecialtyCurrent from {got_from} to {got_to} count {count}");
                for i in 0..count.min(3) {
                    let item = s.read_u32()?;
                    let ratio = s.read_i32()?;
                    println!("      {i}: item {item} ratio {ratio}");
                }
            }
            "cexec" => {
                let (craft, doodad, count) = parse_craft_args(&parts)?;
                game.send_cs(
                    cs::CSExecuteCraft,
                    &execute_craft_body(craft, doodad, count),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(10))
                    .await?;
                for p in &got {
                    if p.is_sc(sc::SCCraftItemUnlockPacket) {
                        let mut s = p.stream();
                        let (slot_type, index) = decode_craft_unlock(&mut s)?;
                        ensure!(
                            s.remaining() == 0,
                            "SCCraftItemUnlockPacket has trailing bytes"
                        );
                        println!("    SCCraftItemUnlock slot type {slot_type} index {index}");
                    } else if p.is_sc(sc::SCCraftFailedPacket) {
                        let mut s = p.stream();
                        let (id, count, items) = decode_craft_failed(&mut s)?;
                        ensure!(s.remaining() == 0, "SCCraftFailedPacket has trailing bytes");
                        println!("    SCCraftFailed id {id} count {count} items {items:?}");
                    }
                }
                show(&got);
            }
            "maillist" => {
                game.send_cs(cs::CSListMailPacket, &[]).await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                let mut headers = 0usize;
                let mut list_end = false;
                for p in &got {
                    if p.is_sc(sc::SCMailListPacket) {
                        let mut s = p.stream();
                        let is_sent = s.read_bool()?;
                        let total = s.read_i32()?;
                        let h = decode_mail_header(&mut s)?;
                        ensure!(s.remaining() == 0, "SCMailListPacket has trailing bytes");
                        println!(
                            "    SCMailList isSent {is_sent} total {total} mail {} type {} \
                             status {} title {:?} sender {:?} receiver {:?} attachments {} \
                             open {} returned {} extra {}",
                            h.mail_id,
                            h.mail_type,
                            h.status,
                            h.title,
                            h.sender_name,
                            h.receiver_name,
                            h.attachments,
                            h.open_date,
                            h.returned,
                            h.extra
                        );
                        headers += 1;
                    } else if p.is_sc(sc::SCMailListEndPacket) {
                        let mut s = p.stream();
                        let total_headers = s.read_i32()?;
                        let total_bodies = s.read_i32()?;
                        println!("    SCMailListEnd headers {total_headers} bodies {total_bodies}");
                        list_end = true;
                    }
                }
                show(&got);
                ensure!(list_end, "maillist: no SCMailListEndPacket");
                println!("    {headers} mail header(s)");
            }
            "mailread" => {
                let is_sent = num::<u8>(&parts, 0)? != 0;
                let mail_id: i64 = num(&parts, 1)?;
                game.send_cs(cs::CSReadMailPacket, &read_mail_body(is_sent, mail_id))
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                let mut body_seen = false;
                for p in &got {
                    if p.is_sc(sc::SCMailBodyPacket) {
                        let mut s = p.stream();
                        let is_prepare = s.read_bool()?;
                        let body_is_sent = s.read_bool()?;
                        let b = decode_mail_body(&mut s)?;
                        let open_modified = s.read_bool()?;
                        let (sent, received, mia, commercial) = decode_count_unread(&mut s)?;
                        ensure!(s.remaining() == 0, "SCMailBodyPacket has trailing bytes");
                        println!(
                            "    SCMailBody prepare {is_prepare} sent {body_is_sent} mail {} \
                             type {} title {:?} text {:?} money [{}, {}, {}] dates [{}, {}, {}] \
                             openDateModified {open_modified} unread [{sent}, {received}, {mia}, \
                             {commercial}]",
                            b.mail_id,
                            b.mail_type,
                            b.title,
                            b.text,
                            b.copper_coins,
                            b.billing_amount,
                            b.money_amount2,
                            b.send_date,
                            b.recv_date,
                            b.open_date
                        );
                        for (i, (tpl, id, grade, count)) in b.items.iter().enumerate() {
                            if *tpl != 0 {
                                println!(
                                    "      attachment {i}: tpl {tpl} id {id} grade {grade} \
                                     count {count}"
                                );
                            }
                        }
                        body_seen = true;
                    } else if p.is_sc(sc::SCMailStatusUpdatedPacket) {
                        let mut s = p.stream();
                        let sent = s.read_bool()?;
                        let id = s.read_i64()?;
                        let status = s.read_u8()?;
                        println!("    SCMailStatusUpdated isSent {sent} mail {id} status {status}");
                    }
                }
                show(&got);
                ensure!(
                    body_seen,
                    "mailread: no SCMailBodyPacket for mail {mail_id}"
                );
            }
            "mailtakefirst" => {
                let want_type: Option<u8> = parts.first().and_then(|x| x.parse().ok());
                game.send_cs(cs::CSListMailPacket, &[]).await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                let mut chosen = None;
                for p in &got {
                    if p.is_sc(sc::SCMailListPacket) {
                        let mut s = p.stream();
                        let is_sent = s.read_bool()?;
                        let _total = s.read_i32()?;
                        let h = decode_mail_header(&mut s)?;
                        if !is_sent
                            && h.attachments > 0
                            && want_type.is_none_or(|t| h.mail_type == t)
                        {
                            chosen = Some((h.mail_id, h.mail_type, h.attachments));
                            break;
                        }
                    }
                }
                let Some((mail_id, mail_type, attachments)) = chosen else {
                    bail!("mailtakefirst: no received mail with attachments");
                };
                println!("    taking mail {mail_id} type {mail_type} attachments {attachments}");
                let mut body = aaemu_commons::PacketStream::new();
                body.write_i64(mail_id);
                game.send_cs(cs::CSTakeAttachmentSequentially, body.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
                let mut taken = None;
                for p in &got {
                    if p.is_sc(sc::SCAttachmentTakenPacket) {
                        let mut s = p.stream();
                        let _id = s.read_i64()?;
                        let _money = s.read_bool()?;
                        let _aa = s.read_bool()?;
                        let _sequential = s.read_bool()?;
                        let _count = s.read_u8()?;
                        let mut items = 0;
                        for _ in 0..10 {
                            let id = s.read_u64()?;
                            let _slot_type = s.read_u8()?;
                            let _slot = s.read_u8()?;
                            if id != 0 {
                                items += 1;
                            }
                        }
                        ensure!(items > 0, "mailtakefirst: SCAttachmentTaken lists no item");
                        taken = Some(items);
                    }
                }
                ensure!(taken.is_some(), "mailtakefirst: no SCAttachmentTakenPacket");
                println!("    took {taken:?} attachment(s) from mail {mail_id}");
            }
            "buyics" => {
                let shop_id: u32 = num(&parts, 0)?;
                let detail: u8 = num(&parts, 1)?;
                let main_tab: u8 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
                let sub_tab: u8 = parts.get(3).and_then(|x| x.parse().ok()).unwrap_or(0);
                let receiver = parts.get(4).copied().unwrap_or("");
                let mut body = aaemu_commons::PacketStream::new();
                body.write_u8(1)
                    .write_u32(shop_id)
                    .write_u8(main_tab)
                    .write_u8(sub_tab)
                    .write_u8(detail)
                    .write_str(receiver);
                game.send_cs(cs::CSICSBuyGoodPacket, body.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(10))
                    .await?;
                show(&got);
                let mut bought = None;
                let mut charged = false;
                let mut commercial = None;
                for p in &got {
                    if p.is_sc(sc::SCICSBuyResultPacket) {
                        let mut s = p.stream();
                        bought = Some(s.read_bool()?);
                    } else if p.is_sc(sc::SCGotMailPacket) {
                        let mut s = p.stream();
                        let h = decode_mail_header(&mut s)?;
                        let (sent, received, mia, commercial_received) =
                            decode_count_unread(&mut s)?;
                        println!(
                            "    SCGotMail type {} unread [{sent}, {received}, {mia}, \
                             {commercial_received}]",
                            h.mail_type
                        );
                        if h.mail_type == 9 {
                            charged = true;
                        }
                        commercial = Some(commercial_received);
                    }
                }
                ensure!(bought == Some(true), "buyics: no successful SCICSBuyResult");
                ensure!(charged, "buyics: no CHARGED (type 9) mail");
                ensure!(
                    commercial == Some(1),
                    "buyics: SCCountUnreadMail's commercial counter is not 1"
                );
            }
            "giftics" => {
                let shop_id: u32 = num(&parts, 0)?;
                let detail: u8 = num(&parts, 1)?;
                let receiver = *parts
                    .get(2)
                    .with_context(|| "giftics: missing receiver name")?;
                let main_tab: u8 = parts.get(3).and_then(|x| x.parse().ok()).unwrap_or(0);
                let sub_tab: u8 = parts.get(4).and_then(|x| x.parse().ok()).unwrap_or(0);
                let mut body = aaemu_commons::PacketStream::new();
                body.write_u8(1)
                    .write_u32(shop_id)
                    .write_u8(main_tab)
                    .write_u8(sub_tab)
                    .write_u8(detail)
                    .write_str(receiver);
                game.send_cs(cs::CSICSBuyGoodPacket, body.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(10))
                    .await?;
                show(&got);
                let mut bought = None;
                let mut error = None;
                for p in &got {
                    if p.is_sc(sc::SCICSBuyResultPacket) {
                        bought = Some(p.stream().read_bool()?);
                    } else if p.is_sc(sc::SCErrorMsgPacket) {
                        error = Some(p.stream().read_i16()?);
                    }
                }
                ensure!(
                    bought == Some(true),
                    "giftics: no successful SCICSBuyResult (SCErrorMsg {error:?})"
                );
                ensure!(error.is_none(), "giftics: SCErrorMsg {error:?}");
            }
            "unreadmail" => {
                let want = (
                    num::<i32>(&parts, 0)?,
                    num::<i32>(&parts, 1)?,
                    num::<i32>(&parts, 2)?,
                    num::<i32>(&parts, 3)?,
                );
                let got = game
                    .state
                    .count_unread_mail
                    .context("unreadmail: no SCCountUnreadMailPacket seen")?;
                ensure!(got == want, "unreadmail: got {got:?}, want {want:?}");
                println!("    SCCountUnreadMail {got:?}");
            }
            "aucpost" => {
                let npc_tpl: u32 = num(&parts, 0)?;
                let item_tpl: u32 = num(&parts, 1)?;
                let start: i32 = num(&parts, 2)?;
                let buyout: i32 = num(&parts, 3)?;
                let duration: u8 = num(&parts, 4)?;
                let item_id = *items
                    .get(&item_tpl)
                    .with_context(|| format!("item {item_tpl} not added with `item`"))?;
                let npc = nearest(game, npc_tpl)
                    .with_context(|| format!("NPC {npc_tpl} not in sight"))?;
                clock += 1200;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.start_interaction(npc.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                game.send_cs(
                    cs::CSAuctionPostPacket,
                    &auction_post_body(npc.obj_id, 0, item_id, start, buyout, duration),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                let mut posted = false;
                for p in &got {
                    if p.is_sc(sc::SCAuctionPostedPacket) {
                        let mut s = p.stream();
                        let lot = decode_auction_lot(&mut s)?;
                        ensure!(
                            s.remaining() == 0,
                            "SCAuctionPostedPacket has trailing bytes"
                        );
                        println!(
                            "    SCAuctionPosted lot {} duration {} item tpl {} id {} \
                             client {} '{}' start {} buyout {}",
                            lot.id,
                            lot.duration,
                            lot.item_template,
                            lot.item_id,
                            lot.client_id,
                            lot.client_name,
                            lot.start_money,
                            lot.direct_money
                        );
                        posted = true;
                    }
                }
                show(&got);
                ensure!(
                    posted,
                    "aucpost: no SCAuctionPostedPacket (auctioneer NPC in range?)"
                );
            }
            "aucbid" => {
                let npc_tpl: u32 = num(&parts, 0)?;
                let lot_id: u64 = num(&parts, 1)?;
                let money: i32 = num(&parts, 2)?;
                let npc = nearest(game, npc_tpl)
                    .with_context(|| format!("NPC {npc_tpl} not in sight"))?;
                clock += 1200;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.start_interaction(npc.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                let body = auction_bid_body(
                    npc.obj_id,
                    0,
                    lot_id,
                    game.state.selected_char,
                    &cfg.character,
                    money,
                );
                game.send_cs(cs::CSBidAuctionPacket, &body).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                let mut bid_seen = false;
                for p in &got {
                    if p.is_sc(sc::SCAuctionBidPacket) {
                        let mut s = p.stream();
                        let bid = decode_auction_bid(&mut s)?;
                        let is_buyout = s.read_bool()?;
                        let item_id = s.read_u32()?;
                        ensure!(s.remaining() == 0, "SCAuctionBidPacket has trailing bytes");
                        println!(
                            "    SCAuctionBid lot {} bidder {} '{}' money {} buyout {is_buyout} \
                             item {item_id}",
                            bid.lot_id, bid.bidder_id, bid.bidder_name, bid.money
                        );
                        bid_seen = true;
                    } else if p.is_sc(sc::SCAuctionMessagePacket) {
                        let (msg, item, amount) = decode_auction_message(&mut p.stream())?;
                        println!("    SCAuctionMessage type {msg} item {item} money {amount}");
                    }
                }
                show(&got);
                ensure!(
                    bid_seen,
                    "aucbid: no SCAuctionBidPacket (auctioneer NPC in range, lot valid?)"
                );
            }
            "item" => {
                let tpl: u32 = num(&parts, 0)?;
                let count: u32 = num(&parts, 1)?;
                let reply = gm_command(game, &format!("item add self {tpl} {count}")).await?;
                if let Some(id) = stack_id(&reply, tpl) {
                    items.insert(tpl, id);
                    println!("    item {tpl} stack id {id}");
                }
                if let Some((_, slot_type, slot)) = stack_location(&reply, tpl) {
                    item_slots.insert(tpl, (slot_type, slot));
                    println!("    item {tpl} at slot type {slot_type} slot {slot}");
                }
                show(&reply);
            }
            "moveitem" => {
                let id: u64 = num(&parts, 0)?;
                let from_type: u8 = num(&parts, 1)?;
                let from_slot: u8 = num(&parts, 2)?;
                let to_type: u8 = num(&parts, 3)?;
                let to_slot: u8 = num(&parts, 4)?;
                game.swap_slots((id, from_type, from_slot), (0, to_type, to_slot))
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "equip" => {
                let tpl: u32 = num(&parts, 0)?;
                let slot: u8 = num(&parts, 1)?;
                let id = *items
                    .get(&tpl)
                    .with_context(|| format!("item {tpl} not added with `item`"))?;
                let (slot_type, from_slot) = *item_slots
                    .get(&tpl)
                    .with_context(|| format!("item {tpl} has no known slot"))?;
                game.swap_slots((id, slot_type, from_slot), (0, 1, slot))
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
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
            "icastitem" => {
                let skill: u32 = num(&parts, 0)?;
                let scroll_tpl: u32 = num(&parts, 1)?;
                let target_tpl: u32 = num(&parts, 2)?;
                let expect = parts.get(3).copied().unwrap_or("ok");
                let scroll = *items
                    .get(&scroll_tpl)
                    .with_context(|| format!("item {scroll_tpl} not added with `item`"))?;
                let target = *items
                    .get(&target_tpl)
                    .with_context(|| format!("item {target_tpl} not added with `item`"))?;
                game.start_item_skill_on_item(skill, scroll, scroll_tpl, target)
                    .await?;
                let got = game
                    .drain(Duration::from_millis(2000), Duration::from_secs(10))
                    .await?;
                show(&got);
                let errors: Vec<i16> = got
                    .iter()
                    .filter(|p| p.is_sc(sc::SCErrorMsgPacket))
                    .filter_map(|p| p.stream().read_i16().ok())
                    .collect();
                let regraded = got.iter().any(|p| p.is_sc(sc::SCGradeEnchantResultPacket));
                println!(
                    "    item cast {skill} from {scroll_tpl} on {target_tpl}: errors {errors:?}, \
                     regrade result {regraded}"
                );
                match expect {
                    "ok" => ensure!(
                        regraded && errors.is_empty(),
                        "item cast {skill} on {target_tpl}: expected SCGradeEnchantResult and no \
                         error, got errors {errors:?}"
                    ),
                    "error" => {
                        let code: i16 = num(&parts, 4)?;
                        ensure!(
                            errors.contains(&code) && !regraded,
                            "item cast {skill} on {target_tpl}: expected error {code} and no \
                             regrade result, got errors {errors:?}"
                        );
                    }
                    other => bail!("unknown icastitem expectation '{other}' (error <code>|ok)"),
                }
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
            "expectchat" => {
                let args = split_quoted(rest)?;
                let chat_type: i16 = args
                    .first()
                    .context("expectchat <chat type> \"<text>\" [secs]")?
                    .parse()?;
                let text = args
                    .get(1)
                    .context("expectchat <chat type> \"<text>\" [secs]")?;
                let secs: u64 = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(5);
                expect_chat(game, chat_type, text, secs).await?;
            }
            "expecttradecancel" => {
                let reason: i32 =
                    num(&parts, 0).context("expecttradecancel <reason> <caused 0|1> [secs]")?;
                let caused = match parts.get(1).copied() {
                    Some("1") | Some("true") => true,
                    Some("0") | Some("false") => false,
                    other => bail!("expecttradecancel caused must be 0 or 1, not {other:?}"),
                };
                let secs: u64 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(5);
                expect_trade_canceled(game, reason, caused, secs).await?;
            }
            "expectnopkt" => {
                let name = parts.first().context("expectnopkt <PacketName> [secs]")?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(5);
                expect_no_packet(game, name, secs).await?;
            }
            "expectbuff" => {
                let buff: u32 = num(&parts, 0)?;
                let secs: u64 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(10);
                expect_buff(game, buff, parts.get(1).copied(), secs).await?;
            }
            "expectbuffremoved" => {
                let buff: u32 = num(&parts, 0)?;
                let secs: u64 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(10);
                expect_buff_removed(game, buff, parts.get(1).copied(), secs).await?;
            }
            "expecthpdrop" | "expectnohpdrop" => {
                let spec = parts.first().copied();
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(10);
                expect_hp_drop(game, spec, secs, cmd == "expecthpdrop").await?;
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
            "deathwait" => {
                let wait_ms: i32 =
                    num(&parts, 0).context("deathwait <wait_ms> <lost|*> <dur|*>")?;
                let lost = parts.get(1).copied().unwrap_or("*");
                let durability = parts.get(2).copied().unwrap_or("*");
                let secs: u64 = parts.get(3).and_then(|x| x.parse().ok()).unwrap_or(30);
                // A `gm` command drains its own packets, so a death that happened during the
                // previous command is taken from the observed state; otherwise wait for the
                // next one.
                let reported = last_death_seq;
                let mut death = if game.state.self_death_seq > reported {
                    game.state.self_death
                } else {
                    None
                };
                let deadline = Instant::now() + Duration::from_secs(secs);
                while death.is_none() && Instant::now() < deadline {
                    let left = deadline.saturating_duration_since(Instant::now());
                    game.recv_within(left.min(Duration::from_millis(500)))
                        .await?;
                    if game.state.self_death_seq > reported {
                        death = game.state.self_death;
                    }
                }
                last_death_seq = game.state.self_death_seq;
                let Some((reason, got_wait, got_lost, got_dur)) = death else {
                    bail!("FAIL deathwait: no SCUnitDeathPacket for self within {secs}s");
                };
                println!(
                    "    deathwait: kill reason {reason}, wait {got_wait} ms, lost exp {got_lost}, \
                     durability {got_dur}"
                );
                anyhow::ensure!(
                    got_wait == wait_ms,
                    "FAIL deathwait: resurrect_wait_time {got_wait}, expected {wait_ms}"
                );
                if lost != "*" {
                    if let Some(want) = lost.strip_prefix('>') {
                        let want: i32 = want.parse().context("deathwait lost exp >N")?;
                        anyhow::ensure!(
                            got_lost > want,
                            "FAIL deathwait: lost exp {got_lost}, expected > {want}"
                        );
                    } else {
                        let want: i32 = lost.parse().context("deathwait lost exp")?;
                        anyhow::ensure!(
                            got_lost == want,
                            "FAIL deathwait: lost exp {got_lost}, expected {want}"
                        );
                    }
                }
                if durability != "*" {
                    let want: u8 = durability.parse().context("deathwait durability")?;
                    anyhow::ensure!(
                        got_dur == want,
                        "FAIL deathwait: durability {got_dur}, expected {want}"
                    );
                }
                println!("    deathwait ok");
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
            "targetpeer" => {
                let target = peer(game, parts.first().copied())?;
                game.change_target(target.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
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
            "rawcs" | "rawcswait" | "rawcsend" => {
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
                let peer_unit = if raw.contains("PEER") {
                    Some(peer(game, None).context("PEER / PEERID need another player in sight")?)
                } else {
                    None
                };
                let le32 = |id: u32| id.to_le_bytes().map(|b| format!("{b:02x}")).concat();
                let peer_bc = peer_unit.as_ref().map(|u| bc(u.obj_id)).unwrap_or_default();
                let peer_id = peer_unit.as_ref().map(|u| le32(u.id)).unwrap_or_default();
                // `GUILD` is the bot's guild id as a little-endian u32; `SELFID` / `PEERID` are
                // character ids (`CSFamilyReplyInvitation` and the other u32 character-id
                // packets), while `SELF` / `PEER` stay the 3-byte `bc` object ids.
                let mut hex = raw
                    .replace(
                        "GUILD",
                        &game
                            .state
                            .guild_id
                            .to_le_bytes()
                            .map(|b| format!("{b:02x}"))
                            .concat(),
                    )
                    .replace("PEERID", &peer_id)
                    .replace("SELFID", &le32(game.state.selected_char))
                    .replace("SELF", &bc(game.state.self_obj))
                    .replace("PEER", &peer_bc);
                // `STR(<text>)` expands to a `PacketStream` string (u16 length + UTF-8 bytes).
                while let Some(start) = hex.find("STR(") {
                    let rest = &hex[start + 4..];
                    let end = rest.find(')').context("STR( needs a closing )")?;
                    let text = &rest[..end];
                    anyhow::ensure!(
                        text.len() <= usize::from(u16::MAX),
                        "STR() text is longer than a packet string"
                    );
                    let bytes = text.as_bytes();
                    let mut enc = format!("{:02x}{:02x}", bytes.len() & 0xff, bytes.len() >> 8);
                    for byte in bytes {
                        enc.push_str(&format!("{byte:02x}"));
                    }
                    hex.replace_range(start..start + 4 + end + 1, &enc);
                }
                // `ITEM(<template>)` expands to the stack id remembered by
                // `item <template>`, little-endian, so a packet can carry the real item
                // instance id.
                while let Some(start) = hex.find("ITEM(") {
                    let rest = &hex[start + 5..];
                    let end = rest.find(')').context("ITEM( needs a closing )")?;
                    let template: u32 = rest[..end].parse()?;
                    let id = *items
                        .get(&template)
                        .with_context(|| format!("item {template} not added with `item`"))?;
                    let bytes: String = id
                        .to_le_bytes()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    hex.replace_range(start..start + 5 + end + 1, &bytes);
                }
                anyhow::ensure!(
                    hex.len() % 2 == 0 && hex.is_ascii(),
                    "rawcs body hex must be an even number of hex digits"
                );
                let body: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
                    .collect::<Result<_, _>>()?;
                game.send_cs(opcode, &body).await?;
                if cmd == "rawcswait" {
                    // The answer is asserted, not drained away (`rawcs` prints it instead).
                    let want = parts
                        .get(2)
                        .context("rawcswait needs <PacketName> after the body")?;
                    let secs: u64 = parts.get(3).and_then(|x| x.parse().ok()).unwrap_or(5);
                    expect_packet(game, want, secs).await?;
                } else if cmd == "rawcsend" {
                    // Leave the reply for a following `expecterror` / `expectpkt`.
                } else {
                    let got = game
                        .drain(Duration::from_millis(1200), Duration::from_secs(6))
                        .await?;
                    show(&got);
                }
            }
            "expectseen" => {
                let code: i16 = num(&parts, 0)?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(5);
                let deadline = Instant::now() + Duration::from_secs(secs);
                loop {
                    let found = game.state.events[errors_from.min(game.state.events.len())..]
                        .iter()
                        .position(
                            |e| matches!(e, crate::game::GameEvent::ErrorMsg(c) if *c == code),
                        );
                    if let Some(i) = found {
                        errors_from = errors_from.min(game.state.events.len()) + i + 1;
                        println!("    SCErrorMsg {code} seen");
                        break;
                    }
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        bail!("no SCErrorMsg {code} within {secs} s");
                    }
                    game.recv_within(left.min(Duration::from_millis(250)))
                        .await?;
                }
            }
            "hangup" => {
                // A hard disconnect: the socket closes without `CSLeaveWorld` and the script
                // ends here (the server runs its disconnect cleanup, not the 10 s leave).
                game.hang_up().await;
                r.end(s, "hung up");
                return Ok(());
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
            "areainvite" => {
                let team_id: u32 = parts.first().and_then(|x| x.parse().ok()).unwrap_or(0);
                let is_party = !parts.get(1).is_some_and(|x| x.eq_ignore_ascii_case("raid"));
                let (op, body) = social::invite_area_to_team(team_id, is_party);
                game.send_cs(op, &body).await?;
            }
            "expectareacool" => {
                let secs: u64 = parts.first().and_then(|x| x.parse().ok()).unwrap_or(5);
                loop {
                    match await_event(game, &mut consumed, "AreaInvited", secs).await? {
                        SocialEvent::AreaInvited { r, s: false } => {
                            anyhow::ensure!(
                                r > 0 && r <= 30_000,
                                "area cooldown reply has r={r} (want 1..=30000 ms)"
                            );
                            println!("    area invite on cooldown: {r} ms left");
                            break;
                        }
                        SocialEvent::AreaInvited { r, s: true } => {
                            println!("    skipped success area invite (r={r})");
                        }
                        _ => unreachable!(),
                    }
                }
            }
            "partyacceptarea" => {
                let SocialEvent::TeamAskArea {
                    team_id,
                    sender_id,
                    is_party,
                    ..
                } = await_event(game, &mut consumed, "TeamAskArea", 60).await?
                else {
                    bail!("not an area team invitation");
                };
                let (op, body) = social::reply_area_to_join_team(
                    team_id,
                    is_party,
                    sender_id,
                    false,
                    &cfg.character,
                );
                game.send_cs(op, &body).await?;
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

            "grename" => {
                let id = game.state.guild_id;
                if id == 0 {
                    bail!("grename: the bot has not seen its guild yet");
                }
                let (op, body) = social::rename_expedition(id, rest.trim(), true);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "gexpectrenamed" => {
                let name = rest.trim();
                ensure!(!name.is_empty(), "gexpectrenamed <name>");
                let SocialEvent::GuildRenamed {
                    id,
                    name: got,
                    by_gm,
                } = await_event(game, &mut consumed, "GuildRenamed", 60).await?
                else {
                    bail!("not a faction rename");
                };
                ensure!(
                    got == *name,
                    "expected the guild to be renamed to {name:?}, got {got:?} (id {id}, byGm {by_gm})"
                );
                println!("    guild {id} renamed to {got}");
            }
            "gexpectretry" => {
                let code: i16 = num(&parts, 0)?;
                let SocialEvent::GuildRenameRetry {
                    is_expedition,
                    error: error_message,
                    name,
                } = await_event(game, &mut consumed, "GuildRenameRetry", 60).await?
                else {
                    bail!("not a faction rename retry");
                };
                ensure!(
                    is_expedition,
                    "retry packet for a nation (xpdt false), expected the expedition dialog"
                );
                ensure!(
                    error_message == code,
                    "expected rename retry error {code}, got {error_message} (name {name:?})"
                );
                println!("    rename retry {error_message} with name {name:?}");
            }

            "ninvite" => {
                // The character name differs in case from the account name: use the peer's.
                let target = peer(game, parts.first().copied())?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_str(&target.name);
                game.send_cs(cs::CSFactionImmigrationInvitePacket, &b.into_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
                show(&got);
            }
            "naccept" => {
                // The reply echoes the invitation's (nation id, nonce) (PORTING.md, NATION-11).
                let join = parts.first().map(|v| *v != "0").unwrap_or(true);
                let invitation = game.wait_sc(sc::SCFactionImmigrateInvitePacket).await?;
                let mut s = invitation.stream();
                let nation = s.read_u32()?;
                let nonce = s.read_u32()?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u32(nation).write_u32(nonce).write_bool(join);
                game.send_cs(cs::CSFactionImmigrationInviteReplyPacket, &b.into_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
                show(&got);
            }
            "nreply" => {
                // Answer an invitation that was never received (forged / replayed reply).
                let nation: u32 = num(&parts, 0)?;
                let nonce: u32 = num(&parts, 1)?;
                let join = num::<u8>(&parts, 2)? != 0;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u32(nation).write_u32(nonce).write_bool(join);
                game.send_cs(cs::CSFactionImmigrationInviteReplyPacket, &b.into_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
                show(&got);
            }
            "nlevel" => {
                // `/level <name> <level>`: the GM command's name match is exact.
                let target = peer(game, parts.first().copied())?;
                let level: u8 = num(&parts, 1)?;
                let got = gm_command(game, &format!("level {} {level}", target.name)).await?;
                show(&got);
            }
            "norigin" => {
                game.send_cs(cs::CSFactionImmigrateToOriginPacket, &[])
                    .await?;
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
                show(&got);
            }
            "nkick" => {
                let target = peer(game, parts.first().copied())?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_str(&target.name);
                game.send_cs(cs::CSFactionKickToOriginPacket, &b.into_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
                show(&got);
            }
            "nhostile" => {
                // No drain: the caller asserts the result / relation packets.
                let faction: u32 = num(&parts, 0)?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u32(faction);
                game.send_cs(cs::CSFactionDeclareHostilePacket, &b.into_bytes())
                    .await?;
                println!("    hostile declaration sent");
            }
            "nattack" => {
                let skill: u32 = num(&parts, 0)?;
                let target = peer(game, parts.get(1).copied())?;
                clock += 1500;
                game.move_to((target.pos.0 + 1.0, target.pos.1, target.pos.2), clock)
                    .await?;
                game.change_target(target.obj_id).await?;
                game.start_skill(skill, target.obj_id).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                let refused = got.iter().find(|p| p.is_sc(sc::SCErrorMsgPacket)).map(|p| {
                    let mut s = p.stream();
                    s.read_i16().unwrap_or_default()
                });
                show(&got);
                if let Some(code) = refused {
                    bail!("attack at the peer was refused (error {code})");
                }
                ensure!(
                    got.iter().any(|p| p.is_sc(sc::SCSkillStartedPacket)
                        || p.is_sc(sc::SCSkillFiredPacket)
                        || p.is_sc(sc::SCSkillEndedPacket)
                        || p.is_sc(sc::SCUnitDamagedPacket)),
                    "the attack did not start"
                );
                println!("    attack started");
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
