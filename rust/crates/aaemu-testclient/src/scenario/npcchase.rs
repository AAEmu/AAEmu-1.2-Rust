//! `npc-chase`: measure how a hostile NPC chases a standing player on a slope.
//!
//! Reproduces the tester report of Browntail Harpies on the grassy hillside of Solzreed Peninsula
//! (main_world, around 14987, 14063, z 140) that take absurd detours (a ~100 m route for a 12 m
//! chase). The server sends debug chat lines to the chased player from `npc_find_path`
//! (`systems/ai/geodata/mod.rs`): `Aggro from <obj>, getting attack path in N/M steps` followed
//! by one `Path step -> <x, y, z>` line per step. The bot records them together with the NPC's
//! own `SCOneUnitMovementPacket`s and judges the chase:
//!
//! * every path of a chase shorter than 40 m has a polyline/straight length ratio <= 1.5;
//! * the NPC gets within its attack range of the player within 8 s of the player standing still
//!   (or is seen casting a skill);
//! * at most 3 re-paths while the player stands still.
//!
//! Setup uses GM chat commands (`/move`, `/height`, `/godmode true`, `/revive`), so the bot's
//! account needs access level 100 (see `rust/LIVE-TESTING.md` section 3). Each case teleports to a
//! spot a few metres downhill/uphill of an NPC spawn, snaps the bot's Z to the navmesh height
//! (`/height`), waits for natural aggro and provokes the NPC (a hit from the spot, then from
//! next to it) when it stays calm, then stands still for `npc_chase_stand_secs`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};
use serde::Serialize;

use super::questitem::chat_text;
use super::{BotConfig, Runner};
use crate::game::{GameClient, Packet};

type P3 = (f32, f32, f32);

/// A chase scenario: an NPC spawn and the spot the player stands on.
#[derive(Clone)]
struct Case {
    name: &'static str,
    /// NPC template (`npcs.id`).
    npc: u32,
    /// Spawn position of the NPC to provoke (`main_world/npc_spawns.json`).
    spawn: P3,
    /// Where the player stands.
    spot: P3,
    /// Replace the spot's Z with the navmesh height (`/height`); `false` keeps the exact Z.
    snap: bool,
}

/// Cases against `main_world/npc_spawns.json` (and the tester capture at the first one).
const CASES: [Case; 5] = [
    // The tester capture, with its exact Z: harpy <14987.004, 14063.533, 145.424> chasing a
    // player at <14984.287, 14075.668, 137.04>, 12 m away and 8 m downhill. The navmesh floor
    // at that spot is 140.89, so the player is 3.8 m below it.
    Case {
        name: "harpy-hill",
        npc: 14667, // Browntail Harpy
        spawn: (14991.051, 14062.297, 145.42401),
        spot: (14984.287, 14075.668, 137.04),
        snap: false,
    },
    // The same spot with the player standing on the navmesh floor.
    Case {
        name: "harpy-hill-floor",
        npc: 14667,
        spawn: (14991.051, 14062.297, 145.42401),
        spot: (14984.287, 14075.668, 140.89),
        snap: true,
    },
    // The same hill, the harpy at its top (z 151.7) and the player 14 m north and ~3 m lower.
    Case {
        name: "harpy-hill-top",
        npc: 14667,
        spawn: (14983.263, 14035.418, 151.67404),
        spot: (14983.0, 14050.0, 148.0),
        snap: true,
    },
    // The player above the harpy: 17 m south of the capture's harpy, ground rising to z ~150.
    Case {
        name: "harpy-uphill",
        npc: 14667,
        spawn: (14991.051, 14062.297, 145.42401),
        spot: (14985.0, 14046.0, 149.0),
        snap: true,
    },
    // A ground mob on a hillside 600 m west: a "Giant Rabbit" (z 117.6) with the ground falling
    // to z ~112.7 towards the west, the player 14 m downhill of it.
    Case {
        name: "rabbit-slope",
        npc: 3491,
        spawn: (14529.08, 14422.74, 117.61117),
        spot: (14515.5, 14419.2, 114.5),
        snap: true,
    },
];

/// Pass/fail limits of one case.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Highest accepted path length ratio for chases shorter than [`SHORT_CHASE`].
    max_ratio: f32,
    /// Seconds until the NPC is in range (or seen attacking) after the player stands still.
    reach_secs: f32,
    /// Horizontal metres that count as "in attack range".
    attack_range: f32,
    /// Re-paths allowed while the player stands still.
    max_repaths: usize,
}

