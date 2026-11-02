//! `busy`: a bot that plays like a fully active MMO player, for capacity and memory tests of the
//! game server (`aaemu-testclient load --scenario busy --bots N --busy-secs 300`).
//!
//! Per bot (GM account needed: `/item add`, `/move`, `/spawn`, see `rust/LIVE-TESTING.md`):
//! 1. teleport to one of [`POINTS`] (bot `i` takes point `i % 8` plus a small offset, so there
//!    are crowds and spread), 2. summon a mate, 3. summon a boat (rowboat scroll; the sea points
//!    are the places where it can float), 4. spawn a few GM NPCs and doodads, 5. for
//!    `busy_secs` cast skills, move a few metres and chat while draining incoming packets,
//!    6. leave the world.
//!
//! Setup failures (mate, boat, spawns) and rejected casts are counted, never fatal; a closed
//! connection fails the bot.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{bail, Context};

use super::questitem::{chat_text, gm_command, stack_id};
use super::{own_skill_response, skill_response_result, summon_mate, BotConfig, Runner};
use crate::game::GameClient;

/// Where the bots spread out: `(x, y, z)` taken from main-world NPC spawns (land) or from the
/// z 99.2 sea creature spawns south of the Nuian start (water, for the boat).
const POINTS: [(f32, f32, f32); 8] = [
    (15562.0, 15354.0, 128.0), // Nuian start (quest giver 3597)
    (11041.0, 15622.0, 246.0), // Soulreath (questitem scenario)
    (12590.0, 15407.0, 156.0), // stablehand 1890
    (12718.0, 15999.0, 230.0), // inland north-west
    (11068.0, 13001.0, 165.0), // inland mid
    (11015.0, 10891.0, 149.0), // inland south
    (15458.0, 10366.0, 100.5), // sea (water level 100)
    (15331.0, 10494.0, 100.5), // sea, second spot
];

/// NPC template spawned by `/spawn npc` (the dummy NPC of the command) and doodad template.
const SPAWN_NPC: u32 = 7512;
const SPAWN_DOODAD: u32 = 5622;
/// Buff applied to the bot itself now and then (Comet's Boon, see `cometsboon.rs`).
const SELF_BUFF: u32 = 3771;

/// Index of the bot: the trailing digits of its account name (`load17` -> 17), else a hash.
fn bot_index(user: &str) -> u64 {
    let digits: String = user
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().unwrap_or_else(|_| {
        user.bytes()
            .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
    })
}

/// A small deterministic pseudo random number from `seed` (offsets, jitter).
fn mix(seed: u64) -> u64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// `mix` mapped to `-range..=range`.
fn offset(seed: u64, range: f32) -> f32 {
    ((mix(seed) % 2001) as f32 / 1000.0 - 1.0) * range
}

