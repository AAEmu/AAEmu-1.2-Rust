//! "Quest Details Map Display" range: the client's given-quest map markers are produced by
//! `X2::GameClient::ClientUnit::UpdateQuestMark` for every unit the server streams
//! (`D:/aa-re/client12/out-x2game/x2game.dll.c:693168` `FUN_39359c60`, event `0x34` at
//! `FUN_39359e90`). 1.2 has no client-side distance option, so the range is widened by
//! streaming safely quest-relevant NPCs further than the ordinary 2-region neighbourhood.
//!
//! The 3.5 option semantics (`option_map_given_quest_distance`, slot * 100 m, 4.0 =
//! unlimited; 10.x `sub_396433C0`) are reproduced as a per-character range. Quest objectives
//! of accepted quests are already map-wide (`X2Map:AddNotifyQuestInfo` reads the static quest
//! data, `FUN_392b6ac0`); this module only adds quest givers / reporters.
//!
//! The range is not persisted server-side: the client stores the option value and the injected
//! driver sends `/questmap <m|0>` after entering the world and when the option is saved.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::server::GameServer;
use crate::world::visibility;
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

/// How often a tracked player is refreshed while moving.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Movement that forces an immediate refresh even inside [`REFRESH_INTERVAL`].
const MOVE_EPSILON_SQ: f32 = 64.0; // 8 m
/// Hard cap of extra marker NPCs streamed per character. Keeps the Unlimited option bounded
/// (the 1.2 world has about two thousand quest giver placements).
pub const MAX_EXTRA_NPCS: usize = 256;
/// Placements prepared per refresh; the rest follow on later ticks.
const MAX_SPAWNERS_PER_REFRESH: usize = 192;
/// NPC creations per refresh across all tracked players (one quest-map tick).
pub const SPAWN_BUDGET_PER_TICK: i64 = 32;

/// The four option steps. Values match the client option item (0..3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    #[default]
    Meters100,
    Meters200,
    Meters300,
    Unlimited,
}

impl Range {
    /// `option_map_given_quest_distance` slot: 0 = 100 m, 1 = 200 m, 2 = 300 m, 3 = unlimited.
    pub fn from_slot(slot: u32) -> Option<Self> {
        match slot {
            0 => Some(Self::Meters100),
            1 => Some(Self::Meters200),
            2 => Some(Self::Meters300),
            3 => Some(Self::Unlimited),
            _ => None,
        }
    }

    /// The `/questmap` argument: metres, or 0 for unlimited.
    pub fn from_meters(meters: u32) -> Option<Self> {
        match meters {
            100 => Some(Self::Meters100),
            200 => Some(Self::Meters200),
            300 => Some(Self::Meters300),
            0 => Some(Self::Unlimited),
            _ => None,
        }
    }

    pub fn slot(self) -> u32 {
        match self {
            Self::Meters100 => 0,
            Self::Meters200 => 1,
            Self::Meters300 => 2,
            Self::Unlimited => 3,
        }
    }

    pub fn meters(self) -> u32 {
        match self {
            Self::Meters100 => 100,
            Self::Meters200 => 200,
            Self::Meters300 => 300,
            Self::Unlimited => 0,
        }
    }

    /// The extra (beyond the ordinary region neighbourhood) radius; infinite for Unlimited.
    fn radius(self) -> f32 {
        match self {
            Self::Meters100 => 100.0,
            Self::Meters200 => 200.0,
            Self::Meters300 => 300.0,
            Self::Unlimited => f32::INFINITY,
        }
    }

    fn is_default(self) -> bool {
        self == Self::Meters100
    }
}

#[derive(Debug)]
struct PlayerState {
    range: Range,
    /// NPCs streamed only because of this option (never naturally visible).
    extra: HashMap<u32, NpcRef>,
    last_refresh: Instant,
    last_pos: (f32, f32),
}