/// Chases (straight first-to-last step distance) below this length are held to the ratio limit.
const SHORT_CHASE: f32 = 40.0;
/// Seconds of natural aggro awaited before the NPC is provoked.
const SETTLE: Duration = Duration::from_secs(4);
/// NPCs within these many metres of the recorded spawn are candidates (they drift).
const SPAWN_RADIUS: f32 = 40.0;
/// The NPC counts as at home within this distance of the recorded spawn.
const HOME_RADIUS: f32 = 6.0;

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    let limits = Limits {
        max_ratio: cfg.npc_chase_max_ratio,
        reach_secs: cfg.npc_chase_reach_secs,
        attack_range: cfg.npc_chase_attack_range,
        max_repaths: cfg.npc_chase_max_repaths as usize,
    };
    let cases = select_cases(cfg)?;
    let mut tr = Trace::new();
    let mut clock = 5000u32;

    // ---- GM access ----
    let s = r.begin("game.npcchase.gm");
    let reply = gm(game, &mut tr, "height", Duration::from_secs(4)).await?;
    ensure!(
        !reply.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100, e.g. \
         `UPDATE <game db>.accounts SET access_level = 100 WHERE account_id = {};` (private MySQL \
         only), then run the scenario again",
        game.account_id,
        game.account_id
    );
    ensure!(!reply.is_empty(), "no reply to the GM command /height");
    // The harpies hit a level-1 bot for the whole observation: keep it alive.
    gm(game, &mut tr, "godmode true", Duration::from_millis(500)).await?;
    gm(game, &mut tr, "revive", Duration::from_millis(500)).await?;
    r.end(s, format!("GM commands accepted ({})", reply.join(" | ")));

    let mut reports = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let s = r.begin(&format!("game.npcchase.{}", case.name));
        if i > 0 {
            // Let the previous case's NPCs lose the player and return to their spawn.
            let far = [case.spot.0 - 700.0, case.spot.1, case.spot.2];
            teleport(game, &mut tr, far, &mut clock).await?;
            observe(game, &mut tr, Duration::from_secs(8)).await?;
        }
        match chase(game, &mut tr, case, cfg, limits, &mut clock).await {
            Ok(rep) => {
                let note = rep.note();
                let ok = rep.failures.is_empty();
                println!("[npc-chase] {}", rep.pretty_line());
                reports.push(rep);
                if ok {
                    r.end(s, note);
                } else {
                    r.fail(s, note);
                }
            }
            Err(e) => {
                r.fail(s, format!("{e:#}"));
                reports.push(Report::errored(case, format!("{e:#}")));
            }
        }
    }

    let summary = Summary {
        ok: reports.iter().all(|c| c.failures.is_empty()),
        limits: LimitsJson {
            max_ratio: limits.max_ratio,
            short_chase_m: SHORT_CHASE,
            reach_secs: limits.reach_secs,
            attack_range_m: limits.attack_range,
            max_repaths: limits.max_repaths,
        },
        cases: reports,
    };
    let json = serde_json::to_string_pretty(&summary)?;
    println!("[npc-chase] summary:\n{json}");
    if let Some(path) = &cfg.npc_chase_json {
        std::fs::write(path, &json).with_context(|| format!("writing {path}"))?;
        println!("[npc-chase] summary written to {path}");
    }

    // ---- leave ----
    let s = r.begin("game.leave");
    gm(game, &mut tr, "godmode false", Duration::from_millis(300))
        .await
        .ok();
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

/// The cases named by `npc_chase_case` ("all", a comma list, or "custom" with
/// `npc_chase_npc` / `npc_chase_pos`).
fn select_cases(cfg: &BotConfig) -> anyhow::Result<Vec<Case>> {
    let mut out = Vec::new();
    for name in cfg.npc_chase_case.split(',').map(str::trim) {
        match name {
            "" => {}
            "all" => out.extend(CASES.iter().cloned()),
            "custom" => {
                ensure!(
                    cfg.npc_chase_npc != 0,
                    "case 'custom' needs npc_chase_npc (--npc TEMPLATE)"
                );
                let [x, y, z] = cfg
                    .npc_chase_pos
                    .context("case 'custom' needs npc_chase_pos (--pos X,Y,Z)")?;
                out.push(Case {
                    name: "custom",
                    npc: cfg.npc_chase_npc,
                    // Any NPC of the template near the spot is provoked.
                    spawn: (x, y, z),
                    spot: (x, y, z),
                    snap: true,
                });
            }
            other => {
                let case = CASES.iter().find(|c| c.name == other).with_context(|| {
                    format!(
                        "unknown npc-chase case '{other}' (all, custom, {})",
                        CASES.map(|c| c.name).join(", ")
                    )
                })?;
                out.push(case.clone());
            }
        }
    }
    ensure!(!out.is_empty(), "no npc-chase case selected");
    Ok(out)
}

// ---------------------------------------------------------------- one case

