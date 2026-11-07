//! `script`: runs the commands of the file named by `$AAEMU_SCRIPT` after the usual login /
//! select / spawn, printing what the server sends back. Meant for ad-hoc live checks of quest
//! and spawn fixes (GM account needed), so a new check needs no rebuild of the bot.
//!
//! One command per line, `#` starts a comment:
//!
//! ```text
//! gm <command line>                  GM chat command (without the leading slash)
//! gmraw <command line>               same, without draining (the caller asserts the reply)
//! tp <x> <y> <z>                     /move there, acknowledge the teleport, wait for the area
//! moverel <dx> <dy> <dz>             /move to self + delta (e.g. leave an aura radius)
//! gopeer [name]                      /move to the peer's position (no drain)
//! pump <secs>                        just read packets
//! units [<npc template>...]          NPCs in sight (all within 60 m when no template given)
//! doodads [<doodad template>...]     doodads in sight (all within 40 m when none given)
//! sq <quest> <npc template|0>        CSStartQuestContext from the nearest NPC of that template
//! cq <quest> <npc template|0> [sel]  CSCompleteQuestContext at that NPC
//! cqd <quest> <doodad obj id> [sel]  CSCompleteQuestContext at that doodad object
//! target <npc template>              select the nearest NPC of that template
//! targetself                         select our own unit (e.g. for `/buff` on self)
//! kill <npc template>                walk next to the nearest NPC, target it, `/kill`
//! cast <skill> <npc template>        CSStartSkill on the nearest NPC
//! castraw <skill> <npc template>     walk next to the nearest NPC, target it, send CSStartSkill
//!                                    and do not drain (the caller asserts the delayed effect packets)
//! slave <template>                  nearest slave (obj / tl / template)
//! interact <template>               CSStartInteraction on the slave, print its skill list
//! bind <template>                   CSBindSlave 0x031 with the slave's tl
//! scast <skill> <template> [point]  CSStartSkill on the nearest slave (wait for the attach)
//! mcast <skill> <template> <mount row>  vehicle bar cast (Mount caster) on the nearest slave
//! scastfail <skill> <template> <code>  CSStartSkill on the slave, wait for SCErrorMsg
//! expectattach <point>              fail unless SCUnitAttached for us at that point
//! expecterror <code>                fail unless SCErrorMsg with that code
//! dcast <skill> <doodad template>    doodad-target CSStartSkill
//! buycoin <doodad template> <func>   CSBuyCoinItemPacket (DoodadFuncPurchase) and print the reply
//! buycoinraw <doodad template> <func>  same without draining (the caller asserts the reply)
//! goods <npc template>              walk to the NPC, CSListSpecialtyGoods and print SCSpecialtyGoods
//! sellpack <npc template>           walk to the NPC, CSSellBackpackGoods; fail unless the labor
//!                                   and item-task answers arrive (the payment mail is delayed)
//! trader <npc template>             replay the captured trader talk (0x068, 0x065, 0x052 25205,
//!                                   0x043, 0x065 false, 0x042) and fail if 0x9b comes back
//! priceboard <from> <to>            CSRequestSpecialtyCurrent (0x131) and print the parsed table
//! cexec <craft> <doodad obj> <n>    CSExecuteCraft (client writer `FUN_397bbfc0`), print the
//!                                   decoded craft replies (`SCCraftItemUnlock`, `SCCraftFailed`)
//! maillist                          CSListMail (empty) and decode `SCMailList` / `SCMailListEnd`
//! mailread <isSent 0|1> <mailId>    CSReadMail (client writer `FUN_397ba350`) and decode
//!                                   `SCMailBody` (`FUN_397af2d0` / `FUN_398a5e60`)
//! aucpost <npc tpl> <item tpl> <start> <buyout> <duration>
//!                                   walk to the auctioneer NPC, CSAuctionPost (client writer
//!                                   `FUN_397bae70`) and decode `SCAuctionPosted`
//! aucbid <npc tpl> <lotId> <money>  CSAuctionBid (client writer `FUN_397bb050` + `FUN_398a6880`)
//!                                   and decode `SCAuctionBid` / `SCAuctionMessage`
//! item <template> <count>            `/item add self` and remember the stack id / slot
//! equip <template> <slot>            move the remembered stack to equipment slot `slot` (26 = back)
//! moveitem <id> <ftype> <fslot> <ttype> <tslot>  CSSwapItemsPacket with an explicit item id
//! icast <skill> <item template> <npc template|self|peer|player:NAME> [metres [max secs]]
//!                                    cast from the remembered item stack
//! icastitem <skill> <scroll template> <target item template> error <code>|ok
//!                                    item-on-item cast (`GradeEnchant` scroll on the target
//!                                    item); fails unless the expected error (and no result) or
//!                                    an `SCGradeEnchantResult` arrives.
//! waitpeer [secs] [name]             read packets until another player is in sight (60 s default)
//! rawcs <opcode hex> [<body hex>]    send any client packet (`SELF` / `PEER` / `ITEM(<template>)`
//!                                    in the body hex)
//! rawcsend <opcode hex> <body hex>   send it without draining (the caller asserts the reply)
//! rawcswait <opcode hex> <body hex> <PacketName> [secs]
//!                                    send it and fail unless that packet answers (default 5 s)
//! targetpeer [name]                 select the peer (GM commands that act on the target)
//! pkill [name]                       `/kill` with the peer targeted (GM)
//! await <Event> [secs]               wait (60 s default) for a party/guild event and consume it
//! partyinvite <name>                 CSInviteToTeam (party) by character name
//! partyaccept                        wait for SCAskToJoinTeam and accept it
//! areainvite [team] [party|raid]     CSInviteAreaToTeam (team 0 = the current team); the
//!                                    answer is left for expectpkt (SCTeamAreaInvitedPacket)
//! expectareacool [secs]              fail unless SCTeamAreaInvited arrives with s=false and a
//!                                    positive remaining cooldown in r
//! partyacceptarea                    wait for SCAskToJoinTeamArea and accept it (isArea=1);
//!                                    the join packets are left for expectpkt
//! gcreate <name>                     CSCreateExpedition
//! ginvite [name]                     CSInviteToExpedition to the peer
//! gaccept                            wait for the guild invitation and join
//! greply <faction id> <0|1>          CSReplyExpeditionInvitation without waiting for an invite
//! grename <name>                     CSRenameExpedition for the bot's own guild (the `GUILD` id)
//! gexpectrenamed <name>              fail unless SCFactionRenamed carries that name
//! gexpectretry <error>               fail unless SCFactionRetryRename re-opens the expedition dialog
//!                                    with that error code
//! ninvite [name]                     CSFactionImmigrationInvite to the peer (nation king)
//! nlevel [name] <level>              `/level <peer> <level>` (GM)
//! naccept [0|1]                      wait for SCFactionImmigrateInvite and answer it
//! nreply <nation> <nonce> <0|1>      CSFactionImmigrationInviteReply without an invitation
//! norigin                            CSFactionImmigrateToOrigin
//! nkick <name>                       CSFactionKickToOrigin
//! nhostile <faction>                 CSFactionDeclareHostile (nation king)
//! nattack <skill> [name]             target the peer and cast at it; fail unless SCSkillStarted
//!                                    arrives without SCErrorMsg
//! hangup                            hard disconnect (socket closed, no CSLeaveWorld); ends the script
//! expectseen <code> [secs]          fail unless an SCErrorMsg with that code arrived since the last
//!                                   `expectseen` (also one an earlier command already drained)
//! sync <token> [count]              suite barrier: wait until `count` bots (default: all) are here
//! expectpkt <PacketName> [secs]     fail unless the packet arrives within `secs` (default 5)
//! expectnopkt <PacketName> [secs]   fail if the packet arrives within `secs` (default 5)
//! expectbuff <buff> [self|peer|player:NAME] [secs]
//!                                   fail unless SCBuffCreated for that buff / owner arrives
//! expectbuffremoved <buff> [self|peer|player:NAME] [secs]
//!                                   fail unless SCBuffRemoved for a previously announced
//!                                   SCBuffCreated of that buff / owner arrives
//! expectlog <regex> [secs]          fail unless a new game-log line matches (AAEMU_VERIFY_GAME_LOG)
//! expectdb "<sql>" <value> [secs]   fail unless the SQL's value equals `value`
//!                                   (AAEMU_VERIFY_MYSQL; re-checked for `secs` when given)
//! rezexpect inplace|temple ok|refused  CSResurrectCharacter, then fail unless SCCharacterResurrected
//!                                    for our own unit did (ok) / did not (refused) arrive
//! deathwait <wait_ms> <lost|*> <dur|*> [secs]
//!                                    wait for our own SCUnitDeathPacket and check the resurrect
//!                                    wait, lost exp (`>0` = any loss) and durability loss
//! lootexpect took|failed [npc obj]  like `loot`, then fail unless SCLootItemTook (took) / SCLootItemFailed
//!                                    (failed) was among the replies; `took` first walks to 2 m of
//!                                    the corpse, as the real client does
//! ```
//!
//! Every failed expectation prints a `FAIL` line and makes the run exit non-zero.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{cs, sc};
use anyhow::{bail, ensure, Context};