#[derive(Default)]
struct Counts {
    casts: u32,
    self_actions: u32,
    responses: u32,
    rejected: u32,
    errors: BTreeMap<i16, u32>,
    moves: u32,
    chats: u32,
    packets: u64,
    /// Target (name, template id, position) of the last cast and the first rejection code.
    last_target: String,
    first_reject: Option<(i64, String)>,
}

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    let idx = bot_index(&cfg.user);

    // ---- GM access ----
    let s = r.begin("game.busy.gm");
    let reply = gm_command(game, "quest list").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    if texts.iter().any(|t| t.contains("Insufficient privileges")) {
        bail!(
            "GM commands are refused: run the game server with Account:AccessLevelDefault=100 \
             or set accounts.access_level = 100 for {}",
            cfg.user
        );
    }
    gm_command(game, "revive").await?;
    r.end(s, "GM commands accepted");

    // ---- spread out ----
    let s = r.begin("game.busy.teleport");
    let point = (idx % POINTS.len() as u64) as usize;
    let (px, py, pz) = POINTS[point];
    let (x, y) = (
        px + offset(idx * 2 + 1, 25.0),
        py + offset(idx * 2 + 2, 25.0),
    );
    game.state.last_teleport = None;
    game.chat(0, &format!("/move {x} {y} {pz}")).await?;
    game.wait_for("SCTeleportUnitPacket", |p| {
        p.is_sc(sc::SCTeleportUnitPacket)
    })
    .await?;
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    game.move_to(at, 3000).await?;
    game.drain(Duration::from_millis(800), Duration::from_secs(4))
        .await?;
    r.end(
        s,
        format!(
            "point {point} at {at:?}, {} units, {} doodads in sight",
            game.state.units.len(),
            game.state.doodads.len()
        ),
    );

    // ---- pet ----
    let s = r.begin("game.busy.mate");
    let note = match setup_mate(game, cfg).await {
        Ok(note) => note,
        Err(e) if game.state.disconnected => return Err(e),
        Err(e) => format!("WARN no mate: {e:#}"),
    };
    r.end(s, note);

    // ---- boat ----
    let s = r.begin("game.busy.boat");
    let note = match setup_boat(game, cfg).await {
        Ok(note) => note,
        Err(e) if game.state.disconnected => return Err(e),
        Err(e) => format!("WARN no boat: {e:#}"),
    };
    r.end(s, note);

    // ---- more things in the world ----
    let s = r.begin("game.busy.spawn");
    let mut spawned = 0;
    for cmd in [
        format!("spawn npc {SPAWN_NPC}"),
        format!("spawn npc {SPAWN_NPC}"),
        format!("spawn doodad {SPAWN_DOODAD}"),
        format!("spawn doodad {SPAWN_DOODAD}"),
    ] {
        if cmd.starts_with("spawn doodad") && spawned == 2 {
            // `/spawn` drops things 3 m in front of the bot: step aside so the doodads do not
            // sit on the NPCs and block the line of sight of the casts.
            let p = game.state.self_pos;
            game.move_to((p.0 + 8.0, p.1, p.2), 4000).await?;
            game.drain(Duration::from_millis(300), Duration::from_secs(1))
                .await?;
        }
        let reply = gm_command(game, &cmd).await?;
        if reply
            .iter()
            .filter_map(chat_text)
            .all(|t| !t.to_lowercase().contains("error"))
        {
            spawned += 1;
        }
    }
    r.end(s, format!("{spawned}/4 GM spawns accepted"));

    // ---- activity loop ----
    let s = r.begin("game.busy.loop");
    let counts = activity(game, cfg, idx).await?;
    let errors: Vec<String> = counts
        .errors
        .iter()
        .map(|(code, n)| format!("{code}x{n}"))
        .collect();
    r.end(
        s,
        format!(
            "{}s: {} casts ({} answered, {} rejected), {} self actions, {} moves, {} chats, \
             {} packets, SCErrorMsg [{}], first reject {:?}",
            cfg.busy_secs,
            counts.casts,
            counts.responses,
            counts.rejected,
            counts.self_actions,
            counts.moves,
            counts.chats,
            counts.packets,
            errors.join(" "),
            counts.first_reject
        ),
    );

    // ---- leave ----
    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    let prep = game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    let logout_ms = prep.stream().read_i32().unwrap_or(0);
    if cfg.wait_leave_granted && logout_ms <= 10_000 {
        game.wait_sc(sc::SCLeaveWorldGrantedPacket).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

async fn setup_mate(game: &mut GameClient, cfg: &BotConfig) -> anyhow::Result<String> {
    let reply = gm_command(game, &format!("item add self {} 1", cfg.mate_item_template)).await?;
    let item = stack_id(&reply, cfg.mate_item_template).context("mate item was not added")?;
    let view = summon_mate(game, cfg, item, None).await?;
    Ok(format!(
        "mate obj {} tl {} hp {}",
        view.obj_id, view.tl_id, view.hp
    ))
}

async fn setup_boat(game: &mut GameClient, cfg: &BotConfig) -> anyhow::Result<String> {
    let reply = gm_command(game, &format!("item add self {} 1", cfg.busy_boat_item)).await?;
    let item = stack_id(&reply, cfg.busy_boat_item).context("boat item was not added")?;
    let slaves = |g: &GameClient| g.state.units.values().filter(|u| u.unit_type == 2).count();
    let before = slaves(game);
    game.use_item_skill(cfg.busy_boat_skill, item, cfg.busy_boat_item)
        .await?;
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let got = game
            .drain(Duration::from_millis(500), Duration::from_secs(1))
            .await?;
        if let Some(p) = got.iter().find(|p| p.is_sc(sc::SCErrorMsgPacket)) {
            bail!(
                "boat summon rejected with SCErrorMsg {}",
                p.stream().read_i16().unwrap_or(0)
            );
        }
        if slaves(game) > before {
            return Ok(format!("boat {} summoned", cfg.busy_boat_item));
        }
        if Instant::now() >= deadline {
            bail!("no slave appeared within 12 s of the summon skill");
        }
    }
}