async fn chase(
    game: &mut GameClient,
    tr: &mut Trace,
    case: &Case,
    cfg: &BotConfig,
    limits: Limits,
    clock: &mut u32,
) -> anyhow::Result<Report> {
    tr.reset_for_case(case.npc);
    tr.me = game.state.self_obj;
    tr.skill = cfg.skill_id;

    // Teleport to the spot and snap Z to the navmesh height.
    let [sx, sy, sz] = [case.spot.0, case.spot.1, case.spot.2];
    teleport(game, tr, [sx, sy, sz], clock).await?;
    observe(game, tr, Duration::from_secs(2)).await?;
    let reply = gm(game, tr, "height", Duration::from_secs(4)).await?;
    let nav = reply.iter().find_map(|t| nav_mesh_height(t));
    let z = match nav {
        Some(h) if case.snap && h > 0.0 && (h - sz).abs() < 30.0 => h,
        _ => sz,
    };
    if (z - sz).abs() > 0.05 {
        teleport(game, tr, [sx, sy, z], clock).await?;
    }
    let spot = game.state.self_pos;
    println!(
        "[npc-chase] {}: player spot {spot:?} (z hint {sz}, navmesh {:?}); {:?}",
        case.name, nav, reply
    );

    // Wait for the NPC around the spawn to be announced.
    let target = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(u) = pick_target(game, case) {
                break u;
            }
            ensure!(
                Instant::now() < deadline,
                "no NPC {} within {SPAWN_RADIUS} m of its spawn {:?} in sight of {spot:?}",
                case.npc,
                case.spawn
            );
            observe(game, tr, Duration::from_millis(500)).await?;
        }
    };
    // A previous case may have drawn the NPC away: let it settle at its spawn first.
    let mut target = target;
    let home_deadline = Instant::now() + Duration::from_secs(40);
    while dist3(target.pos, case.spawn) > HOME_RADIUS && Instant::now() < home_deadline {
        observe(game, tr, Duration::from_secs(1)).await?;
        if let Some(u) = pick_target(game, case) {
            target = u;
        }
    }
    let target_obj = target.obj_id;
    println!(
        "[npc-chase] {}: target npc obj {target_obj} (template {}) at {:?}, {:.1} m from the player",
        case.name,
        target.id,
        target.pos,
        dist3(target.pos, spot)
    );

    // Natural aggro first, then provoke: a hit from the spot (works in range only) and finally
    // one from next to the NPC before walking back to the spot.
    let mut stand_start = tr.now();
    let mut provoked = "none";
    observe(game, tr, SETTLE).await?;
    if !tr.engaged(target_obj) && cfg.npc_chase_provoke != "none" {
        provoked = "ranged";
        game.change_target(target_obj).await?;
        game.start_skill(cfg.skill_id, target_obj).await?;
        observe(game, tr, Duration::from_secs(3)).await?;
        println!(
            "[npc-chase] {}: ranged hit: own casts {:?}, errors {:?}, engaged {}",
            case.name,
            tr.own_casts,
            tr.errors,
            tr.engaged(target_obj)
        );
    }
    if !tr.engaged(target_obj) && cfg.npc_chase_provoke != "none" {
        provoked = "adjacent";
        // Harpies drift: stand next to the NPC's latest position and hit it, until a cast is
        // accepted (the server rejects out-of-range / out-of-angle casts with a SkillResult).
        for attempt in 0..4 {
            let npc_pos = game
                .state
                .units
                .get(&target_obj)
                .map_or(target.pos, |u| u.pos);
            teleport(game, tr, [npc_pos.0 + 1.0, npc_pos.1, npc_pos.2], clock).await?;
            let npc_pos = game.state.units.get(&target_obj).map_or(npc_pos, |u| u.pos);
            let here = (npc_pos.0 + 0.8, npc_pos.1, npc_pos.2);
            game.change_target(target_obj).await?;
            *clock += 100;
            game.move_to_facing(here, npc_pos, *clock).await?;
            let before = tr.own_casts.len();
            game.start_skill(cfg.skill_id, target_obj).await?;
            observe(game, tr, Duration::from_millis(900)).await?;
            let result = tr.own_casts.get(before).map(|c| c.1);
            println!(
                "[npc-chase] {}: adjacent hit #{attempt} from {here:?} on {npc_pos:?}: SkillResult {result:?}, errors {:?}, engaged {}",
                case.name,
                tr.errors,
                tr.engaged(target_obj)
            );
            if result == Some(0) || tr.engaged(target_obj) {
                break;
            }
        }
        teleport(game, tr, [spot.0, spot.1, spot.2], clock).await?;
        stand_start = tr.now();
    }
    if provoked == "none" && tr.engaged(target_obj) {
        // Natural aggro: the player has been standing since the teleport.
        stand_start = tr.now() - SETTLE.as_secs_f32();
    }
    println!(
        "[npc-chase] {}: provocation '{provoked}', standing still from t={stand_start:.1}s",
        case.name
    );

    // Stand still and record.
    let end = stand_start + cfg.npc_chase_stand_secs as f32;
    while tr.now() < end {
        observe(game, tr, Duration::from_millis(250)).await?;
    }

    let start_pos = tr.start_pos(target_obj).unwrap_or(target.pos);
    Ok(analyze(
        case,
        target_obj,
        spot,
        start_pos,
        stand_start,
        end,
        provoked,
        &tr.paths,
        tr.samples.get(&target_obj).map_or(&[][..], Vec::as_slice),
        &tr.attacks,
        tr.compressed,
        limits,
    ))
}