use super::questitem::{chat_text, gm_command, quest_state, stack_id, stack_location};
use super::{pump, skill_response_result, Runner};
use crate::game::{parse_item_task, GameClient, Packet, UnitInfo};
use crate::social::{self, SocialEvent};

/// How close `lootexpect took` walks to a corpse first (retail enables looting at 3 m or less).
const LOOT_APPROACH_RANGE: f32 = 2.0;

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

/// `SCBuffCreatedPacket`: (owner obj id, buff index, buff template id).
fn buff_created(p: &Packet) -> Option<(u32, u32, u32)> {
    use aaemu_game::systems::skills::types::SkillCaster;
    let mut s = p.stream();
    let kind = s.read_u8().ok()?;
    SkillCaster::read(kind, &mut s).ok()?;
    s.read_u32().ok()?;
    let owner = s.read_bc().ok()?;
    let index = s.read_u32().ok()?;
    let buff = s.read_u32().ok()?;
    Some((owner, index, buff))
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
                Some((owner, index, buff)) => {
                    println!("    SCBuffCreated owner {owner} index {index} buff {buff}")
                }
                None => println!("    SCBuffCreated (unreadable)"),
            }
        } else if p.is_sc(sc::SCBuffRemovedPacket) {
            let mut s = p.stream();
            if let (Ok(owner), Ok(index)) = (s.read_bc(), s.read_u32()) {
                println!("    SCBuffRemoved owner {owner} index {index}");
            }
        } else if p.is_sc(sc::SCErrorMsgPacket) {
            println!("    SCErrorMsg {:?}", p.stream().read_i16());
        } else if p.is_sc(sc::SCDoodadCompleteQuestPacket) {
            let mut s = p.stream();
            if let (Ok(doodad), Ok(quest)) = (s.read_bc(), s.read_u32()) {
                println!("    SCDoodadCompleteQuest doodad {doodad} quest {quest}");
            } else {
                println!("    SCDoodadCompleteQuest (unreadable)");
            }
        } else if p.is_sc(sc::SCRaceCondPacket) {
            let mut s = p.stream();
            match (s.read_u8(), s.read_u32(), s.read_u32()) {
                (Ok(rc), Ok(track), Ok(after)) => {
                    println!("    SCRaceCond rc {rc} track {track} after {after} ms");
                }
                _ => println!("    SCRaceCond (unreadable)"),
            }
        } else if p.is_sc(sc::SCRacingPacket) {
            let mut s = p.stream();
            match (s.read_u8(), s.read_u32(), s.read_i32()) {
                (Ok(rc), Ok(record), Ok(v1)) => {
                    println!("    SCRacing rc {rc} record {record} ms v1 {v1}");
                }
                _ => println!("    SCRacing (unreadable)"),
            }
        } else if p.is_sc(sc::SCRacingResultPacket) {
            match decode_racing_result(&mut p.stream()) {
                Ok(rows) => println!("    SCRacingResult rows {rows:?}"),
                Err(_) => println!("    SCRacingResult (unreadable)"),
            }
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

/// The nearest slave (unit type 2) of `template`.
fn nearest_slave(game: &GameClient, template: u32) -> Option<UnitInfo> {
    let me = game.state.self_pos;
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 2 && u.id == template)
        .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
        .cloned()
}

