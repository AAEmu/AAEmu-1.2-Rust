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
//! walkrel <dx> <dy> <dz>             plain CSMoveUnit to self + delta (no GM command; keep it to
//!                                    a few metres, the move guard refuses a larger step)
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
//! pcast <skill> <x> <y> <z>           ground-target CSStartSkill
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
//! mailtakefirst [mail type]         CSListMail, take the first received mail with attachments
//!                                   (type filter when given) via CSTakeAttachmentSequentially
//!                                   and fail unless `SCAttachmentTaken` lists an item
//! buyics <shop id> <detail> [main] [sub] [receiver]
//!                                   CSICSBuyGoodPacket (0x11d); fail unless the buy succeeds,
//!                                   `SCGotMail` is a CHARGED (9) mail and SCCountUnreadMail's
//!                                   4th counter is 1
//! giftics <shop id> <detail> <receiver> [main] [sub]
//!                                   CSICSBuyGoodPacket (0x11d) as a gift; fail unless
//!                                   SCICSBuyResult is successful and no SCErrorMsg arrives.
//!                                   The receiver may be offline (no SCGotMail to the buyer).
//! unreadmail <sent> <received> <mia> <commercial>
//!                                   fail unless the last SCCountUnreadMailPacket (sent at
//!                                   spawn, so it includes offline mail) has these counters
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
//! rawcs <opcode hex> [<body hex>]    send any client packet (`SELF` / `PEER` = 3-byte object
//!                                    ids, `SELFID` / `PEERID` = u32 character ids,
//!                                    `GUILD` = u32 guild id, `ITEM(<template>)` = stack id,
//!                                    `STR(<text>)` = a length-prefixed string)
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
//! expectchat <type> "<text>" [secs] fail unless SCChatMessage has that type and text
//! expecttradecancel <reason> <caused 0|1> [secs]
//!                                   fail unless SCTradeCanceled has that reason / causedByMe
//! expectbuff <buff> [self|peer|player:NAME] [secs]
//!                                   fail unless SCBuffCreated for that buff / owner arrives
//! expectbuffremoved <buff> [self|peer|player:NAME] [secs]
//!                                   fail unless SCBuffRemoved for a previously announced
//!                                   SCBuffCreated of that buff / owner arrives
//! expecthpdrop <self|peer|player:NAME> [secs]  fail unless that unit's HP falls below its
//!                                   value at the command start (the last SCUnitPointsPacket;
//!                                   fails as "hp unknown" without one, so `gm heal` it first:
//!                                   the server sends none at spawn). Regeneration can refill
//!                                   a small hit before the next packet and mask it
//! expectnohpdrop <self|peer|player:NAME> [secs]  fail if that unit's HP falls in the window
//!                                   (same baseline and the same regeneration caveat)
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

use anyhow::{bail, Context};

use super::{interaction_skill_list, music, summon_mate, BotConfig, Runner, ScriptInput};
use crate::game::{GameClient, UnitInfo};

mod expectations;
mod packet_views;
mod runner;

use self::runner::run_lines;

#[cfg(test)]
use self::expectations::{chat_type_and_text, expect_hp_drop, trade_cancel_reason};
#[cfg(test)]
use self::packet_views::*;
#[cfg(test)]
use crate::game::Packet;

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
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

#[cfg(test)]
mod tests;