/// The NPC of the case's template nearest to its spawn (a wandering NPC may have moved off the
/// recorded point, a respawn changes the object id).
fn pick_target(game: &GameClient, case: &Case) -> Option<crate::game::UnitInfo> {
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 1 && u.id == case.npc)
        .map(|u| (dist3(u.pos, case.spawn), u))
        .filter(|(d, _)| *d <= SPAWN_RADIUS)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, u)| u.clone())
}

// ---------------------------------------------------------------- recording

/// What the bot saw, timestamped in seconds since the scenario's first call.
struct Trace {
    t0: Instant,
    template: u32,
    /// `Aggro from ...` / `Path step -> ...` chat lines, folded into paths.
    paths: Vec<PathRec>,
    /// Positions of every NPC of the case's template (`SCOneUnitMovementPacket`, unit state).
    samples: HashMap<u32, Vec<Sample>>,
    /// NPC casts `(t, caster obj)` of the case's template.
    attacks: Vec<(f32, u32)>,
    /// Number of compressed batches seen (their inner packets are not decoded).
    compressed: usize,
    /// The bot's own object id and attack skill, to tell its casts from the NPCs'.
    me: u32,
    skill: u32,
    /// The bot's own `SCSkillStarted/Fired` answers `(t, skill result)` and `SCErrorMsg` codes.
    own_casts: Vec<(f32, u8)>,
    errors: Vec<(f32, i16)>,
}

#[derive(Clone, Debug, Serialize)]
struct Sample {
    t: f32,
    pos: [f32; 3],
}

#[derive(Clone, Debug)]
struct PathRec {
    t: f32,
    npc: u32,
    declared_steps: usize,
    declared_nodes: usize,
    steps: Vec<P3>,
}

impl Trace {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            template: 0,
            paths: Vec::new(),
            samples: HashMap::new(),
            attacks: Vec::new(),
            compressed: 0,
            me: 0,
            skill: 0,
            own_casts: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn reset_for_case(&mut self, template: u32) {
        self.template = template;
        self.paths.clear();
        self.samples.clear();
        self.attacks.clear();
        self.compressed = 0;
        self.own_casts.clear();
        self.errors.clear();
    }

    fn now(&self) -> f32 {
        self.t0.elapsed().as_secs_f32()
    }

    fn engaged(&self, npc: u32) -> bool {
        self.paths.iter().any(|p| p.npc == npc)
    }

    fn start_pos(&self, npc: u32) -> Option<P3> {
        let s = self.samples.get(&npc)?.first()?;
        Some((s.pos[0], s.pos[1], s.pos[2]))
    }

    /// Folds one received packet into the trace.
    fn feed(&mut self, game: &GameClient, p: &Packet) {
        let t = self.now();
        if let Some(text) = chat_text(p) {
            if let Some((npc, n, m)) = parse_aggro(&text) {
                self.paths.push(PathRec {
                    t,
                    npc,
                    declared_steps: n,
                    declared_nodes: m,
                    steps: Vec::new(),
                });
            } else if let Some(step) = parse_step(&text) {
                if let Some(last) = self.paths.last_mut() {
                    last.steps.push(step);
                }
            }
            return;
        }
        if p.level == crate::game::LEVEL_COMPRESSED {
            self.compressed += 1;
            return;
        }
        let obj = if p.is_sc(sc::SCOneUnitMovementPacket) || p.is_sc(sc::SCUnitStatePacket) {
            p.stream().read_bc().ok()
        } else if p.is_sc(sc::SCErrorMsgPacket) {
            if let Ok(code) = p.stream().read_i16() {
                self.errors.push((t, code));
            }
            return;
        } else if p.is_sc(sc::SCSkillStartedPacket) || p.is_sc(sc::SCSkillFiredPacket) {
            if super::own_skill_response(p, self.skill, self.me) {
                self.own_casts
                    .push((t, super::skill_response_result(p).unwrap_or(255)));
                return;
            }
            let mut s = p.stream();
            let caster = (s.read_u32().is_ok() && s.read_u16().is_ok() && s.read_u8().is_ok())
                .then(|| s.read_bc().ok())
                .flatten();
            if let Some(c) = caster.filter(|c| self.watched(game, *c)) {
                self.attacks.push((t, c));
            }
            return;
        } else {
            None
        };
        let Some(obj) = obj else { return };
        if self.watched(game, obj) {
            if let Some(u) = game.state.units.get(&obj) {
                self.samples.entry(obj).or_default().push(Sample {
                    t,
                    pos: [u.pos.0, u.pos.1, u.pos.2],
                });
            }
        }
    }

    fn watched(&self, game: &GameClient, obj: u32) -> bool {
        game.state
            .units
            .get(&obj)
            .is_some_and(|u| u.unit_type == 1 && u.id == self.template)
    }
}

