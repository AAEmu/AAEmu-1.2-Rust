use std::time::Duration;

use aaemu_game::opcodes::sc;

use super::super::questitem::{chat_text, quest_state};
use super::num;
use crate::game::{parse_item_task, GameClient, Packet};

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

pub(super) fn show(packets: &[Packet]) {
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
pub(super) async fn show_quiet(game: &mut GameClient, secs: f32) -> anyhow::Result<()> {
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

/// `(child, point, parent, reason)` of an `SCUnitAttachedPacket` body.
pub(super) fn unit_attached(body: &[u8]) -> anyhow::Result<(u32, u8, u32, u8)> {
    let mut s = aaemu_commons::PacketStream::from_bytes(body.to_vec());
    Ok((s.read_bc()?, s.read_u8()?, s.read_bc()?, s.read_u8()?))
}

/// `CSCompleteQuestContextPacket(quest, npcObjId, doodadObjId, selected)` body.
pub(super) fn complete_quest_body(
    quest: u32,
    npc_obj: u32,
    doodad_obj: u32,
    selected: i32,
) -> Vec<u8> {
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
pub(super) fn execute_craft_body(craft_id: u32, doodad_obj: u32, count: i32) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_u32(craft_id).write_bc(doodad_obj).write_i32(count);
    b.into_bytes()
}

/// `CSReadMail` body as the client's `ReadMailById` builds it (`FUN_393cc2e0` -> writer
/// `FUN_397ba350`): `bool isSent, i64 mailId`.
pub(super) fn read_mail_body(is_sent: bool, mail_id: i64) -> Vec<u8> {
    let mut b = aaemu_commons::PacketStream::new();
    b.write_bool(is_sent).write_i64(mail_id);
    b.into_bytes()
}

/// `CSAuctionPost` body as the client's post-article call builds it (`FUN_394cea60` -> writer
/// `FUN_397bae70`): `bc auctioneerId, bc auctioneerId2, u64 itemId, i32 startPrice,
/// i32 buyoutPrice, u8 duration`.
pub(super) fn auction_post_body(
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
pub(super) fn auction_bid_body(
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
pub(super) struct MailHeaderView {
    pub(super) mail_id: i64,
    pub(super) mail_type: u8,
    pub(super) status: u8,
    pub(super) title: String,
    pub(super) sender_name: String,
    pub(super) attachments: u8,
    pub(super) receiver_name: String,
    pub(super) open_date: i64,
    pub(super) returned: bool,
    pub(super) extra: i64,
}

pub(super) fn decode_mail_header(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<MailHeaderView> {
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
pub(super) struct MailBodyView {
    pub(super) mail_id: i64,
    pub(super) mail_type: u8,
    pub(super) receiver_name: String,
    pub(super) title: String,
    pub(super) text: String,
    pub(super) copper_coins: i32,
    pub(super) billing_amount: i32,
    pub(super) money_amount2: i32,
    pub(super) send_date: i64,
    pub(super) recv_date: i64,
    pub(super) open_date: i64,
    pub(super) items: Vec<(u32, u64, u8, i32)>,
}

pub(super) fn decode_mail_body(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<MailBodyView> {
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
pub(super) fn decode_count_unread(
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
pub(super) struct AuctionLotView {
    pub(super) id: u64,
    pub(super) duration: u8,
    pub(super) item_template: u32,
    pub(super) item_id: u64,
    pub(super) world_id: u8,
    pub(super) client_id: u32,
    pub(super) client_name: String,
    pub(super) start_money: i32,
    pub(super) direct_money: i32,
    pub(super) post_date: i64,
    pub(super) bid_world_id: u8,
    pub(super) bidder_id: u32,
    pub(super) bidder_name: String,
    pub(super) bid_money: i32,
    pub(super) extra: i32,
}

pub(super) fn decode_auction_lot(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<AuctionLotView> {
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
pub(super) struct AuctionBidView {
    pub(super) lot_id: u64,
    pub(super) world_id: u8,
    pub(super) bidder_id: u32,
    pub(super) bidder_name: String,
    pub(super) money: i32,
}

pub(super) fn decode_auction_bid(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<AuctionBidView> {
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
pub(super) fn decode_auction_message(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<(u8, u32, i32)> {
    Ok((s.read_u8()?, s.read_u32()?, s.read_i32()?))
}

/// `SCCraftItemUnlock` as the client reads it (`FUN_397b4c60`): two u8s (slot type, index).
pub(super) fn decode_craft_unlock(s: &mut aaemu_commons::PacketStream) -> anyhow::Result<(u8, u8)> {
    Ok((s.read_u8()?, s.read_u8()?))
}

/// `SCRacingResult` as the client reads it (`FUN_397b0340`): i32 count, then `min(count, 10)`
/// rows of `{u32 type, u32 record}` (`FUN_397a8870`).
pub(super) fn decode_racing_result(
    s: &mut aaemu_commons::PacketStream,
) -> anyhow::Result<Vec<(u32, u32)>> {
    let count = s.read_i32()?;
    let mut rows = Vec::new();
    for _ in 0..count.clamp(0, 10) {
        rows.push((s.read_u32()?, s.read_u32()?));
    }
    Ok(rows)
}

/// `SCCraftFailed` as the client reads it (`FUN_397b8110`): u32 id, i32 count (clamped to 20),
/// `count` u32 item template ids.
pub(super) fn decode_craft_failed(
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
pub(super) fn parse_craft_args(parts: &[&str]) -> anyhow::Result<(u32, u32, i32)> {
    Ok((num(parts, 0)?, num(parts, 1)?, num(parts, 2)?))
}
