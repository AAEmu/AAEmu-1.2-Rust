//! Reproducible AI tick load bench on the real game data.
//!
//! Builds a world with [`NPCS`] AI-backed NPCs and [`PLAYERS`] players around them (the load of
//! the 2026-11-11 cloud run: ~1500 active candidates) and reports the tick-time distribution and
//! the profile step breakdown. Gated like the other real-data tests:
//!
//! ```sh
//! AAEMU_COMPACT_DB=D:/aaemu-live-r7-ai-perf/rs-game/Data/compact.sqlite3 \
//!   cargo test -p aaemu-game --profile live tick_cost_with_1500_active_npcs \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Run it in the `live` profile: a debug build spends its time in unoptimized geometry / hashing
//! code and its absolute numbers do not compare with the server.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::{Connection, OpenFlags};

use crate::data::client_files::ClientFileManager;
use crate::data::{FactionData, FormulaManager, GameData, Localization};
use crate::models::npc::Npc;
use crate::systems::{DataLoadCtx, SystemsData};
use crate::test_support::*;
use crate::world::Entity;

use super::framework::AiKind;
use super::manager::tick_all;
use super::profile;

/// NPCs around the players (the cloud run's candidate plateau).
const NPCS: usize = 1500;
/// Players spread over the NPC block.
const PLAYERS: usize = 50;
/// A peaceful ambient template (aggression off, sight range scale 1.5, fov 1.0): the tick cost
/// of the idle behaviors dominates the cloud log, not combat.
const AMBIENT_TEMPLATE: u32 = 1203;
/// Side of the square the NPCs and players are scattered over, m.
const AREA: f32 = 190.0;
/// Measured ticks after the spawn initialization settled.
const TICKS: usize = 50;
/// Ticks the spawn initialization (`Spawning` -> `RunCommandSet` -> idle) gets.
const SETTLE_TICKS: usize = 25;

/// Deterministic scatter over `0..=AREA` (xorshift; no RNG dependency).
fn scatter(seed: &mut u64) -> f32 {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *seed = x;
    (x % 10_000) as f32 / 10_000.0 * AREA
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1) - 1;
    sorted[rank.min(sorted.len() - 1)]
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only) and the live profile"]
async fn tick_cost_with_1500_active_npcs() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let app = Path::new(&db)
        .parent()
        .and_then(Path::parent)
        .expect("AAEMU_COMPACT_DB must be <app>/Data/compact.sqlite3")
        .to_path_buf();
    std::env::set_var("AAEMU_AI_PROFILE", "1");
    // The phase-2 + phase-3 tables the AI reads (no client files needed: the world is the test
    // server's synthetic one).
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("compact.sqlite3");
    let localization = Localization::load(&conn, "en_us").expect("localization");
    let files = ClientFileManager::from_sources(&[], &app);
    let systems = SystemsData::load(&DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    })
    .expect("systems data");
    let mut data = GameData::empty();
    data.factions = FactionData::load(&conn, &localization).expect("factions");
    data.formulas = FormulaManager::load(&conn).expect("formulas");
    data.systems = systems;
    let server = Arc::new(test_server_with(data));
    server.set_self();

    let world = server.world.world(0).expect("instance 0");
    let ctx = server.view_ctx();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;

    // Players first: every NPC spawn then shows to the players around it like the live server.
    let mut clients = Vec::new();
    let mut players = Vec::new();
    for i in 0..PLAYERS {
        let (x, y) = (scatter(&mut seed), scatter(&mut seed));
        let client = TestClient::new(&server, 0x100 + i as u32, 1);
        let character = test_character(0x100 + i as u32, &format!("Bench{i}"), x, y);
        let player = spawn_player(&server, &client, character, 0x100 + i as u32);
        clients.push(client);
        players.push(player);
    }

    let mut npcs = Vec::new();
    for i in 0..NPCS {
        let (x, y) = (scatter(&mut seed), scatter(&mut seed));
        let mut n = Npc::new(AMBIENT_TEMPLATE);
        n.unit.hp = 1_000;
        n.unit.max_hp = 1_000;
        n.unit.transform.set_xyz(x, y, 100.0);
        n.unit.transform.zone_id = TEST_ZONE;
        let npc = Entity::new(0x10_0000 + i as u32, n);
        server.systems.ai.ais.add_ai(&npc, AiKind::HoldPosition);
        world.spawn(&npc, &ctx);
        super::hooks::spawn_positioned(&server, &npc);
        npcs.push(npc);
    }

    // Let the spawn state machine settle (the manager's initialization grace is 20 ticks), then
    // measure steady-state ticks.
    for _ in 0..SETTLE_TICKS {
        tick_all(&server, 100);
    }
    let candidates = server.systems.ai.ais.tick_stats().candidates;
    let before = profile::snapshot();
    let mut samples = Vec::with_capacity(TICKS);
    for _ in 0..TICKS {
        let started = Instant::now();
        let diag = tick_all(&server, 100);
        samples.push(started.elapsed().as_micros());
        assert_eq!(diag.candidates, candidates, "the load must stay stable");
    }
    let after = profile::snapshot();
    let window = profile::delta(&after, &before);

    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let mean = samples.iter().sum::<u128>() / samples.len() as u128;
    eprintln!(
        "bench: {NPCS} NPCs, {PLAYERS} players, candidates {candidates}, ticks {TICKS}; \
         tick p50 {:.1}ms p99 {:.1}ms max {:.1}ms mean {:.1}ms",
        percentile(&sorted, 50) as f64 / 1e3,
        percentile(&sorted, 99) as f64 / 1e3,
        sorted.last().copied().unwrap_or(0) as f64 / 1e3,
        mean as f64 / 1e3
    );
    let mut steps = String::new();
    for step in profile::Step::ALL {
        let i = step as usize;
        if window.step_calls[i] == 0 {
            continue;
        }
        steps.push_str(&format!(
            "{}={:.0}ms/{} ",
            profile::step_name(step),
            window.step_ns[i] as f64 / 1e6,
            window.step_calls[i]
        ));
    }
    eprintln!("bench steps (totals over {TICKS} ticks): {steps}");
    for (i, kind) in super::framework::BehaviorKind::ALL.iter().enumerate() {
        if window.behavior_calls[i] > 0 {
            eprintln!(
                "bench behavior {kind:?}: {:.0}ms/{}",
                window.behavior_ns[i] as f64 / 1e6,
                window.behavior_calls[i]
            );
        }
    }
    assert!(candidates >= NPCS, "the players must activate every NPC");
    drop((clients, players, npcs));
}