/// Receives packets for `dur`, feeding the trace.
async fn observe(game: &mut GameClient, tr: &mut Trace, dur: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(100)))
            .await?
        {
            tr.feed(game, &p);
        }
    }
}

/// `/<cmd>`; returns the chat lines received until the reply went quiet (at most `wait`, at
/// least until the first chat line plus 300 ms; the server answers in a tick or two).
async fn gm(
    game: &mut GameClient,
    tr: &mut Trace,
    cmd: &str,
    wait: Duration,
) -> anyhow::Result<Vec<String>> {
    game.chat(0, &format!("/{cmd}")).await?;
    let deadline = Instant::now() + wait;
    let mut lines = Vec::new();
    let mut got_at: Option<Instant> = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || got_at.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
            return Ok(lines);
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(100)))
            .await?
        {
            tr.feed(game, &p);
            if let Some(text) = chat_text(&p) {
                if parse_aggro(&text).is_none() && parse_step(&text).is_none() {
                    got_at.get_or_insert_with(Instant::now);
                    lines.push(text);
                }
            }
        }
    }
}

/// GM `/move` like the real client sees it: `SCTeleportUnit`, `CSTeleportEnded`, then the
/// server adopts the position from the bot's next movement packet.
async fn teleport(
    game: &mut GameClient,
    tr: &mut Trace,
    pos: [f32; 3],
    clock: &mut u32,
) -> anyhow::Result<()> {
    game.chat(0, &format!("/move {} {} {}", pos[0], pos[1], pos[2]))
        .await?;
    let deadline = Instant::now() + game.timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("timed out waiting for SCTeleportUnitPacket after /move");
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(100)))
            .await?
        {
            tr.feed(game, &p);
            if p.is_sc(sc::SCTeleportUnitPacket) {
                break;
            }
        }
    }
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    *clock += 1000;
    game.move_to(at, *clock).await?;
    observe(game, tr, Duration::from_millis(500)).await
}

// ---------------------------------------------------------------- parsing

/// `Aggro from <obj>, getting attack path in N/M steps`.
fn parse_aggro(text: &str) -> Option<(u32, usize, usize)> {
    let rest = text.trim().strip_prefix("Aggro from ")?;
    let (obj, rest) = rest.split_once(", getting attack path in ")?;
    let (counts, _) = rest.split_once(" steps")?;
    let (n, m) = counts.split_once('/')?;
    Some((
        obj.trim().parse().ok()?,
        n.trim().parse().ok()?,
        m.trim().parse().ok()?,
    ))
}

/// `Path step -> <x, y, z>`.
fn parse_step(text: &str) -> Option<P3> {
    let rest = text.trim().strip_prefix("Path step -> <")?;
    let rest = rest.strip_suffix('>')?;
    let mut it = rest.split(',').map(|v| v.trim().parse::<f32>());
    let (x, y, z) = (it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?);
    it.next().is_none().then_some((x, y, z))
}

/// `NavMeshHeight: <z>` of the `/height` reply.
fn nav_mesh_height(text: &str) -> Option<f32> {
    let rest = text.split("NavMeshHeight:").nth(1)?;
    let num: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+'))
        .collect();
    num.parse().ok()
}

// ---------------------------------------------------------------- analysis

fn dist3(a: P3, b: P3) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

fn dist2d(a: P3, b: P3) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

#[derive(Debug, Serialize)]
struct PathReport {
    /// Seconds since the player stood still (negative: before).
    t: f32,
    declared_steps: usize,
    declared_nodes: usize,
    listed_steps: usize,
    length: f32,
    straight: f32,
    ratio: f32,
    steps: Vec<[f32; 3]>,
}

#[derive(Debug, Serialize)]
struct Report {
    case: String,
    npc_template: u32,
    npc_obj: u32,
    player_pos: [f32; 3],
    npc_start: [f32; 3],
    provocation: String,
    stand_secs: f32,
    engaged: bool,
    /// Paths computed since the first aggro line of the case.
    paths_total: usize,
    /// Paths computed while the player stood still.
    paths_standing: usize,
    /// `paths_standing - 1`: the first path is the chase itself.
    repaths: usize,
    max_path_ratio: f32,
    paths: Vec<PathReport>,
    /// Seconds after the player stood still when the NPC was first within the attack range
    /// (horizontal), `null` when never.
    reached_range_secs: Option<f32>,
    /// Seconds after the player stood still when the NPC was first seen casting a skill.
    attacked_secs: Option<f32>,
    min_dist_to_player: f32,
    min_dist_to_player_2d: f32,
    max_dist_from_player: f32,
    max_dist_from_start: f32,
    /// Lowest and highest Z of the NPC (a flyer sent through a lower navmesh layer shows up
    /// far below the hillside).
    npc_z_range: [f32; 2],
    final_dist_to_player: f32,
    /// NPC ended within 3 m of its start after leaving it and out of range: it gave up and went
    /// back (leash / return).
    returned_to_start: bool,
    movement_samples: usize,
    compressed_batches: usize,
    failures: Vec<String>,
    /// `[t, x, y, z]` of the NPC while the player stood still (thinned to <= 120 points).
    track: Vec<[f32; 4]>,
}