/// Per-character quest-map range state (`server.systems.quests.quest_map`).
#[derive(Debug, Default)]
pub struct State {
    players: Mutex<HashMap<u32, PlayerState>>,
    /// Serializes a snapshot + world work + commit with range changes. A refresh holds this,
    /// takes `players` only to snapshot / commit, releases it before any world call and never
    /// takes this lock from inside a world / spawner path.
    refreshing: Mutex<()>,
}

impl State {
    pub fn range_of(&self, character_id: u32) -> Range {
        self.players
            .lock()
            .expect("quest map state poisoned")
            .get(&character_id)
            .map_or(Range::Meters100, |p| p.range)
    }

    pub fn tracked_count(&self) -> usize {
        self.players.lock().expect("quest map state poisoned").len()
    }

    /// Drops a character without sending packets (it is leaving the world).
    pub fn forget(&self, character_id: u32) {
        self.players
            .lock()
            .expect("quest map state poisoned")
            .remove(&character_id);
    }

    /// Obj ids of the extra marker NPCs currently streamed by any tracked player. The spawn
    /// activation sweep keeps their placements warm instead of reclaiming and rebuilding them
    /// every retention period.
    pub fn streamed_npc_ids(&self) -> HashSet<u32> {
        self.players
            .lock()
            .expect("quest map state poisoned")
            .values()
            .flat_map(|p| p.extra.keys().copied())
            .collect()
    }

    /// Test guard against the 2026-11-11 lock cycle: a world call must never run while the
    /// state mutex is held. `try_lock` re-entered from the owning thread fails, so a caller
    /// that kept the guard across world work trips this.
    #[cfg(test)]
    fn assert_unlocked(&self) {
        assert!(
            self.players.try_lock().is_ok(),
            "quest map state lock held across a world call"
        );
    }

    /// Forgets `npc` for every player that streamed it only through this option and sends
    /// those players the unit removal. The ordinary removal broadcast reaches only the NPC's
    /// region neighbourhood; a far quest-map viewer would otherwise keep a ghost unit / marker.
    pub fn forget_npc(&self, server: &GameServer, npc: &NpcRef) {
        let mut targets: Vec<u32> = Vec::new();
        {
            let mut players = self.players.lock().expect("quest map state poisoned");
            for (&character_id, ps) in players.iter_mut() {
                if ps
                    .extra
                    .get(&npc.obj_id)
                    .is_some_and(|tracked| Arc::ptr_eq(tracked, npc))
                {
                    ps.extra.remove(&npc.obj_id);
                    targets.push(character_id);
                }
            }
        }
        if targets.is_empty() {
            return;
        }
        let obj = WorldObject::Npc(npc.clone());
        let Some(world) = server.world.world_of(&obj) else {
            return;
        };
        for character_id in targets {
            if let Some(player) = server.world.character_by_id(character_id) {
                visibility::remove_visible_object(&world, &obj, &player);
            }
        }
    }
}

fn new_player_state(range: Range) -> PlayerState {
    PlayerState {
        range,
        extra: HashMap::new(),
        last_refresh: Instant::now() - REFRESH_INTERVAL * 2,
        last_pos: (f32::NAN, f32::NAN),
    }
}

/// `/questmap <100|200|300|0>`: changes the range and applies it immediately.
pub fn set_range(server: &GameServer, player: &PlayerRef, range: Range) -> Range {
    let state = &server.systems.quests.quest_map;
    let _serial = state.refreshing.lock().expect("quest map state poisoned");
    let previous = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        let entry = players
            .entry(player.character_id)
            .or_insert_with(|| new_player_state(range));
        let previous = entry.range;
        entry.range = range;
        entry.last_refresh = Instant::now() - REFRESH_INTERVAL * 2;
        previous
    };
    refresh_player_locked(server, player, SPAWN_BUDGET_PER_TICK);
    if range.is_default() {
        // Nothing can be extra at 100 m; the removal pass above has run.
        state
            .players
            .lock()
            .expect("quest map state poisoned")
            .remove(&player.character_id);
    }
    previous
}