/// `(child, point, parent, reason)` of an `SCUnitAttachedPacket` body.
fn unit_attached(body: &[u8]) -> anyhow::Result<(u32, u8, u32, u8)> {
    let mut s = aaemu_commons::PacketStream::from_bytes(body.to_vec());
    Ok((s.read_bc()?, s.read_u8()?, s.read_bc()?, s.read_u8()?))
}

fn num<T: std::str::FromStr>(parts: &[&str], i: usize) -> anyhow::Result<T> {
    parts
        .get(i)
        .with_context(|| format!("missing argument {i}"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("bad number '{}'", parts[i]))
}

/// `CSCompleteQuestContextPacket(quest, npcObjId, doodadObjId, selected)` body.
fn complete_quest_body(quest: u32, npc_obj: u32, doodad_obj: u32, selected: i32) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_u32(quest)
        .write_bc(npc_obj)
        .write_bc(doodad_obj)
        .write_i32(selected);
    b.into_bytes()
}

// ---------------------------------------------------------------------------------------------
// Craft / mail / auction: C->S bodies built exactly like the 1.2 client writer functions.
// ---------------------------------------------------------------------------------------------

/// `CSExecuteCraft` body as `X2Craft.ExecuteCraft` builds it (`FUN_394d1340` -> writer
/// `FUN_397bbfc0`): `u32 craftId, bc doodadObjId, i32 count`.
fn execute_craft_body(craft_id: u32, doodad_obj: u32, count: i32) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_u32(craft_id).write_bc(doodad_obj).write_i32(count);
    b.into_bytes()
}

/// `CSReadMail` body as the client's `ReadMailById` builds it (`FUN_393cc2e0` -> writer
/// `FUN_397ba350`): `bool isSent, i64 mailId`.
fn read_mail_body(is_sent: bool, mail_id: i64) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_bool(is_sent).write_i64(mail_id);
    b.into_bytes()
}