#[derive(Debug, Serialize)]
struct LimitsJson {
    max_ratio: f32,
    short_chase_m: f32,
    reach_secs: f32,
    attack_range_m: f32,
    max_repaths: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    ok: bool,
    limits: LimitsJson,
    cases: Vec<Report>,
}

impl Report {
    fn errored(case: &Case, error: String) -> Self {
        Self {
            case: case.name.into(),
            npc_template: case.npc,
            npc_obj: 0,
            player_pos: [case.spot.0, case.spot.1, case.spot.2],
            npc_start: [case.spawn.0, case.spawn.1, case.spawn.2],
            provocation: String::new(),
            stand_secs: 0.0,
            engaged: false,
            paths_total: 0,
            paths_standing: 0,
            repaths: 0,
            max_path_ratio: 0.0,
            paths: Vec::new(),
            reached_range_secs: None,
            attacked_secs: None,
            min_dist_to_player: 0.0,
            min_dist_to_player_2d: 0.0,
            max_dist_from_player: 0.0,
            max_dist_from_start: 0.0,
            npc_z_range: [0.0, 0.0],
            final_dist_to_player: 0.0,
            returned_to_start: false,
            movement_samples: 0,
            compressed_batches: 0,
            failures: vec![error],
            track: Vec::new(),
        }
    }

    fn note(&self) -> String {
        if self.failures.is_empty() {
            self.pretty_line()
        } else {
            format!(
                "{} -- FAILED: {}",
                self.pretty_line(),
                self.failures.join("; ")
            )
        }
    }

    fn pretty_line(&self) -> String {
        format!(
            "{}: npc {} paths {} (standing {}, repaths {}), max ratio {:.2}, in range {}, \
             attacked {}, dist to player min {:.1} max {:.1} final {:.1}, max from start {:.1}, \
             returned {}, z {:?}, {} movement samples",
            self.case,
            self.npc_obj,
            self.paths_total,
            self.paths_standing,
            self.repaths,
            self.max_path_ratio,
            self.reached_range_secs
                .map_or("never".to_string(), |t| format!("after {t:.1}s")),
            self.attacked_secs
                .map_or("never".to_string(), |t| format!("after {t:.1}s")),
            self.min_dist_to_player,
            self.max_dist_from_player,
            self.final_dist_to_player,
            self.max_dist_from_start,
            self.returned_to_start,
            self.npc_z_range,
            self.movement_samples
        )
    }
}

fn polyline_length(points: &[P3]) -> f32 {
    points.windows(2).map(|w| dist3(w[0], w[1])).sum()
}

