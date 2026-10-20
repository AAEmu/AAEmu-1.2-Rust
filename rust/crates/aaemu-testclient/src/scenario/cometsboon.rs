//! `cometsboon`: Auramancy "Comet's Boon" (skill 18222, plot 363, `plot_only`).
//!
//! The plot's event 2904 runs `InteractionEffect` 2666 (`SummonDoodad`, doodad 5017) with a
//! position target. Doodad 5017's START phase runs `DoodadFuncClout` 1100, whose area trigger
//! applies buff 3771 (MoveSpeedMul, raid relation, 3.5 s) to the caster inside its box. The bug:
//! the position was dropped, so the doodad never spawned and the buff was never applied.
//!
//! Setup uses GM chat commands (`/level`, so the account needs access level 100; the first
//! character of a fresh database gets it). The bot swaps its second ability slot to Auramancy,
//! levels up, learns the skill and casts it on itself, then watches for `SCDoodadCreatedPacket`
//! (template 5017) and `SCBuffCreatedPacket` / `SCBuffRemovedPacket` for buff 3771.

use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{ensure, Context};

use super::questitem::{chat_text, gm_command};
use super::{Runner, QUIET};
use crate::game::{GameClient, Packet};

/// Auramancy "Comet's Boon" (plot 363, plot_only).
const SKILL: u32 = 18222;
/// Summoned doodad of the plot's `InteractionEffect` 2666.
const DOODAD: u32 = 5017;
/// The area trigger's buff (`MoveSpeedMul`).
const BUFF: u32 = 3771;
/// Auramancy (`AbilityType`); the bot's second slot starts as `None` (11).
const AURAMANCY: u8 = 4;
const ABILITY_NONE: u8 = 11;
/// `/level`: enough skill points and ability level for the skill (ability level 5).
const LEVEL: u8 = 50;
/// The buff lasts 3.5 s; the whole cast and removal window is a little longer.
const WATCH: Duration = Duration::from_secs(12);

/// `(owner obj id, buff index, buff id)` of an `SCBuffCreatedPacket`.
fn buff_created(p: &Packet) -> Option<(u32, u32, u32)> {
    if !p.is_sc(sc::SCBuffCreatedPacket) {
        return None;
    }
    let mut s = p.stream();
    let kind = s.read_u8().ok()?;
    s.read_bc().ok()?; // caster obj id
    match kind {
        // SkillCaster::Item
        2 => {
            s.read_u64().ok()?;
            s.read_u32().ok()?;
            s.read_u8().ok()?;
            s.read_u32().ok()?;
        }
        // SkillCaster::Mount
        3 => {
            s.read_u32().ok()?;
        }
        _ => {}
    }
    s.read_u32().ok()?; // caster id
    let owner = s.read_bc().ok()?;
    let index = s.read_u32().ok()?;
    let buff_id = s.read_u32().ok()?;
    Some((owner, index, buff_id))
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.cometsboon.gm");
    let reply = gm_command(game, "quest list").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        !texts.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100",
        game.account_id
    );
    ensure!(!texts.is_empty(), "no reply to the GM command /quest list");
    gm_command(game, "revive").await?;
    r.end(s, "GM commands accepted");

    let s = r.begin("game.cometsboon.ability");
    game.swap_ability(ABILITY_NONE, AURAMANCY).await?;
    let swapped = game
        .wait_for("SCAbilitySwappedPacket", |p| {
            p.is_sc(sc::SCAbilitySwappedPacket)
        })
        .await?;
    let mut ss = swapped.stream();
    let obj = ss.read_bc().unwrap_or(0);
    let old = ss.read_u8().unwrap_or(255);
    let new = ss.read_u8().unwrap_or(255);
    ensure!(
        obj == game.state.self_obj && old == ABILITY_NONE && new == AURAMANCY,
        "ability swap answered with obj {obj} {old}->{new}"
    );
    r.end(s, format!("{old} -> {new}"));

    let s = r.begin("game.cometsboon.level");
    gm_command(game, &format!("level {LEVEL}")).await?;
    game.drain(QUIET, Duration::from_secs(4)).await?;
    r.end(s, format!("level {LEVEL}"));

    let s = r.begin("game.cometsboon.learn");
    game.learn_skill(SKILL).await?;
    game.wait_for("SCSkillLearnedPacket", |p| {
        p.is_sc(sc::SCSkillLearnedPacket) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await?;
    game.drain(QUIET, Duration::from_secs(2)).await?;
    r.end(s, format!("skill {SKILL}"));

    // ---- the cast ----
    let s = r.begin("game.cometsboon.cast");
    let caster = game.state.self_obj;
    // A `plot_only` skill answers with plot packets, not `SCSkillStarted`: the effects below are
    // the evidence.
    game.start_skill(SKILL, caster).await?;
    r.end(s, format!("skill {SKILL} on obj {caster}"));

    // ---- doodad 5017 + buff 3771 ----
    let s = r.begin("game.cometsboon.effects");
    let started = Instant::now();
    let mut doodad_at = None;
    let mut created = None;
    let mut removed_at = None;
    while started.elapsed() < WATCH {
        if game.state.doodads.values().any(|d| d.template_id == DOODAD) && doodad_at.is_none() {
            doodad_at = Some(started.elapsed());
        }
        let Some(p) = game.recv_within(Duration::from_millis(250)).await? else {
            continue;
        };
        if let Some((owner, index, buff_id)) = buff_created(&p) {
            println!(
                "[cometsboon] +{:>5} ms SCBuffCreated owner {owner} index {index} buff {buff_id}",
                started.elapsed().as_millis()
            );
            if owner == caster && buff_id == BUFF {
                created = Some((index, started.elapsed()));
            }
        } else if p.is_sc(sc::SCBuffRemovedPacket) {
            let mut rs = p.stream();
            let obj = rs.read_bc()?;
            let index = rs.read_u32()?;
            if let Some((created_index, _)) = created {
                if obj == caster && index == created_index {
                    println!(
                        "[cometsboon] +{:>5} ms SCBuffRemoved obj {obj} index {index}",
                        started.elapsed().as_millis()
                    );
                    removed_at = Some(started.elapsed());
                }
            }
        }
        if doodad_at.is_some() && created.is_some() && removed_at.is_some() {
            break;
        }
    }
    let doodad_at = doodad_at.context("doodad 5017 was not announced after the cast")?;
    let (index, created_at) =
        created.context("buff 3771 was not created on the caster after the cast")?;
    let removed_at = removed_at.context("buff 3771 was not removed on the caster")?;
    ensure!(
        removed_at > created_at,
        "buff 3771 removed before it was created"
    );
    r.end(
        s,
        format!(
            "doodad {DOODAD} at +{} ms; buff {BUFF} (index {index}) at +{} ms, removed at +{} ms",
            doodad_at.as_millis(),
            created_at.as_millis(),
            removed_at.as_millis()
        ),
    );
    Ok(())
}