/// `CSAuctionPost` body as the client's post-article call builds it (`FUN_394cea60` -> writer
/// `FUN_397bae70`): `bc auctioneerId, bc auctioneerId2, u64 itemId, i32 startPrice,
/// i32 buyoutPrice, u8 duration`.
fn auction_post_body(
    auctioneer_id: u32,
    auctioneer_id2: u32,
    item_id: u64,
    start_price: i32,
    buyout_price: i32,
    duration: u8,
) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_bc(auctioneer_id)
        .write_bc(auctioneer_id2)
        .write_u64(item_id)
        .write_i32(start_price)
        .write_i32(buyout_price)
        .write_u8(duration);
    b.into_bytes()
}

/// `CSBidAuction` body as the client's bid call builds it (`FUN_394cfff0` -> writer
/// `FUN_397bb050`): `bc auctioneerId, bc auctioneerId2, AuctionDisplay, AuctionBid`.
///
/// `AuctionDisplay` is the client's copy of the listed lot (`FUN_398a6de0`). The server only
/// reads its `id`; the other fields are sent zeroed, with an empty item (`templateId 0`), which
/// is the same wire layout the client writes for a real lot.
fn auction_bid_body(
    auctioneer_id: u32,
    auctioneer_id2: u32,
    lot_id: u64,
    bidder_id: u32,
    bidder_name: &str,
    money: i32,
) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_bc(auctioneer_id).write_bc(auctioneer_id2);
    // AuctionDisplay (client writer `FUN_398a6de0`).
    b.write_u64(lot_id)
        .write_u8(0) // duration
        .write_u32(0) // item template 0: the client writes no item body
        .write_u8(0) // worldId
        .write_u32(0) // clientId
        .write_str("") // clientName
        .write_i32(0) // startMoney
        .write_i32(0) // directMoney
        .write_i64(0) // postDate
        .write_u8(0) // bidWorldId
        .write_u32(0) // bidderId
        .write_str("") // bidderName
        .write_i32(0) // bidMoney
        .write_i32(0); // extra

    // AuctionBid (client writer `FUN_398a6880`).
    b.write_u64(lot_id)
        .write_u8(0) // worldId
        .write_u32(bidder_id)
        .write_str(bidder_name)
        .write_i32(money);
    b.into_bytes()
}

// ---------------------------------------------------------------------------------------------
// Craft / mail / auction: S->C decoders from the 1.2 client reader functions (not the server's
// writers, so a server-side wire change cannot hide behind a mirrored decoder).
// ---------------------------------------------------------------------------------------------

/// `MailHeader` as the client reads it (`FUN_398a5a00`): i64 mailId, u8 type, u8 status,
/// string title, string senderName, u8 attachments, string receiverName, i64 openDate,
/// bool returned, i64 extra.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MailHeaderView {
    mail_id: i64,
    mail_type: u8,
    status: u8,
    title: String,
    sender_name: String,
    attachments: u8,
    receiver_name: String,
    open_date: i64,
    returned: bool,
    extra: i64,
}

fn decode_mail_header(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<MailHeaderView> {
    Ok(MailHeaderView {
        mail_id: s.read_i64()?,
        mail_type: s.read_u8()?,
        status: s.read_u8()?,
        title: s.read_string()?,
        sender_name: s.read_string()?,
        attachments: s.read_u8()?,
        receiver_name: s.read_string()?,
        open_date: s.read_i64()?,
        returned: s.read_bool()?,
        extra: s.read_i64()?,
    })
}

/// `MailBody` as the client reads it (`FUN_398a5e60`): i64 mailId, u8 type, string receiverName,
/// string title, string text, i32 moneyAmount x3, i64 sendDate/recvDate/openDate, ten items.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MailBodyView {
    mail_id: i64,
    mail_type: u8,
    receiver_name: String,
    title: String,
    text: String,
    copper_coins: i32,
    billing_amount: i32,
    money_amount2: i32,
    send_date: i64,
    recv_date: i64,
    open_date: i64,
    items: Vec<(u32, u64, u8, i32)>,
}