/// Turns a recording into a [`Report`] and judges it against `limits`.
#[allow(clippy::too_many_arguments)]
fn analyze(
    case: &Case,
    npc_obj: u32,
    player: P3,
    npc_start: P3,
    stand_start: f32,
    end: f32,
    provoked: &str,
    all_paths: &[PathRec],
    samples: &[Sample],
    attacks: &[(f32, u32)],
    compressed: usize,
    limits: Limits,
) -> Report {
    let mine: Vec<&PathRec> = all_paths.iter().filter(|p| p.npc == npc_obj).collect();
    let paths: Vec<PathReport> = mine
        .iter()
        .map(|p| {
            let length = polyline_length(&p.steps);
            let straight = match (p.steps.first(), p.steps.last()) {
                (Some(a), Some(b)) => dist3(*a, *b),
                _ => 0.0,
            };
            // A start on top of the goal is a trivially straight path.
            let ratio = if straight < 0.5 {
                1.0
            } else {
                length / straight
            };
            PathReport {
                t: round1(p.t - stand_start),
                declared_steps: p.declared_steps,
                declared_nodes: p.declared_nodes,
                listed_steps: p.steps.len(),
                length: round1(length),
                straight: round1(straight),
                ratio: (ratio * 100.0).round() / 100.0,
                steps: p.steps.iter().map(|s| [s.0, s.1, s.2]).collect(),
            }
        })
        .collect();
    let standing: Vec<&PathReport> = paths.iter().filter(|p| p.t >= -0.05).collect();
    let max_ratio = paths
        .iter()
        .filter(|p| p.straight < SHORT_CHASE)
        .map(|p| p.ratio)
        .fold(0.0, f32::max);

    let pos = |s: &Sample| (s.pos[0], s.pos[1], s.pos[2]);
    let mut min_d = f32::MAX;
    let mut min_d2 = f32::MAX;
    let mut max_d = 0.0f32;
    let mut max_start = 0.0f32;
    for s in samples {
        let p = pos(s);
        min_d = min_d.min(dist3(p, player));
        min_d2 = min_d2.min(dist2d(p, player));
        max_d = max_d.max(dist3(p, player));
        max_start = max_start.max(dist3(p, npc_start));
    }
    let last = samples.last().map(pos);
    let final_d = last.map_or(0.0, |p| dist3(p, player));
    let returned = last.is_some_and(|p| {
        dist3(p, npc_start) <= 3.0 && max_start >= 5.0 && dist2d(p, player) > limits.attack_range
    });

    // In range at the moment the player stood still counts as 0 s.
    let before = samples.iter().rev().find(|s| s.t <= stand_start).map(pos);
    let reached = if before.is_some_and(|p| dist2d(p, player) <= limits.attack_range) {
        Some(0.0)
    } else {
        samples
            .iter()
            .find(|s| s.t > stand_start && dist2d(pos(s), player) <= limits.attack_range)
            .map(|s| round1(s.t - stand_start))
    };
    let attacked = attacks
        .iter()
        .find(|(t, c)| *t >= stand_start && *c == npc_obj)
        .map(|(t, _)| round1(t - stand_start));

    let mut failures = Vec::new();
    if mine.is_empty() {
        failures.push("the NPC never computed an attack path (no 'Aggro from' line)".into());
    }
    for p in &paths {
        if p.straight < SHORT_CHASE && p.ratio > limits.max_ratio {
            failures.push(format!(
                "path at t={:.1}s is {:.1} m long for a {:.1} m chase (ratio {:.2} > {:.2})",
                p.t, p.length, p.straight, p.ratio, limits.max_ratio
            ));
        }
        if p.declared_steps != p.listed_steps {
            failures.push(format!(
                "path at t={:.1}s declares {} steps but {} were listed",
                p.t, p.declared_steps, p.listed_steps
            ));
        }
    }
    let first_in_range = [reached, attacked].into_iter().flatten().reduce(f32::min);
    match first_in_range {
        Some(t) if t <= limits.reach_secs => {}
        Some(t) => failures.push(format!(
            "NPC reached its attack range only after {t:.1}s (limit {:.1}s)",
            limits.reach_secs
        )),
        None => failures.push(format!(
            "NPC never got within {:.1} m of the player (closest {:.1} m horizontally)",
            limits.attack_range,
            if min_d2 == f32::MAX { f32::NAN } else { min_d2 }
        )),
    }
    let repaths = standing.len().saturating_sub(1);
    if repaths > limits.max_repaths {
        failures.push(format!(
            "{repaths} re-paths while the player stood still (limit {})",
            limits.max_repaths
        ));
    }

    // The NPC's way while the player stood still, thinned for the JSON.
    let window: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.t >= stand_start - 0.5 && s.t <= end + 0.5)
        .collect();
    let step = window.len().div_ceil(120).max(1);
    let track = window
        .iter()
        .step_by(step)
        .map(|s| [round1(s.t - stand_start), s.pos[0], s.pos[1], s.pos[2]])
        .collect();

    Report {
        case: case.name.into(),
        npc_template: case.npc,
        npc_obj,
        player_pos: [player.0, player.1, player.2],
        npc_start: [npc_start.0, npc_start.1, npc_start.2],
        provocation: provoked.into(),
        stand_secs: round1(end - stand_start),
        engaged: !mine.is_empty(),
        paths_total: paths.len(),
        paths_standing: standing.len(),
        repaths,
        max_path_ratio: max_ratio,
        paths,
        reached_range_secs: reached,
        attacked_secs: attacked,
        min_dist_to_player: if min_d == f32::MAX {
            0.0
        } else {
            round1(min_d)
        },
        min_dist_to_player_2d: if min_d2 == f32::MAX {
            0.0
        } else {
            round1(min_d2)
        },
        max_dist_from_player: round1(max_d),
        max_dist_from_start: round1(max_start),
        npc_z_range: [
            round1(samples.iter().map(|s| s.pos[2]).fold(f32::MAX, f32::min)),
            round1(samples.iter().map(|s| s.pos[2]).fold(f32::MIN, f32::max)),
        ],
        final_dist_to_player: round1(final_d),
        returned_to_start: returned,
        movement_samples: samples.len(),
        compressed_batches: compressed,
        failures,
        track,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        max_ratio: 1.5,
        reach_secs: 8.0,
        attack_range: 5.0,
        max_repaths: 3,
    };

    fn case() -> Case {
        CASES[0].clone()
    }

    fn path(t: f32, steps: &[P3]) -> PathRec {
        PathRec {
            t,
            npc: 7,
            declared_steps: steps.len(),
            declared_nodes: steps.len() + 3,
            steps: steps.to_vec(),
        }
    }

    fn sample(t: f32, x: f32) -> Sample {
        Sample {
            t,
            pos: [x, 0.0, 0.0],
        }
    }

    fn run(paths: &[PathRec], samples: &[Sample], attacks: &[(f32, u32)]) -> Report {
        analyze(
            &case(),
            7,
            (0.0, 0.0, 0.0),
            (12.0, 0.0, 0.0),
            0.0,
            15.0,
            "none",
            paths,
            samples,
            attacks,
            0,
            LIMITS,
        )
    }

    #[test]
    fn aggro_and_step_lines_parse() {
        assert_eq!(
            parse_aggro("Aggro from 4294, getting attack path in 4/9 steps"),
            Some((4294, 4, 9))
        );
        assert_eq!(parse_aggro("Path step -> <1, 2, 3>"), None);
        assert_eq!(
            parse_step("Path step -> <14987.004, 14063.533, 145.424>"),
            Some((14987.004, 14063.533, 145.424))
        );
        assert_eq!(parse_step("Path step -> <1, 2>"), None);
        assert_eq!(parse_step("hello"), None);
    }

    #[test]
    fn height_reply_yields_the_navmesh_height() {
        assert_eq!(
            nav_mesh_height("Bot Z-Pos: 137.04 - Floor: 136.9, NavMeshHeight: 137.5"),
            Some(137.5)
        );
        assert_eq!(nav_mesh_height("nothing"), None);
    }

    #[test]
    fn a_direct_chase_passes() {
        let rep = run(
            &[path(0.2, &[(12.0, 0.0, 0.0), (0.0, 0.0, 0.0)])],
            &[sample(-1.0, 12.0), sample(1.0, 8.0), sample(2.0, 4.0)],
            &[],
        );
        assert!(rep.failures.is_empty(), "{:?}", rep.failures);
        assert_eq!(rep.reached_range_secs, Some(2.0));
        assert_eq!(rep.repaths, 0);
    }

    #[test]
    fn the_tester_detour_fails_the_ratio() {
        // 14987,14063 -> 15075,14070 -> 15085,14074 -> 14984,14075: ~190 m for a 12 m chase.
        let rep = run(
            &[path(
                0.0,
                &[
                    (14987.004, 14063.533, 145.424),
                    (15075.675, 14070.383, 159.26),
                    (15085.528, 14074.576, 161.77),
                    (14984.287, 14075.668, 137.04),
                ],
            )],
            &[sample(0.5, 12.0)],
            &[],
        );
        assert!(rep.max_path_ratio > 10.0, "{}", rep.max_path_ratio);
        assert!(rep.failures.iter().any(|f| f.contains("ratio")));
        assert!(rep.failures.iter().any(|f| f.contains("never got within")));
    }

    #[test]
    fn late_arrival_and_repaths_fail() {
        let paths: Vec<PathRec> = (0..6)
            .map(|i| path(i as f32, &[(12.0, 0.0, 0.0), (0.0, 0.0, 0.0)]))
            .collect();
        let rep = run(&paths, &[sample(10.0, 3.0)], &[]);
        assert_eq!(rep.repaths, 5);
        assert!(rep.failures.iter().any(|f| f.contains("re-paths")));
        assert!(rep.failures.iter().any(|f| f.contains("only after 10.0s")));
    }

    #[test]
    fn an_attack_counts_as_arrival() {
        let rep = run(
            &[path(0.0, &[(12.0, 0.0, 0.0), (0.0, 0.0, 0.0)])],
            &[sample(1.0, 12.0)],
            &[(3.0, 7)],
        );
        assert_eq!(rep.attacked_secs, Some(3.0));
        assert!(rep.failures.is_empty(), "{:?}", rep.failures);
    }

    #[test]
    fn leashing_back_to_the_start_is_reported() {
        let rep = run(
            &[path(0.0, &[(12.0, 0.0, 0.0), (0.0, 0.0, 0.0)])],
            &[sample(0.0, 12.0), sample(1.0, 30.0), sample(2.0, 12.0)],
            &[],
        );
        assert!(rep.returned_to_start);
        assert!((rep.max_dist_from_start - 18.0).abs() < 0.2);
    }

    #[test]
    fn unknown_and_custom_cases_are_selected() {
        let mut cfg = test_cfg();
        assert_eq!(select_cases(&cfg).unwrap().len(), CASES.len());
        cfg.npc_chase_case = "rabbit-slope, harpy-hill".into();
        let names: Vec<_> = select_cases(&cfg).unwrap().iter().map(|c| c.name).collect();
        assert_eq!(names, ["rabbit-slope", "harpy-hill"]);
        cfg.npc_chase_case = "nope".into();
        assert!(select_cases(&cfg).is_err());
        cfg.npc_chase_case = "custom".into();
        assert!(select_cases(&cfg).is_err());
        cfg.npc_chase_npc = 14667;
        cfg.npc_chase_pos = Some([1.0, 2.0, 3.0]);
        let c = select_cases(&cfg).unwrap();
        assert_eq!((c[0].npc, c[0].spot), (14667, (1.0, 2.0, 3.0)));
    }

    fn test_cfg() -> BotConfig {
        serde_json::from_value(serde_json::json!({
            "login": "x", "user": "u", "password": "p", "character": "c"
        }))
        .unwrap()
    }
}
