//! `plant`: place crops, trees and animals with `CSCreateDoodadPacket` like the 1.2 client does
//! and report what the server did with each (GM account needed, see `rust/LIVE-TESTING.md`).
//!
//! The bot adds the seed / sapling / baby animal with `/item add self <item> 2`, sends
//! `CSCreateDoodadPacket` for its doodad template about 2 m from itself, and then lists what the
//! client saw: new doodads of that template, `SCErrorMsg` codes and item tasks. Failed
//! placements do not fail the scenario, they are printed as a table; the server log and the
//! `doodads` table are checked by whoever runs it.
//!
//! `AAEMU_PLANT_POS="x y z"` teleports the bot first (a public farm, a house plot).

use std::time::Duration;

use aaemu_game::opcodes::sc;
use anyhow::ensure;

use super::questitem::{chat_text, gm_command, stack_id};
use super::{Runner, QUIET};
use crate::game::{GameClient, GameEvent};

/// `(label, doodad template, item template)`; `item_spawn_doodads` of the 1.2 data.
const PLACEMENTS: [(&str, u32, u32); 7] = [
    ("daffodil", 2274, 15673),
    ("potato", 2259, 15659),
    ("turmeric", 2756, 16269),
    ("wolfdog", 4867, 23702),
    ("elk", 4891, 23709),
    ("growth fruit", 4594, 23635),
    ("fir sapling", 367, 14899),
];

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.plant.gm");
    let reply = gm_command(game, "quest list").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        !texts.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100",
        game.account_id
    );
    gm_command(game, "revive").await?;
    r.end(s, "GM commands accepted");

    if let Ok(pos) = std::env::var("AAEMU_PLANT_POS") {
        let s = r.begin("game.plant.teleport");
        game.chat(0, &format!("/move {pos}")).await?;
        game.wait_for("SCTeleportUnitPacket", |p| {
            p.is_sc(sc::SCTeleportUnitPacket)
        })
        .await?;
        let at = game.state.self_pos;
        game.teleport_ended(at).await?;
        game.move_to(at, 5000).await?;
        game.drain(QUIET, Duration::from_secs(8)).await?;
        r.end(s, format!("at {at:?}"));
    }

    let repeat: u32 = std::env::var("AAEMU_PLANT_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let mut rows = Vec::new();
    for (i, (label, doodad, item)) in PLACEMENTS.into_iter().enumerate() {
        let s = r.begin(&format!("game.plant.{label}"));
        let reply = gm_command(game, &format!("item add self {item} {}", repeat + 1)).await?;
        let Some(stack) = stack_id(&reply, item) else {
            let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
            r.end(s, format!("no item {item} was added: {texts:?}"));
            rows.push(format!("{label}: item {item} not added"));
            continue;
        };
        let me = game.state.self_pos;
        let pos = (me.0 + 2.0 + i as f32 * 0.7, me.1 + 2.0, me.2);
        let events_before = game.state.events.len();
        let doodads_before: Vec<u32> = game.state.doodads.keys().copied().collect();
        for n in 0..repeat {
            let at = (pos.0, pos.1 + n as f32 * 0.5, pos.2);
            game.create_doodad(doodad, at, 0.0, 1.0, stack).await?;
            if repeat > 1 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        game.drain(Duration::from_millis(800), Duration::from_secs(4))
            .await?;
        let new: Vec<_> = game
            .state
            .doodads
            .values()
            .filter(|d| d.template_id == doodad && !doodads_before.contains(&d.obj_id))
            .map(|d| (d.obj_id, d.pos))
            .collect();
        let events: Vec<String> = game.state.events[events_before..]
            .iter()
            .map(|e| match e {
                GameEvent::ErrorMsg(code) => format!("SCErrorMsg {code}"),
                GameEvent::ItemTask(t) => format!(
                    "ItemTask type {} items {:?}",
                    t.action,
                    t.items
                        .iter()
                        .map(|i| (i.task, i.template, i.id))
                        .collect::<Vec<_>>()
                ),
            })
            .collect();
        let note = format!(
            "doodad {doodad} item {item} stack {stack} at {pos:?}: placed {repeat}, spawned {}; events {}",
            new.len(),
            events.iter().take(4).cloned().collect::<Vec<_>>().join("; ")
        );
        println!("[plant] {label}: {note}");
        rows.push(format!("{label}: spawned={}", !new.is_empty()));
        r.end(s, note);
    }
    println!("[plant] summary: {rows:?}");

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}