/// One quest-map tick: refresh every player that selected a wider range, drop stale state.
pub fn tick(server: &GameServer) {
    let present: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    let state = &server.systems.quests.quest_map;
    let mut budget = SPAWN_BUDGET_PER_TICK;
    let tracked: Vec<PlayerRef> = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        players.retain(|id, _| present.contains(id));
        let ids: Vec<u32> = players.keys().copied().collect();
        drop(players);
        ids.iter()
            .filter_map(|id| server.world.character_by_id(*id))
            .collect()
    };
    for player in tracked {
        let spent = refresh_player(server, &player, budget);
        budget -= spent;
        if budget <= 0 {
            break;
        }
    }
}

/// Refreshes one tracked player (tick / [`set_range`] entry point).
fn refresh_player(server: &GameServer, player: &PlayerRef, budget: i64) -> i64 {
    let state = &server.systems.quests.quest_map;
    let _serial = state.refreshing.lock().expect("quest map state poisoned");
    refresh_player_locked(server, player, budget)
}

/// Applies the player's range and returns the NPC creation budget spent. The caller holds
/// [`State::refreshing`] and must not hold the `players` mutex: it is taken only to snapshot
/// and to commit, never across a world call (2026-11-11 lock cycle).
fn refresh_player_locked(server: &GameServer, player: &PlayerRef, budget: i64) -> i64 {
    let state = &server.systems.quests.quest_map;
    let (range, last_refresh, last_pos, extra) = {
        let players = state.players.lock().expect("quest map state poisoned");
        let Some(ps) = players.get(&player.character_id) else {
            return 0;
        };
        (ps.range, ps.last_refresh, ps.last_pos, ps.extra.clone())
    };
    #[cfg(test)]
    state.assert_unlocked();

    let now = Instant::now();
    let Some((inst, prx, pry)) = player.region() else {
        return 0;
    };
    let Some(world) = server.world.world(inst) else {
        return 0;
    };
    let world_object = WorldObject::from(player);
    let pos = world.world_pos_rot(&world_object);
    let forced = last_refresh.elapsed() >= REFRESH_INTERVAL
        || (pos.x - last_pos.0).powi(2) + (pos.y - last_pos.1).powi(2) > MOVE_EPSILON_SQ;
    if !forced && !range.is_default() {
        return 0;
    }

    let radius = range.radius();
    let radius2 = if radius.is_finite() {
        radius * radius
    } else {
        f32::INFINITY
    };
    let natural: HashSet<(i32, i32)> = world.neighbors((prx, pry)).into_iter().collect();
    let query = MarkerQuery {
        center: (pos.x, pos.y),
        radius,
        radius2,
    };
    let mut set = CandidateSet {
        natural,
        candidates: HashMap::new(),
        spent: 0,
    };
    if !range.is_default() {
        #[cfg(test)]
        state.assert_unlocked();
        if let Some(manager) =
            crate::systems::npcs::spawn_manager::manager_of(server, &world_object, Some(inst))
        {
            collect_from_spawners(server, &world, &manager, &query, budget, &mut set);
        }
    }

    // Removals first: an NPC that left the range, despawned or became naturally visible.
    #[cfg(test)]
    state.assert_unlocked();
    let mut dead: Vec<u32> = Vec::new();
    let mut removals: Vec<NpcRef> = Vec::new();
    for (id, stored) in extra.iter() {
        let current = world.object(*id).and_then(|o| o.as_npc().cloned());
        let same = current.as_ref().is_some_and(|npc| Arc::ptr_eq(npc, stored));
        if !same {
            // The unit the client still holds is gone (deleted, or its id reused). Do not hide
            // a replacement that already became naturally visible under the reused id.
            let replacement_visible = current.as_ref().is_some_and(|npc| {
                world
                    .region_of(&WorldObject::Npc(npc.clone()))
                    .is_some_and(|r| set.natural.contains(&r))
            });
            if !replacement_visible {
                removals.push(stored.clone());
            }
            dead.push(*id);
            continue;
        }
        let npc = current.expect("checked above");
        let obj = WorldObject::Npc(npc.clone());
        let is_natural = world
            .region_of(&obj)
            .is_some_and(|r| set.natural.contains(&r));
        if is_natural {
            // The regular region add/remove path owns it from now on.
            dead.push(*id);
            continue;
        }
        let np = world.world_pos_rot(&obj);
        let d2 = (np.x - pos.x).powi(2) + (np.y - pos.y).powi(2);
        if d2 > radius2 || !npc.is_visible() {
            removals.push(npc);
            dead.push(*id);
        }
    }
    for npc in removals {
        visibility::remove_visible_object(&world, &WorldObject::Npc(npc), player);
    }

    // Adds: nearest first, bounded by the option cap.
    let mut added: Vec<(u32, NpcRef)> = Vec::new();
    if !range.is_default() {
        #[cfg(test)]
        state.assert_unlocked();
        let mut ordered: Vec<(f32, NpcRef)> = set.candidates.into_values().collect();
        ordered.sort_by(|a, b| a.0.total_cmp(&b.0));
        let ctx = server.view_ctx();
        for (_, npc) in ordered {
            if extra.len() + added.len() >= MAX_EXTRA_NPCS {
                break;
            }
            let id = npc.obj_id;
            if extra.contains_key(&id) {
                continue;
            }
            let obj = WorldObject::Npc(npc.clone());
            if world
                .region_of(&obj)
                .is_some_and(|r| set.natural.contains(&r))
            {
                continue;
            }
            visibility::add_visible_object(&world, &obj, player, &ctx);
            added.push((id, npc));
        }
    }

    // Commit briefly. `refreshing` keeps range changes out, so only a `forget` (leave-world)
    // can have removed the entry; a changed set is merged by Arc identity.
    let committed = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        match players.get_mut(&player.character_id) {
            Some(ps) if ps.range == range => {
                for id in &dead {
                    let tracked = extra.get(id);
                    if ps
                        .extra
                        .get(id)
                        .is_some_and(|n| tracked.is_some_and(|t| Arc::ptr_eq(n, t)))
                    {
                        ps.extra.remove(id);
                    }
                }
                for (id, npc) in &added {
                    if ps.extra.len() >= MAX_EXTRA_NPCS {
                        break;
                    }
                    ps.extra.entry(*id).or_insert_with(|| npc.clone());
                }
                ps.last_refresh = now;
                ps.last_pos = (pos.x, pos.y);
                true
            }
            _ => false,
        }
    };
    if !committed {
        // The player left the world or changed range while this refresh ran: nothing may stay
        // streamed without being tracked, it would never be removed.
        for (_, npc) in added {
            visibility::remove_visible_object(&world, &WorldObject::Npc(npc), player);
        }
    }
    set.spent
}