fn decode_mail_body(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<MailBodyView> {
    let mail_id = s.read_i64()?;
    let mail_type = s.read_u8()?;
    let receiver_name = s.read_string()?;
    let title = s.read_string()?;
    let text = s.read_string()?;
    let copper_coins = s.read_i32()?;
    let billing_amount = s.read_i32()?;
    let money_amount2 = s.read_i32()?;
    let send_date = s.read_i64()?;
    let recv_date = s.read_i64()?;
    let open_date = s.read_i64()?;
    let mut items = Vec::with_capacity(10);
    for _ in 0..10 {
        items.push(decode_item_summary(s)?);
    }
    Ok(MailBodyView {
        mail_id,
        mail_type,
        receiver_name,
        title,
        text,
        copper_coins,
        billing_amount,
        money_amount2,
        send_date,
        recv_date,
        open_date,
        items,
    })
}

/// `UnreadMailCount` as the client reads it (`FUN_398a5700`): four i32 counters.
fn decode_count_unread(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<(i32, i32, i32, i32)> {
    Ok((s.read_i32()?, s.read_i32()?, s.read_i32()?, s.read_i32()?))
}

/// One `Item` record as the client reads it (`FUN_3961f500`): `u32 templateId`; when nonzero
/// `u64 id, u8 grade, u8 flags, i32 stackSize, ItemDetail, i64 creationTime, u32 lifespanMins,
/// u32 madeUnitId, u8 worldId, i64 unsecureDateTime, i64 unpackDateTime`. Only the summary is
/// kept; the detail body is skipped with the client's own length table.
fn decode_item_summary(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<(u32, u64, u8, i32)> {
    let template = s.read_u32()?;
    if template == 0 {
        return Ok((0, 0, 0, 0));
    }
    let id = s.read_u64()?;
    let grade = s.read_u8()?;
    let _flags = s.read_u8()?;
    let count = s.read_i32()?;
    let detail = s.read_u8()?;
    s.read_bytes(aaemu_game::models::item::detail_body_len(detail))?;
    s.read_i64()?; // creationTime
    s.read_i32()?; // lifespanMins
    s.read_u32()?; // madeUnitId
    s.read_u8()?; // worldId
    s.read_i64()?; // unsecureDateTime
    s.read_i64()?; // unpackDateTime
    Ok((template, id, grade, count))
}

/// `AuctionLot` as the client reads it (`FUN_398a6de0`): u64 id, u8 duration, Item, u8 worldId,
/// u32 clientId, string clientName, i32 startMoney, i32 directMoney, i64 postDate,
/// u8 bidWorldId, u32 bidderId, string bidderName, i32 bidMoney, i32 extra.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuctionLotView {
    id: u64,
    duration: u8,
    item_template: u32,
    item_id: u64,
    world_id: u8,
    client_id: u32,
    client_name: String,
    start_money: i32,
    direct_money: i32,
    post_date: i64,
    bid_world_id: u8,
    bidder_id: u32,
    bidder_name: String,
    bid_money: i32,
    extra: i32,
}

fn decode_auction_lot(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<AuctionLotView> {
    let id = s.read_u64()?;
    let duration = s.read_u8()?;
    let (item_template, item_id, _, _) = decode_item_summary(s)?;
    Ok(AuctionLotView {
        id,
        duration,
        item_template,
        item_id,
        world_id: s.read_u8()?,
        client_id: s.read_u32()?,
        client_name: s.read_string()?,
        start_money: s.read_i32()?,
        direct_money: s.read_i32()?,
        post_date: s.read_i64()?,
        bid_world_id: s.read_u8()?,
        bidder_id: s.read_u32()?,
        bidder_name: s.read_string()?,
        bid_money: s.read_i32()?,
        extra: s.read_i32()?,
    })
}

/// `AuctionBid` as the client reads it (`FUN_398a6880`): u64 lotId, u8 worldId, u32 bidderId,
/// string bidderName, i32 money.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuctionBidView {
    lot_id: u64,
    world_id: u8,
    bidder_id: u32,
    bidder_name: String,
    money: i32,
}

fn decode_auction_bid(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<AuctionBidView> {
    Ok(AuctionBidView {
        lot_id: s.read_u64()?,
        world_id: s.read_u8()?,
        bidder_id: s.read_u32()?,
        bidder_name: s.read_string()?,
        money: s.read_i32()?,
    })
}

/// `SCAuctionMessage` as the client reads it (`FUN_397b56f0`): u8 msgType, u32 itemType,
/// i32 moneyAmount.
fn decode_auction_message(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<(u8, u32, i32)> {
    Ok((s.read_u8()?, s.read_u32()?, s.read_i32()?))
}

/// `SCCraftItemUnlock` as the client reads it (`FUN_397b4c60`): two u8s (slot type, index).
fn decode_craft_unlock(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<(u8, u8)> {
    Ok((s.read_u8()?, s.read_u8()?))
}

/// `SCRacingResult` as the client reads it (`FUN_397b0340`): i32 count, then `min(count, 10)`
/// rows of `{u32 type, u32 record}` (`FUN_397a8870`).
fn decode_racing_result(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<Vec<(u32, u32)>> {
    let count = s.read_i32()?;
    let mut rows = Vec::new();
    for _ in 0..count.clamp(0, 10) {
        rows.push((s.read_u32()?, s.read_u32()?));
    }
    Ok(rows)
}

/// `SCCraftFailed` as the client reads it (`FUN_397b8110`): u32 id, i32 count (clamped to 20),
/// `count` u32 item template ids.
fn decode_craft_failed(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<(u32, i32, Vec<u32>)> {
    let id = s.read_u32()?;
    let count = s.read_i32()?;
    let mut items = Vec::new();
    for _ in 0..count.clamp(0, 20) {
        items.push(s.read_u32()?);
    }
    Ok((id, count, items))
}

/// `cexec <craftId> <doodadObjId> <count>` arguments.
fn parse_craft_args(parts: &[&str]) -> anyhow::Result<(u32, u32, i32)> {
    Ok((num(parts, 0)?, num(parts, 1)?, num(parts, 2)?))
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

/// The object a buff expectation watches (`self`, `peer` or `player:NAME`).
fn buff_target(game: &GameClient, spec: Option<&str>) -> anyhow::Result<u32> {
    match spec.unwrap_or("self") {
        "self" => Ok(game.state.self_obj),
        "peer" => Ok(peer(game, None)?.obj_id),
        other => match other.strip_prefix("player:") {
            Some(name) => Ok(peer(game, Some(name))?.obj_id),
            None => bail!("buff target must be self, peer or player:NAME (got '{other}')"),
        },
    }
}

/// `expectbuff <buff> [self|peer|player:NAME] [secs]`.
async fn expect_buff(
    game: &mut GameClient,
    buff: u32,
    spec: Option<&str>,
    secs: u64,
) -> anyhow::Result<()> {
    let target = buff_target(game, spec)?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        // The state tracks every buff packet, including those an earlier command drained.
        if let Some(indices) = game.state.buffs.get(&(target, buff)) {
            let indices: Vec<u32> = indices.iter().copied().collect();
            if !indices.is_empty() {
                println!("    buff {buff} on {target} present (indices {indices:?})");
                return Ok(());
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected buff {buff} on {target} within {secs} s; it did not arrive");
        }
        game.recv_within(left.min(Duration::from_millis(250)))
            .await?;
    }
}

/// `expectbuffremoved <buff> [self|peer|player:NAME] [secs]`.
async fn expect_buff_removed(
    game: &mut GameClient,
    buff: u32,
    spec: Option<&str>,
    secs: u64,
) -> anyhow::Result<()> {
    let target = buff_target(game, spec)?;
    anyhow::ensure!(
        game.state
            .buffs
            .get(&(target, buff))
            .is_some_and(|indices| !indices.is_empty()),
        "expected buff {buff} on {target} to be present before waiting for its removal"
    );
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if game
            .state
            .buffs
            .get(&(target, buff))
            .is_none_or(|indices| indices.is_empty())
        {
            println!("    buff {buff} on {target} removed");
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected buff {buff} on {target} to be removed within {secs} s; it was not");
        }
        game.recv_within(left.min(Duration::from_millis(250)))
            .await?;
    }
}

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
                let peer_bc = if raw.contains("PEER") {
                    bc(peer(game, None)
                        .context("PEER needs another player in sight")?
                        .obj_id)
                } else {
                    String::new()
                };
                // `GUILD` is the bot's guild id as a little-endian u32.
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
                    .replace("SELF", &bc(game.state.self_obj))
                    .replace("PEER", &peer_bc);
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

    #[test]
    fn complete_quest_body_writes_the_doodad_object_id() {
        use aaemu_commons::PacketStream;
        use aaemu_game::net::registry::ClientPacket;
        use aaemu_game::systems::quests::packets::CSCompleteQuestContextPacket;

        let body = super::complete_quest_body(2563, 0, 0x300, 1);
        assert_eq!(
            body,
            vec![0x03, 0x0a, 0, 0, 0, 0, 0, 0, 0x03, 0, 1, 0, 0, 0]
        );
        // The server's reader must see the doodad object id, not an NPC object id.
        let p = CSCompleteQuestContextPacket::read(&mut PacketStream::from_bytes(body)).unwrap();
        assert_eq!(
            p,
            CSCompleteQuestContextPacket {
                quest_context_id: 2563,
                npc_obj_id: 0,
                doodad_obj_id: 0x300,
                selected: 1,
            }
        );

        let body = super::complete_quest_body(100, 0x200, 0, -1);
        assert_eq!(
            body,
            vec![100, 0, 0, 0, 0x00, 0x02, 0x00, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]
        );
        let p = CSCompleteQuestContextPacket::read(&mut PacketStream::from_bytes(body)).unwrap();
        assert_eq!((p.npc_obj_id, p.doodad_obj_id, p.selected), (0x200, 0, -1));
    }

    #[test]
    fn cexec_arguments_parse() {
        assert_eq!(
            super::parse_craft_args(&["1", "2", "3"]).unwrap(),
            (1, 2, 3)
        );
        assert!(super::parse_craft_args(&["1", "2"]).is_err());
        assert!(super::parse_craft_args(&["x", "2", "3"]).is_err());
    }

    #[test]
    fn execute_craft_body_matches_the_client_writer() {
        // `FUN_397bbfc0`: u32 craftId, bc doodadObjId, i32 count.
        assert_eq!(
            super::execute_craft_body(987, 0x300, 1),
            vec![0xdb, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn read_mail_body_matches_the_client_writer() {
        // `FUN_397ba350`: bool isSent, i64 mailId.
        assert_eq!(
            super::read_mail_body(false, 0x0102_0304_0506_0708),
            vec![0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(
            super::read_mail_body(true, -2),
            vec![0x01, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn auction_post_body_matches_the_client_writer() {
        // `FUN_397bae70`: bc, bc, u64 itemId, i32 startPrice, i32 buyoutPrice, u8 duration.
        assert_eq!(
            super::auction_post_body(0x300, 0, 0x0102_0304_0506_0708, 10, 20, 2),
            vec![
                0x00, 0x03, 0x00, // auctioneerId
                0x00, 0x00, 0x00, // auctioneerId2
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // itemId
                0x0a, 0x00, 0x00, 0x00, // startPrice
                0x14, 0x00, 0x00, 0x00, // buyoutPrice
                0x02, // duration
            ]
        );
    }

    #[test]
    fn auction_bid_body_matches_the_client_writer() {
        // `FUN_397bb050` + `FUN_398a6de0` (AuctionDisplay) + `FUN_398a6880` (AuctionBid).
        assert_eq!(
            super::auction_bid_body(0x300, 0, 0x1122_3344_5566_7788, 0x0a, "Bob", 42),
            vec![
                0x00, 0x03, 0x00, // auctioneerId
                0x00, 0x00, 0x00, // auctioneerId2
                0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // display.id
                0x00, // display.duration
                0x00, 0x00, 0x00, 0x00, // display.item templateId 0
                0x00, // display.worldId
                0x00, 0x00, 0x00, 0x00, // display.clientId
                0x00, 0x00, // display.clientName ""
                0x00, 0x00, 0x00, 0x00, // display.startMoney
                0x00, 0x00, 0x00, 0x00, // display.directMoney
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // display.postDate
                0x00, // display.bidWorldId
                0x00, 0x00, 0x00, 0x00, // display.bidderId
                0x00, 0x00, // display.bidderName ""
                0x00, 0x00, 0x00, 0x00, // display.bidMoney
                0x00, 0x00, 0x00, 0x00, // display.extra
                0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // bid.lotId
                0x00, // bid.worldId
                0x0a, 0x00, 0x00, 0x00, // bid.bidderId
                0x03, 0x00, b'B', b'o', b'b', // bid.bidderName
                0x2a, 0x00, 0x00, 0x00, // bid.money
            ]
        );
    }

    #[test]
    fn mail_header_decodes_the_client_reader_layout() {
        // `FUN_398a5a00`: i64, u8, u8, string, string, u8, string, i64, bool, i64.
        let bytes = vec![
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // mailId
            0x03, // type
            0x01, // status
            0x02, 0x00, b'h', b'i', // title
            0x02, 0x00, b'A', b'l', // senderName
            0x02, // attachments
            0x02, 0x00, b'B', b'o', // receiverName
            0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // openDate
            0x01, // returned
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // extra -1
        ];
        let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
        let h = super::decode_mail_header(&mut s).unwrap();
        assert_eq!(s.remaining(), 0);
        assert_eq!(
            h,
            super::MailHeaderView {
                mail_id: 0x0102_0304_0506_0708,
                mail_type: 3,
                status: 1,
                title: "hi".into(),
                sender_name: "Al".into(),
                attachments: 2,
                receiver_name: "Bo".into(),
                open_date: 0x11,
                returned: true,
                extra: -1,
            }
        );
    }

    #[test]
    fn mail_body_decodes_the_client_reader_layout() {
        // `FUN_398a5e60`: i64, u8, string, string, string, i32 x3, i64 x3, ten Items.
        let mut bytes = vec![
            0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mailId
            0x01, // type
            0x02, 0x00, b'B', b'o', // receiverName
            0x01, 0x00, b'T', // title
            0x01, 0x00, b'x', // text
            0x01, 0x00, 0x00, 0x00, // copperCoins
            0x02, 0x00, 0x00, 0x00, // billingAmount
            0x03, 0x00, 0x00, 0x00, // moneyAmount2
            0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // sendDate
            0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // recvDate
            0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // openDate
            // attachment 0: `Item` (`FUN_3961f500`) with detail type 0.
            0xd2, 0x04, 0x00, 0x00, // templateId 1234
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // id
            0x05, // grade
            0x00, // flags
            0x07, 0x00, 0x00, 0x00, // stackSize
            0x00, // detailType
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // creationTime
            0x00, 0x00, 0x00, 0x00, // lifespanMins
            0x00, 0x00, 0x00, 0x00, // madeUnitId
            0x00, // worldId
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // unsecureDateTime
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // unpackDateTime
        ];
        // Attachments 1..9 are empty items (`templateId 0`, no body).
        bytes.extend(std::iter::repeat_n(0, 9 * 4));
        let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
        let b = super::decode_mail_body(&mut s).unwrap();
        assert_eq!(s.remaining(), 0);
        assert_eq!(
            b,
            super::MailBodyView {
                mail_id: 5,
                mail_type: 1,
                receiver_name: "Bo".into(),
                title: "T".into(),
                text: "x".into(),
                copper_coins: 1,
                billing_amount: 2,
                money_amount2: 3,
                send_date: 10,
                recv_date: 11,
                open_date: 12,
                items: {
                    let mut items = vec![(1234, 0x0102_0304_0506_0708, 5, 7)];
                    items.extend(std::iter::repeat_n((0, 0, 0, 0), 9));
                    items
                },
            }
        );
    }

    #[test]
    fn auction_lot_decodes_the_client_reader_layout() {
        // `FUN_398a6de0`: u64, u8, Item, u8, u32, string, i32 x2, i64, u8, u32, string,
        // i32 x2.
        let bytes = vec![
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // id
            0x02, // duration
            0x00, 0x00, 0x00, 0x00, // item templateId 0
            0x03, // worldId
            0x0d, 0x0c, 0x0b, 0x0a, // clientId
            0x01, 0x00, b'S', // clientName
            0x64, 0x00, 0x00, 0x00, // startMoney
            0xc8, 0x00, 0x00, 0x00, // directMoney
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // postDate
            0x04, // bidWorldId
            0x11, 0x00, 0x00, 0x00, // bidderId
            0x01, 0x00, b'B', // bidderName
            0x32, 0x00, 0x00, 0x00, // bidMoney
            0xfe, 0xff, 0xff, 0xff, // extra -2
        ];
        let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
        let lot = super::decode_auction_lot(&mut s).unwrap();
        assert_eq!(s.remaining(), 0);
        assert_eq!(
            lot,
            super::AuctionLotView {
                id: 0x1122_3344_5566_7788,
                duration: 2,
                item_template: 0,
                item_id: 0,
                world_id: 3,
                client_id: 0x0a0b_0c0d,
                client_name: "S".into(),
                start_money: 100,
                direct_money: 200,
                post_date: 0x0102_0304_0506_0708,
                bid_world_id: 4,
                bidder_id: 0x11,
                bidder_name: "B".into(),
                bid_money: 50,
                extra: -2,
            }
        );
    }

    #[test]
    fn auction_bid_decodes_the_client_reader_layout() {
        // `FUN_398a6880`: u64 lotId, u8 worldId, u32 bidderId, string bidderName, i32 money.
        let bytes = vec![
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // lotId
            0x04, // worldId
            0x09, 0x00, 0x00, 0x00, // bidderId
            0x03, 0x00, b'B', b'i', b'd', // bidderName
            0x2a, 0x00, 0x00, 0x00, // money
        ];
        let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
        let bid = super::decode_auction_bid(&mut s).unwrap();
        assert_eq!(s.remaining(), 0);
        assert_eq!(
            bid,
            super::AuctionBidView {
                lot_id: 0x1122_3344_5566_7788,
                world_id: 4,
                bidder_id: 9,
                bidder_name: "Bid".into(),
                money: 42,
            }
        );
    }

    #[test]
    fn auction_message_decodes_the_client_reader_layout() {
        // `FUN_397b56f0`: u8 msgType, u32 itemType, i32 moneyAmount.
        let mut s = aaemu_commons::PacketStream::from_bytes(vec![
            0x01, 0xd2, 0x04, 0x00, 0x00, 0x2a, 0x00, 0x00, 0x00,
        ]);
        assert_eq!(
            super::decode_auction_message(&mut s).unwrap(),
            (1, 1234, 42)
        );
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn craft_unlock_and_failure_decode_the_client_reader_layouts() {
        // `FUN_397b4c60`: two u8s.
        let mut s = aaemu_commons::PacketStream::from_bytes(vec![0x01, 0x02]);
        assert_eq!(super::decode_craft_unlock(&mut s).unwrap(), (1, 2));
        assert_eq!(s.remaining(), 0);

        // `FUN_397b8110`: u32 id, i32 count, count u32 item template ids.
        let mut s = aaemu_commons::PacketStream::from_bytes(vec![
            0x07, 0x00, 0x00, 0x00, // id
            0x02, 0x00, 0x00, 0x00, // count
            0x64, 0x00, 0x00, 0x00, // item 100
            0xc8, 0x00, 0x00, 0x00, // item 200
        ]);
        assert_eq!(
            super::decode_craft_failed(&mut s).unwrap(),
            (7, 2, vec![100, 200])
        );
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn racing_result_decodes_the_client_reader_layout() {
        // `FUN_397b0340`: i32 count, then min(count, 10) rows of {u32 type, u32 record}.
        let mut s = aaemu_commons::PacketStream::from_bytes(vec![
            0x02, 0x00, 0x00, 0x00, // count
            0x02, 0x00, 0x00, 0x00, 0x88, 0x13, 0x00, 0x00, // type 2, record 5000
            0x07, 0x00, 0x00, 0x00, 0x60, 0xEA, 0x00, 0x00, // type 7, record 60000
        ]);
        assert_eq!(
            super::decode_racing_result(&mut s).unwrap(),
            vec![(2, 5000), (7, 60000)]
        );
        assert_eq!(s.remaining(), 0);
    }
}