async fn activity(game: &mut GameClient, cfg: &BotConfig, idx: u64) -> anyhow::Result<Counts> {
    let mut c = Counts::default();
    let started = Instant::now();
    let end = started + Duration::from_secs(cfg.busy_secs);
    let period = Duration::from_millis(1000 + (mix(idx) % 1000));
    let mut next_chat = started + Duration::from_secs(5 + mix(idx + 99) % 10);
    let mut tick = 0u64;
    while Instant::now() < end {
        tick += 1;
        let me = game.state.self_obj;
        let clock = 200_000 + started.elapsed().as_millis() as u32;
        match tick % 3 {
            // A skill on the nearest NPC (when one is close enough to walk up to).
            1 => {
                let here = game.state.self_pos;
                if let Some(npc) = cast_target(game).filter(|n| dist(n.pos, here) < 30.0) {
                    game.move_to_facing((npc.pos.0 + 1.2, npc.pos.1, npc.pos.2), npc.pos, clock)
                        .await?;
                    game.change_target(npc.obj_id).await?;
                    game.start_skill(cfg.skill_id, npc.obj_id).await?;
                    c.casts += 1;
                    c.last_target = format!("{} tpl {} at {:?}", npc.name, npc.id, npc.pos);
                }
            }
            // Self targeted: a buff every other time, a heal otherwise.
            2 => {
                game.change_target(me).await?;
                let text = if tick % 6 == 2 {
                    format!("/buff {SELF_BUFF} 1")
                } else {
                    "/heal self".to_string()
                };
                game.chat(0, &text).await?;
                c.self_actions += 1;
            }
            // Walk a few metres.
            _ => {
                let p = game.state.self_pos;
                let to = (
                    p.0 + offset(idx * 7919 + tick * 2, 4.0),
                    p.1 + offset(idx * 7919 + tick * 2 + 1, 4.0),
                    p.2,
                );
                game.move_to(to, clock).await?;
                c.moves += 1;
            }
        }
        if Instant::now() >= next_chat {
            game.chat(0, &format!("busy bot {} tick {tick}", cfg.user))
                .await?;
            c.chats += 1;
            next_chat = Instant::now() + Duration::from_secs(10);
        }
        let packets = game.drain(period, period).await?;
        c.packets += packets.len() as u64;
        for p in &packets {
            if own_skill_response(p, cfg.skill_id, me) {
                c.responses += 1;
                let result = skill_response_result(p).unwrap_or(0);
                if result != 0 {
                    c.rejected += 1;
                    if c.first_reject.is_none() {
                        c.first_reject = Some((i64::from(result), c.last_target.clone()));
                    }
                }
            } else if p.is_sc(sc::SCErrorMsgPacket) {
                *c.errors
                    .entry(p.stream().read_i16().unwrap_or(0))
                    .or_insert(0) += 1;
            }
        }
        if game.state.disconnected {
            bail!("disconnected during the activity loop");
        }
    }
    Ok(c)
}

/// The closest GM spawned dummy NPC (a safe target), else the closest NPC.
fn cast_target(game: &GameClient) -> Option<crate::game::UnitInfo> {
    let me = game.state.self_pos;
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 1 && u.id == SPAWN_NPC)
        .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
        .cloned()
        .or_else(|| game.nearest_unit(1))
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_index_reads_trailing_digits() {
        assert_eq!(bot_index("load17"), 17);
        assert_eq!(bot_index("busy0"), 0);
        assert_ne!(bot_index("abc"), bot_index("abd"));
    }

    #[test]
    fn offsets_stay_in_range() {
        for i in 0..200 {
            assert!(offset(i, 25.0).abs() <= 25.0);
        }
    }
}