/// The player-centred query of the placement collector.
struct MarkerQuery {
    center: (f32, f32),
    radius: f32,
    radius2: f32,
}

/// Candidate NPCs and the state shared while collecting them.
struct CandidateSet {
    natural: HashSet<(i32, i32)>,
    candidates: HashMap<u32, (f32, NpcRef)>,
    spent: i64,
}

/// Prepares ordinary quest-giver placements around the player and collects their live NPCs.
/// This is the only marker source: every production quest giver / reporter is created by an
/// ordinary spawner placement, so the placement index already yields the same live NPCs a
/// world-wide `all_npcs()` scan would, without scanning tens of thousands of NPCs per player.
fn collect_from_spawners(
    server: &GameServer,
    world: &WorldInstance,
    manager: &crate::systems::npcs::spawn_manager::SpawnManager,
    query: &MarkerQuery,
    budget: i64,
    set: &mut CandidateSet,
) {
    #[cfg(test)]
    server.systems.quests.quest_map.assert_unlocked();
    let residents = server.systems.npcs.spawns.resident_npcs(server);
    let npcs = &server.game_data.systems.npcs.npc;
    for spawner in manager.quest_giver_spawners_near(
        server,
        query.center.0,
        query.center.1,
        query.radius,
        MAX_SPAWNERS_PER_REFRESH,
    ) {
        if set.spent < budget {
            let want = (budget - set.spent).clamp(0, 4);
            if want > 0 {
                let created = spawner.update_warm(server, true, want);
                set.spent += created as i64;
            }
        }
        for npc in spawner.spawned_npcs().into_values().flatten() {
            let template_id = npc.data().unit.template_id;
            let Some(template) = npcs.template(template_id) else {
                continue;
            };
            if !residents.is_quest_map_candidate(template) {
                continue;
            }
            if npc.data().unit.is_dead() || !npc.is_visible() {
                continue;
            }
            let obj = WorldObject::Npc(npc.clone());
            if obj.region().map(|(instance, _, _)| instance) != Some(world.id) {
                continue;
            }
            if world
                .region_of(&obj)
                .is_some_and(|r| set.natural.contains(&r))
            {
                continue;
            }
            let p = world.world_pos_rot(&obj);
            let d2 = (p.x - query.center.0).powi(2) + (p.y - query.center.1).powi(2);
            if d2 <= query.radius2 {
                set.candidates.entry(npc.obj_id).or_insert((d2, npc));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aaemu_commons::PacketStream;

    use super::*;
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::opcodes::sc;
    use crate::systems::npcs::doodad::spawner::{NpcSpawnerDef, SpawnerBase};
    use crate::systems::npcs::npc::data::NpcTemplate;
    use crate::systems::npcs::npc::spawner_data::{
        NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate,
    };
    use crate::systems::npcs::spawn_manager::{backend::TestBackend, SpawnManager};
    use crate::systems::quests::data::QuestData;
    use crate::systems::quests::model::kind;
    use crate::systems::quests::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
    use crate::test_support::*;
    use crate::world::{DoodadRef, Entity, NpcRef, WorldInstance};

    const QUEST_NPC: u32 = 500;
    const OTHER_NPC: u32 = 600;
    const GIVER_SPAWNER: u32 = 10;
    const OTHER_SPAWNER: u32 = 20;

    /// Creates NPCs for the ordinary spawner path without the DB-backed manager.
    struct Backend;

    impl TestBackend for Backend {
        fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
            let id = if obj_id == 0 {
                server.ids.object.get_next_id().ok()?
            } else {
                obj_id
            };
            let mut n = Npc::new(template_id);
            n.unit.level = 5;
            n.unit.hp = 100;
            n.unit.max_hp = 100;
            Some(Entity::new(id, n))
        }

        fn doodad_create(
            &self,
            _server: &GameServer,
            _world: &Arc<WorldInstance>,
            _obj_id: u32,
            _template_id: u32,
        ) -> Option<DoodadRef> {
            None
        }
    }

    fn add_quest_act(q: &mut QuestData, id: u32, component_kind: u8, act: ActKind) {
        q.add_template(QuestTemplate {
            id,
            ..QuestTemplate::default()
        });
        q.add_component(QuestComponentTemplate {
            id,
            quest_id: id,
            kind_id: component_kind,
            ..QuestComponentTemplate::default()
        });
        q.add_act(id, id, id, 0, act);
    }

    fn data() -> GameData {
        let mut gd = GameData::empty();
        for id in [QUEST_NPC, OTHER_NPC] {
            gd.systems.npcs.npc.templates.insert(
                id,
                NpcTemplate {
                    id,
                    ..NpcTemplate::default()
                },
            );
        }
        add_quest_act(
            &mut gd.systems.quests,
            100,
            kind::START,
            ActKind::ConAcceptNpc { npc_id: QUEST_NPC },
        );
        gd.systems.quests.finish();
        gd
    }

    fn npc(template_id: u32, x: f32, y: f32) -> Npc {
        let mut n = Npc::new(template_id);
        n.unit.level = 5;
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.transform.set_xyz(x, y, 100.0);
        n
    }

    fn spawner_template(id: u32) -> NpcSpawnerTemplate {
        NpcSpawnerTemplate {
            id,
            name: format!("t{id}"),
            max_population: 1,
            spawn_delay_min: 10.0,
            spawn_delay_max: 10.0,
            test_radius_npc: 3.0,
            ..NpcSpawnerTemplate::default()
        }
    }

    fn spawner_member(id: u32, spawner_id: u32, npc_id: u32) -> NpcSpawnerNpc {
        NpcSpawnerNpc {
            id,
            npc_spawner_template_id: spawner_id,
            member_id: npc_id,
            member_type: "Npc".into(),
            weight: 1.0,
            unit_id: 0,
        }
    }

    fn npc_def(unit_id: u32, x: f32, y: f32) -> NpcSpawnerDef {
        let base = SpawnerBase {
            unit_id,
            position: crate::data::world_templates::WorldSpawnPosition {
                x,
                y,
                z: 100.0,
                ..Default::default()
            },
            ..SpawnerBase::default()
        };
        NpcSpawnerDef {
            base,
            count: 1,
            npc_spawner_ids: Vec::new(),
        }
    }

    fn server() -> Arc<GameServer> {
        let mut gd = data();
        gd.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
            vec![
                spawner_template(GIVER_SPAWNER),
                spawner_template(OTHER_SPAWNER),
            ],
            vec![
                spawner_member(100, GIVER_SPAWNER, QUEST_NPC),
                spawner_member(200, OTHER_SPAWNER, OTHER_NPC),
            ],
        )
        .expect("spawner data");
        let server = Arc::new(test_server_with(gd));
        server.set_self();
        *server
            .systems
            .npcs
            .spawns
            .test_backend
            .lock()
            .expect("test backend") = Some(Arc::new(Backend));
        server
    }

    fn manager(server: &Arc<GameServer>) -> Arc<SpawnManager> {
        server
            .systems
            .npcs
            .spawns
            .get_or_create(&server.world.world(0).expect("main_world"))
    }

    fn ids_packet(ids: &[u32]) -> (u16, Vec<u8>) {
        let mut s = PacketStream::new();
        s.write_u16(ids.len() as u16);
        for &id in ids {
            s.write_bc(id);
        }
        (sc::SCUnitsRemovedPacket, s.into_bytes())
    }

    #[test]
    fn option_slots_match_the_3_5_slider() {
        assert_eq!(Range::from_slot(0), Some(Range::Meters100));
        assert_eq!(Range::from_slot(1), Some(Range::Meters200));
        assert_eq!(Range::from_slot(2), Some(Range::Meters300));
        assert_eq!(Range::from_slot(3), Some(Range::Unlimited));
        assert_eq!(Range::from_slot(4), None);
        assert_eq!(Range::from_meters(0), Some(Range::Unlimited));
        assert_eq!(Range::from_meters(100), Some(Range::Meters100));
        assert_eq!(Range::from_meters(150), None);
        assert_eq!(Range::Meters300.slot(), 2);
        assert_eq!(Range::Unlimited.meters(), 0);
    }

    #[tokio::test]
    async fn range_streams_far_quest_givers_and_removes_them_again() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        m.add_npc_spawner(&server, npc_def(OTHER_NPC, 1000.0, 1270.0))
            .expect("other placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        let world = server.world.world(0).expect("main_world");

        // 250 m away: outside the ordinary 2-region neighbourhood.
        assert!(client.drain().is_empty(), "nothing is streamed yet");

        // 300 m: only the safe quest giver placement is prepared and streamed.
        set_range(&server, &player, Range::Meters300);
        let frames = client.drain();
        assert_eq!(
            frames.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket],
            "one quest giver"
        );
        let giver = world
            .all_npcs()
            .into_iter()
            .find(|n| n.data().unit.template_id == QUEST_NPC)
            .expect("prepared giver");
        let mut expected = npc(QUEST_NPC, 1000.0, 1250.0);
        let body = crate::world::unit_state::npc_unit_state(
            giver.obj_id,
            &mut expected,
            "",
            server.view_ctx().now_ms,
        )
        .body;
        assert_eq!(
            frames[0].1, body,
            "SCUnitState body is the NPC's own writer"
        );
        assert!(
            !world
                .all_npcs()
                .iter()
                .any(|n| n.data().unit.template_id == OTHER_NPC),
            "a non-quest placement is never prepared for a marker"
        );

        // Back to the default: the extra marker is removed.
        let giver_id = giver.obj_id;
        set_range(&server, &player, Range::Meters100);
        assert_eq!(client.drain(), vec![ids_packet(&[giver_id])]);

        // Unlimited reaches a second quest giver far across the instance.
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 0.0, 2000.0))
            .expect("far placement");
        client.drain();
        set_range(&server, &player, Range::Unlimited);
        let ops = client.drain_ops();
        assert_eq!(
            ops,
            vec![
                sc::SCUnitStatePacket,
                sc::SCUnitPointsPacket,
                sc::SCUnitStatePacket,
                sc::SCUnitPointsPacket
            ],
            "both quest givers, nearest first"
        );

        // Leaving the world drops the state without sending anything.
        crate::systems::quests::on_leave_world(&server, player.character_id);
        assert_eq!(server.systems.quests.quest_map.tracked_count(), 0);
    }

    #[tokio::test]
    async fn default_range_does_not_stream_anything() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();

        set_range(&server, &player, Range::Meters100);
        assert!(client.drain().is_empty());
        assert!(
            server
                .world
                .world(0)
                .expect("main_world")
                .all_npcs()
                .is_empty(),
            "the default range prepares and streams nothing"
        );
        assert_eq!(
            server
                .systems
                .quests
                .quest_map
                .range_of(player.character_id),
            Range::Meters100
        );
    }

    /// The 2026-11-11 lock cycle: a refresh used to hold `players` while it called into the
    /// world (spawner `update_warm`, NPC data locks, visibility packets). The world paths of a
    /// normal refresh assert the state mutex is free, so holding it across them fails here.
    #[tokio::test]
    async fn refresh_runs_world_work_without_the_state_lock() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();

        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
        set_range(&server, &player, Range::Meters100);
        assert_eq!(server.systems.quests.quest_map.tracked_count(), 0);
    }

    #[tokio::test]
    async fn despawned_extra_npc_sends_the_unit_removal_to_the_far_player() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );

        let world = server.world.world(0).expect("main_world");
        let npc = world
            .all_npcs()
            .into_iter()
            .find(|n| n.data().unit.template_id == QUEST_NPC)
            .expect("streamed giver");
        let obj_id = npc.obj_id;
        let spawner = server
            .systems
            .npcs
            .spawns
            .npc_spawner_of(obj_id)
            .expect("spawner link");

        // The player is several regions away: the ordinary deletion broadcast never reaches it.
        spawner.despawn(&server, &npc);
        assert_eq!(
            client.drain(),
            vec![ids_packet(&[obj_id])],
            "the far viewer must not keep a ghost marker"
        );
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    #[tokio::test]
    async fn dying_extra_npc_sends_the_unit_removal_to_the_far_player() {
        let server = server();
        // The NPC death path reaches `after_death` through the skills -> npcs bridge.
        crate::systems::npcs::install_hooks(&server);
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );

        let world = server.world.world(0).expect("main_world");
        let npc = world
            .all_npcs()
            .into_iter()
            .find(|n| n.data().unit.template_id == QUEST_NPC)
            .expect("streamed giver");
        let obj_id = npc.obj_id;
        crate::systems::npcs::npc::death::do_die(&server, &npc, None, 0);
        let frames = client.drain();
        assert!(
            frames.contains(&ids_packet(&[obj_id])),
            "a dying marker is removed for the far viewer: {frames:?}"
        );
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    /// Markers come from the ordinary quest-giver placement index, not from a world-wide NPC
    /// scan: an ad-hoc NPC with a quest-giver template is not a marker source.
    #[tokio::test]
    async fn markers_come_from_placements_not_a_world_npc_scan() {
        let server = server();
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        let world = server.world.world(0).expect("main_world");
        let ctx = server.view_ctx();
        world.spawn(Entity::new(0x500, npc(QUEST_NPC, 1000.0, 1250.0)), &ctx);
        client.drain();

        set_range(&server, &player, Range::Meters300);
        assert!(
            client.drain().is_empty(),
            "a directly spawned quest giver is not a placement marker"
        );
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }
}
